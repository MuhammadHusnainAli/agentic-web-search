//! Authentication and per-client rate limiting.

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, header},
    middleware::Next,
    response::Response,
};
use governor::clock::{Clock, DefaultClock};

use crate::{
    error::AppError,
    metrics::Metrics,
    state::{ClientKey, SharedState},
};

/// Extract an API key from `X-API-Key` or `Authorization: Bearer <key>`.
fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim());
    }
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = auth.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

pub async fn auth_and_rate_limit(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let key = presented_key(req.headers());
    let client = match key.and_then(|k| state.api_keys.verify(k)) {
        Some(id) => id,
        None if state.cfg.require_auth => {
            Metrics::inc(&state.metrics.unauthorized);
            return Err(AppError::Unauthorized);
        }
        None => req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map_or(ClientKey::Anonymous, |ci| ClientKey::Ip(ci.0.ip())),
    };

    if let Err(not_until) = state.limiter.check_key(&client) {
        Metrics::inc(&state.metrics.rate_limited);
        let wait = not_until.wait_time_from(DefaultClock::default().now());
        return Err(AppError::RateLimited {
            retry_after_secs: wait.as_secs_f64().ceil() as u64,
        });
    }

    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn reads_both_header_styles() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static(" k1 "));
        assert_eq!(presented_key(&h), Some("k1"));

        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer k2"));
        assert_eq!(presented_key(&h), Some("k2"));

        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(presented_key(&h), None);
    }
}
