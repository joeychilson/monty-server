//! Authentication and rate-limiting middleware.
//!
//! Two layers run on every `/v1` route, outermost first:
//!
//! 1. [`authenticate`] resolves the request to a [`Caller`] — either a
//!    configured API token's principal, or (when no tokens are configured) the
//!    client IP. The caller is stashed in request extensions.
//! 2. [`rate_limit`] charges the caller's token bucket and rejects with
//!    `429 rate_limited` when it is empty.
//!
//! Handlers downstream read the [`Caller`] via an `Extension<Caller>` extractor
//! to scope sessions to their owner.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::config::{Auth, RateRule};
use crate::error::ApiError;
use crate::state::AppState;

/// The resolved identity behind a request.
#[derive(Clone, Debug)]
pub struct Caller {
    /// Stable identifier: an API token's name, or `ip:<addr>` when unauthenticated.
    /// Used as the rate-limit key and as the owner of any sessions created.
    pub id: String,
    /// Rate limit that applies to this caller.
    pub rate: RateRule,
}

/// Middleware: resolve the [`Caller`] and reject unauthenticated requests when
/// API tokens are configured.
pub async fn authenticate(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let caller = match &state.config.auth {
        Auth::Disabled => Caller {
            id: format!("ip:{}", client_ip(&request, addr)),
            rate: state.config.default_rate,
        },
        Auth::Tokens(tokens) => {
            let token = bearer_token(&request).ok_or(ApiError::Unauthorized)?;
            let principal = tokens.get(token).ok_or(ApiError::Unauthorized)?;
            Caller {
                id: principal.name.clone(),
                rate: principal.rate,
            }
        }
    };

    request.extensions_mut().insert(caller);
    Ok(next.run(request).await)
}

/// Middleware: charge the caller's rate-limit bucket. Must run after [`authenticate`].
pub async fn rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // `authenticate` always runs first and inserts the caller; its absence is a
    // wiring bug, not a client error.
    let caller = request
        .extensions()
        .get::<Caller>()
        .cloned()
        .ok_or(ApiError::Internal)?;

    match state.rate_limiter.check(&caller.id, caller.rate) {
        Ok(()) => Ok(next.run(request).await),
        Err(retry_after) => Err(ApiError::RateLimited { retry_after }),
    }
}

/// Extracts the bearer token from the `Authorization` header, if present and
/// well-formed. The `Bearer` scheme name is matched case-insensitively.
fn bearer_token(request: &Request) -> Option<&str> {
    let header = request.headers().get(axum::http::header::AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

/// Determines the client IP, preferring the first hop of `X-Forwarded-For`
/// (set by Railway and most reverse proxies) over the direct socket address.
fn client_ip(request: &Request, addr: SocketAddr) -> String {
    request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|ip| ip.trim().to_owned())
        .filter(|ip| !ip.is_empty())
        .unwrap_or_else(|| addr.ip().to_string())
}
