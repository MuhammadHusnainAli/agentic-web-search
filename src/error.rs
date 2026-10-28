//! Error type shared across the service and its HTTP mapping.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::model::EngineKind;

/// Failure fetching or parsing one SERP page.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum UpstreamError {
    #[error("{0} blocked the request (captcha / bot challenge)")]
    Blocked(EngineKind),
    #[error("{engine} returned HTTP {status}")]
    Status { engine: EngineKind, status: u16 },
    #[error("{engine} request timed out")]
    Timeout { engine: EngineKind },
    #[error("{engine} request failed: {message}")]
    Transport { engine: EngineKind, message: String },
    #[error("browser unavailable: {0}")]
    BrowserUnavailable(String),
    #[error("{engine} browser render failed: {message}")]
    Browser { engine: EngineKind, message: String },
}

impl UpstreamError {
    pub fn is_blocked(&self) -> bool {
        matches!(self, UpstreamError::Blocked(_))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EngineFailure {
    pub engine: EngineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    pub error: String,
    /// The engine served a captcha / bot challenge.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("missing or invalid API key")]
    Unauthorized,
    #[error("rate limit exceeded")]
    RateLimited { retry_after_secs: u64 },
    #[error("all search engines failed")]
    AllEnginesFailed {
        blocked: bool,
        failures: Vec<EngineFailure>,
    },
    #[error("request timed out")]
    Timeout,
    #[error("service overloaded, retry later")]
    Overloaded,
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a [EngineFailure]>,
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            AppError::AllEnginesFailed { .. } => StatusCode::BAD_GATEWAY,
            AppError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            AppError::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "bad_request",
            AppError::Unauthorized => "unauthorized",
            AppError::RateLimited { .. } => "rate_limited",
            AppError::AllEnginesFailed { blocked: true, .. } => "engine_blocked",
            AppError::AllEnginesFailed { .. } => "upstream_error",
            AppError::Timeout => "timeout",
            AppError::Overloaded => "overloaded",
            AppError::Internal(_) => "internal",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let details = match &self {
            AppError::AllEnginesFailed { failures, .. } => Some(failures.as_slice()),
            _ => None,
        };
        let message = match &self {
            AppError::AllEnginesFailed { blocked: true, .. } => {
                "all search engines failed; at least one blocked the request (try mode=browser, another engine, or configure PROXY_URL)".to_owned()
            }
            other => other.to_string(),
        };
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message,
                details,
            },
        };
        let mut resp = (self.status(), Json(body)).into_response();
        if let AppError::RateLimited { retry_after_secs } = self {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs.max(1)));
        }
        resp
    }
}
