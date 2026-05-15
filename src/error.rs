//! Protocol-level errors.
//!
//! These represent problems with the *request* — bad JSON, missing auth, rate
//! limits — not problems with the *Python code* a client submitted. Failures
//! that happen while running user code (exceptions, resource limits, syntax
//! errors) are normal `200 OK` outcomes; see [`crate::engine`].

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// An error returned to the client as a structured JSON body with an HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request body or parameters were malformed.
    #[error("{0}")]
    BadRequest(String),
    /// No valid bearer token was supplied.
    #[error("missing or invalid API token")]
    Unauthorized,
    /// The caller exceeded its rate limit.
    #[error("rate limit exceeded")]
    RateLimited {
        /// Seconds the client should wait before retrying.
        retry_after: u64,
    },
    /// The requested session does not exist or has expired.
    #[error("session not found")]
    SessionNotFound,
    /// The session is currently being resumed by another request.
    #[error("session is busy with another resume request")]
    SessionBusy,
    /// The server is at capacity (too many sessions or executions queued).
    #[error("{0}")]
    Unavailable(String),
    /// An unexpected internal failure.
    #[error("internal server error")]
    Internal,
}

impl ApiError {
    /// Stable machine-readable error code, safe to branch on from a client.
    fn code(&self) -> &'static str {
        match self {
            ApiError::BadRequest(_) => "bad_request",
            ApiError::Unauthorized => "unauthorized",
            ApiError::RateLimited { .. } => "rate_limited",
            ApiError::SessionNotFound => "session_not_found",
            ApiError::SessionBusy => "session_busy",
            ApiError::Unavailable(_) => "unavailable",
            ApiError::Internal => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            ApiError::SessionNotFound => StatusCode::NOT_FOUND,
            ApiError::SessionBusy => StatusCode::CONFLICT,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorBody {
            error: ErrorDetail {
                code: self.code(),
                message: self.to_string(),
            },
        });
        let mut response = (self.status(), body).into_response();

        // `Retry-After` lets well-behaved clients back off without guessing.
        if let ApiError::RateLimited { retry_after } = self
            && let Ok(value) = retry_after.to_string().parse()
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

/// Convenience alias for handler results.
pub type ApiResult<T> = Result<T, ApiError>;
