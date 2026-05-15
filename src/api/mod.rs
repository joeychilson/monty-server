//! HTTP surface: routing, shared handler plumbing, and the request/response DTOs.

pub mod compile;
pub mod dto;
pub mod meta;
pub mod run;
pub mod sessions;

use std::time::Duration;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Runs blocking monty work on the blocking thread pool, gated by the global
/// concurrency semaphore.
///
/// monty execution is synchronous and CPU-bound, so it must never run on an
/// async worker. The semaphore permit — acquired before the task is spawned and
/// held until it finishes — bounds how much CPU and memory in-flight executions
/// can consume at once; excess requests queue (and are ultimately bounded by
/// the request timeout layer).
///
/// # Errors
///
/// Returns [`ApiError::Internal`] if the runtime is shutting down or the
/// blocking task panics; otherwise returns whatever `work` returned.
pub async fn spawn_execution<F, T>(state: &AppState, work: F) -> ApiResult<T>
where
    F: FnOnce() -> ApiResult<T> + Send + 'static,
    T: Send + 'static,
{
    let permit = state
        .concurrency
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError::Internal)?;

    let handle = tokio::task::spawn_blocking(move || {
        // Hold the permit for exactly the lifetime of the blocking work.
        let _permit = permit;
        work()
    });

    match handle.await {
        Ok(result) => result,
        Err(join_error) => {
            tracing::error!(error = %join_error, "monty execution task panicked");
            Err(ApiError::Internal)
        }
    }
}

/// Builds the full application router, including middleware.
///
/// `/` and `/health` are public; everything under `/v1` is wrapped in the
/// authentication and rate-limiting layers.
pub fn build_router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/run", post(run::run))
        .route("/compile", post(compile::compile))
        .route("/info", get(meta::info))
        .route("/sessions", post(sessions::create))
        .route(
            "/sessions/{id}",
            get(sessions::status).delete(sessions::delete),
        )
        .route("/sessions/{id}/resume", post(sessions::resume))
        // `route_layer` so unmatched paths 404 without first demanding auth.
        // Added rate-limit first, auth second: the later layer is outermost, so
        // `authenticate` runs first and `rate_limit` sees the resolved caller.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::rate_limit,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ));

    let request_timeout = state.config.request_timeout;
    let max_body_bytes = state.config.max_body_bytes;
    let cors = cors_layer(&state.config.cors_origins);

    Router::new()
        .route("/", get(meta::root))
        .route("/health", get(meta::health))
        .nest("/v1", v1)
        // Innermost first: bound the body and the total request time, then CORS,
        // then turn panics into 500s, then trace every request/response.
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ))
        .layer(RequestBodyLimitLayer::new(max_body_bytes))
        .layer(cors)
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Builds the CORS layer. An empty origin list disables CORS entirely; a list
/// containing `*` allows any origin; otherwise only the listed origins.
fn cors_layer(origins: &[String]) -> CorsLayer {
    if origins.is_empty() {
        return CorsLayer::new();
    }
    if origins.iter().any(|o| o == "*") {
        return CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
    }

    let parsed = origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect::<Vec<axum::http::HeaderValue>>();
    CorsLayer::new()
        .allow_origin(parsed)
        .allow_methods(Any)
        .allow_headers(Any)
}

/// The interval at which the background reaper sweeps expired sessions and idle
/// rate-limit buckets.
pub const REAPER_INTERVAL: Duration = Duration::from_secs(30);
