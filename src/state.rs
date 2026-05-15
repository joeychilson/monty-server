//! Shared application state, cloned cheaply into every request handler.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::config::Config;
use crate::rate_limit::RateLimiter;
use crate::session::SessionStore;

/// State shared across all handlers. Every field is behind an `Arc`, so
/// `Clone` is a handful of atomic increments.
#[derive(Clone)]
pub struct AppState {
    /// Immutable server configuration.
    pub config: Arc<Config>,
    /// Live iterative-execution sessions.
    pub sessions: Arc<SessionStore>,
    /// Per-principal request rate limiter.
    pub rate_limiter: Arc<RateLimiter>,
    /// Bounds how many monty executions run at once; protects CPU and memory.
    pub concurrency: Arc<Semaphore>,
    /// Process start time, for uptime reporting.
    pub started_at: Instant,
}

impl AppState {
    /// Builds the shared state from a loaded [`Config`].
    #[must_use]
    pub fn new(config: Config) -> Self {
        let sessions = SessionStore::new(config.sessions.max_sessions);
        let concurrency = Semaphore::new(config.max_concurrency);
        AppState {
            config: Arc::new(config),
            sessions: Arc::new(sessions),
            rate_limiter: Arc::new(RateLimiter::new()),
            concurrency: Arc::new(concurrency),
            started_at: Instant::now(),
        }
    }
}
