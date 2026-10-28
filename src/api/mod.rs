//! HTTP API: routes, handlers and the middleware stack.

mod middleware;
pub mod models;

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use axum::{
    BoxError, Json, Router,
    error_handling::HandleErrorLayer,
    extract::{
        DefaultBodyLimit, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderName, HeaderValue, Request, StatusCode, header},
    middleware::from_fn_with_state,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer, load_shed::LoadShedLayer};
use tower_http::{
    CompressionLevel,
    catch_panic::CatchPanicLayer,
    compression::CompressionLayer,
    cors::CorsLayer,
    request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use self::models::{EngineInfo, SearchParams, SearchResponse};
use crate::{
    error::AppError,
    metrics::Metrics,
    model::{EngineKind, TimeRange},
    state::SharedState,
};

static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
static X_CACHE: HeaderName = HeaderName::from_static("x-cache");

/// Cheap monotonically increasing request ids (no UUID/RNG cost per request).
#[derive(Clone, Default)]
struct CounterRequestId;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl MakeRequestId for CounterRequestId {
    fn make_request_id<B>(&mut self, _: &Request<B>) -> Option<RequestId> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        HeaderValue::from_str(&format!("{id:x}")).ok().map(RequestId::new)
    }
}

pub fn router(state: SharedState) -> Router {
    let cfg = &state.cfg;

    let v1 = Router::new()
        .route("/search", get(search_get).post(search_post))
        .route("/engines", get(engines))
        .route_layer(from_fn_with_state(state.clone(), middleware::auth_and_rate_limit));

    let mut app = Router::new()
        .nest("/v1", v1)
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(16 * 1024));

    if cfg.compression {
        // Fastest level: default brotli quality costs ~7x throughput on 10 KB
        // responses for a few percent smaller bodies.
        app = app.layer(CompressionLayer::new().quality(CompressionLevel::Fastest));
    }
    if cfg.cors_allow_any {
        app = app.layer(CorsLayer::permissive());
    }

    app.layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::new(X_REQUEST_ID.clone(), CounterRequestId))
            .layer(TraceLayer::new_for_http())
            .layer(PropagateRequestIdLayer::new(X_REQUEST_ID.clone()))
            .layer(CatchPanicLayer::new())
            .layer(HandleErrorLayer::new(handle_overload))
            .layer(LoadShedLayer::new())
            .layer(ConcurrencyLimitLayer::new(cfg.max_concurrent_requests))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::GATEWAY_TIMEOUT,
                cfg.request_timeout,
            )),
    )
    .with_state(state)
}

async fn handle_overload(err: BoxError) -> Response {
    if err.is::<tower::load_shed::error::Overloaded>() {
        AppError::Overloaded.into_response()
    } else {
        AppError::Internal(err.to_string()).into_response()
    }
}

async fn search_get(
    State(state): State<SharedState>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Result<Response, AppError> {
    let Query(params) = params.map_err(|e| AppError::BadRequest(e.body_text()))?;
    run_search(state, params).await
}

async fn search_post(
    State(state): State<SharedState>,
    params: Result<Json<SearchParams>, JsonRejection>,
) -> Result<Response, AppError> {
    let Json(params) = params.map_err(|e| AppError::BadRequest(e.body_text()))?;
    run_search(state, params).await
}

async fn run_search(state: SharedState, params: SearchParams) -> Result<Response, AppError> {
    let started = Instant::now();
    Metrics::inc(&state.metrics.requests);
    let fresh = params.fresh.unwrap_or(false);
    let req = params.validate(&state.cfg, state.search.browser().is_enabled())?;
    let (out, cached) = state.search.search(&req, fresh).await?;

    let body = SearchResponse {
        query: &req.query,
        engine: out.engine,
        engines_tried: &out.engines_tried,
        engines_used: &out.engines_used,
        pages_requested: req.pages,
        pages_fetched: out.pages_fetched,
        total_results: out.total_results,
        cached,
        elapsed_ms: started.elapsed().as_millis() as u64,
        results: &out.results,
        errors: &out.errors,
        warnings: &out.warnings,
    };
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert(
        X_CACHE.clone(),
        HeaderValue::from_static(if cached { "HIT" } else { "MISS" }),
    );
    Ok(resp)
}

async fn engines(State(state): State<SharedState>) -> Json<Vec<EngineInfo>> {
    let browser = state.search.browser().is_enabled();
    Json(
        EngineKind::ALL
            .iter()
            .map(|&name| {
                let engine = state.search.engines().get(name);
                let requires_browser = engine.requires_js();
                EngineInfo {
                    name,
                    requires_browser,
                    available: browser || !requires_browser,
                    time_ranges: [TimeRange::Day, TimeRange::Week, TimeRange::Month, TimeRange::Year]
                        .into_iter()
                        .filter(|t| engine.supports_time_range(*t))
                        .collect(),
                }
            })
            .collect(),
    )
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

async fn ready(State(state): State<SharedState>) -> impl IntoResponse {
    let (entries, bytes) = state.search.cache_stats();
    Json(json!({
        "status": "ready",
        "version": env!("CARGO_PKG_VERSION"),
        "browser": state.search.browser().status(),
        "cache": { "entries": entries, "bytes": bytes },
    }))
}

async fn metrics(State(state): State<SharedState>) -> impl IntoResponse {
    let (entries, bytes) = state.search.cache_stats();
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(entries, bytes),
    )
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "not_found", "message": "no such route" } })),
    )
}
