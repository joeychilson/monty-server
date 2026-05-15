//! Unauthenticated metadata endpoints: service banner, health check, and a
//! configuration/limits summary.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::config::Auth;
use crate::state::AppState;

/// `GET /` — a small banner pointing at the real endpoints.
pub async fn root() -> Json<Value> {
    Json(json!({
        "service": "monty-server",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "HTTP API for the monty sandboxed Python interpreter",
        "endpoints": {
            "run": "POST /v1/run",
            "compile": "POST /v1/compile",
            "create_session": "POST /v1/sessions",
            "session_status": "GET /v1/sessions/{id}",
            "session_resume": "POST /v1/sessions/{id}/resume",
            "session_delete": "DELETE /v1/sessions/{id}",
            "info": "GET /v1/info",
            "health": "GET /health"
        }
    }))
}

/// `GET /health` — liveness probe for load balancers and Railway. Cheap and
/// dependency-free: if the process can answer, it is healthy.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// `GET /v1/info` — the server's effective limits and runtime state, so clients
/// can discover the ceilings they will be clamped to.
pub async fn info(State(state): State<AppState>) -> Json<Value> {
    let config = &state.config;
    let limits = &config.limits;

    let auth_mode = match &config.auth {
        Auth::Disabled => "disabled",
        Auth::Tokens(_) => "token",
    };

    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "auth": auth_mode,
        "max_concurrency": config.max_concurrency,
        "max_body_bytes": config.max_body_bytes,
        "limits": {
            "duration_ms": {
                "default": limits.default_duration.as_millis() as u64,
                "max": limits.max_duration.as_millis() as u64,
            },
            "memory_bytes": {
                "default": limits.default_memory_bytes,
                "max": limits.max_memory_bytes,
            },
            "allocations": {
                "default": limits.default_max_allocations,
                "max": limits.max_allocations,
            },
            "recursion_depth": {
                "default": limits.default_recursion_depth,
                "max": limits.max_recursion_depth,
            },
        },
        "sessions": {
            "live": state.sessions.len(),
            "max": config.sessions.max_sessions,
            "default_ttl_seconds": config.sessions.default_ttl.as_secs(),
            "max_ttl_seconds": config.sessions.max_ttl.as_secs(),
        }
    }))
}
