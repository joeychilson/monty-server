//! Server configuration, loaded once at startup from environment variables.
//!
//! Every knob has a safe default so the server runs with zero configuration,
//! but production deployments should at least set `MONTY_API_TOKENS`.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::num::ParseIntError;
use std::time::Duration;

use thiserror::Error;

/// Fully resolved server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the HTTP server binds to.
    pub bind: SocketAddr,
    /// Authentication and per-token rate policy.
    pub auth: Auth,
    /// Rate limit applied when no per-token rate is configured.
    pub default_rate: RateRule,
    /// Maximum number of monty executions running concurrently.
    pub max_concurrency: usize,
    /// Maximum accepted request body size, in bytes.
    pub max_body_bytes: usize,
    /// Overall HTTP request timeout (a backstop above the execution limit).
    pub request_timeout: Duration,
    /// Bounds applied to every execution, regardless of what a client requests.
    pub limits: LimitBounds,
    /// Session store configuration.
    pub sessions: SessionConfig,
    /// CORS allowed origins. Empty means CORS is disabled.
    pub cors_origins: Vec<String>,
}

/// How the server authenticates requests.
#[derive(Debug, Clone)]
pub enum Auth {
    /// No tokens configured: requests are unauthenticated and rate limited per client IP.
    Disabled,
    /// Bearer tokens are required. Maps each secret token to its principal.
    Tokens(HashMap<String, Principal>),
}

impl Auth {
    /// A short label for logs: `"token"` or `"disabled"`.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Auth::Disabled => "disabled",
            Auth::Tokens(_) => "token",
        }
    }
}

/// An authenticated caller, derived from a bearer token.
#[derive(Debug, Clone)]
pub struct Principal {
    /// Human-readable label used in logs and as the rate-limit key.
    pub name: String,
    /// Rate limit applied to this principal.
    pub rate: RateRule,
}

/// A fixed-window-free token-bucket rate rule.
#[derive(Debug, Clone, Copy)]
pub struct RateRule {
    /// Sustained requests allowed per minute.
    pub per_minute: u32,
    /// Burst capacity; defaults to `per_minute` when not set explicitly.
    pub burst: u32,
}

/// Hard bounds the server enforces on client-requested execution limits.
#[derive(Debug, Clone, Copy)]
pub struct LimitBounds {
    pub default_duration: Duration,
    pub max_duration: Duration,
    pub default_memory_bytes: usize,
    pub max_memory_bytes: usize,
    pub default_max_allocations: usize,
    pub max_allocations: usize,
    pub default_recursion_depth: usize,
    pub max_recursion_depth: usize,
}

/// Session store sizing and lifetime.
#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    /// Default session time-to-live when a client does not request one.
    pub default_ttl: Duration,
    /// Maximum session time-to-live a client may request.
    pub max_ttl: Duration,
    /// Maximum number of live sessions before creation is rejected.
    pub max_sessions: usize,
}

/// Errors that prevent the server from starting.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid value for {var}: {source}")]
    Int {
        var: &'static str,
        source: ParseIntError,
    },
    #[error("invalid socket address for MONTY_HOST/PORT: {0}")]
    Addr(String),
    #[error("invalid token entry {0:?}: expected `token`, `name:token`, or `name:token:rpm`")]
    Token(String),
}

impl Config {
    /// Loads configuration from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when an environment variable is set but cannot be parsed.
    pub fn from_env() -> Result<Self, ConfigError> {
        let host: IpAddr = env_str("MONTY_HOST")
            .unwrap_or_else(|| "0.0.0.0".to_owned())
            .parse()
            .map_err(|_| ConfigError::Addr("MONTY_HOST".to_owned()))?;
        // Railway and most PaaS providers inject the listen port as `PORT`.
        let port = env_int::<u16>("PORT")?.unwrap_or(8080);

        let default_rate = RateRule::new(env_int("MONTY_RATE_LIMIT_RPM")?.unwrap_or(120));
        let auth = load_auth(default_rate)?;

        let max_duration =
            Duration::from_millis(env_int("MONTY_MAX_TIMEOUT_MS")?.unwrap_or(30_000));
        let request_timeout = max_duration + Duration::from_secs(5);

        let config = Config {
            bind: SocketAddr::new(host, port),
            auth,
            default_rate,
            max_concurrency: env_int("MONTY_MAX_CONCURRENCY")?.unwrap_or_else(default_concurrency),
            max_body_bytes: env_int("MONTY_MAX_BODY_BYTES")?.unwrap_or(1 << 20),
            request_timeout,
            limits: LimitBounds {
                default_duration: Duration::from_millis(
                    env_int("MONTY_DEFAULT_TIMEOUT_MS")?.unwrap_or(5_000),
                ),
                max_duration,
                default_memory_bytes: env_int("MONTY_DEFAULT_MEMORY_BYTES")?.unwrap_or(64 << 20),
                max_memory_bytes: env_int("MONTY_MAX_MEMORY_BYTES")?.unwrap_or(256 << 20),
                default_max_allocations: env_int("MONTY_DEFAULT_MAX_ALLOCATIONS")?
                    .unwrap_or(5_000_000),
                max_allocations: env_int("MONTY_MAX_ALLOCATIONS")?.unwrap_or(50_000_000),
                default_recursion_depth: env_int("MONTY_DEFAULT_RECURSION_DEPTH")?.unwrap_or(1_000),
                max_recursion_depth: env_int("MONTY_MAX_RECURSION_DEPTH")?.unwrap_or(4_000),
            },
            sessions: SessionConfig {
                default_ttl: Duration::from_secs(
                    env_int("MONTY_SESSION_TTL_SECONDS")?.unwrap_or(300),
                ),
                max_ttl: Duration::from_secs(
                    env_int("MONTY_SESSION_MAX_TTL_SECONDS")?.unwrap_or(3_600),
                ),
                max_sessions: env_int("MONTY_MAX_SESSIONS")?.unwrap_or(1_000),
            },
            cors_origins: env_str("MONTY_CORS_ORIGINS")
                .map(|s| s.split(',').map(|o| o.trim().to_owned()).collect())
                .unwrap_or_default(),
        };
        Ok(config)
    }
}

impl RateRule {
    /// Builds a rate rule with burst capacity equal to the per-minute rate.
    fn new(per_minute: u32) -> Self {
        Self {
            per_minute: per_minute.max(1),
            burst: per_minute.max(1),
        }
    }
}

/// Parses `MONTY_API_TOKENS` into an [`Auth`] policy.
///
/// The variable is a comma-separated list where each entry is one of:
/// `token`, `name:token`, or `name:token:requests_per_minute`.
fn load_auth(default_rate: RateRule) -> Result<Auth, ConfigError> {
    let Some(raw) = env_str("MONTY_API_TOKENS") else {
        return Ok(Auth::Disabled);
    };

    let mut tokens = HashMap::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let parts: Vec<&str> = entry.split(':').collect();
        let (name, token, rate) = match parts.as_slice() {
            [token] => ((*token).to_owned(), (*token).to_owned(), default_rate),
            [name, token] => ((*name).to_owned(), (*token).to_owned(), default_rate),
            [name, token, rpm] => {
                let per_minute = rpm.parse().map_err(|source| ConfigError::Int {
                    var: "MONTY_API_TOKENS",
                    source,
                })?;
                (
                    (*name).to_owned(),
                    (*token).to_owned(),
                    RateRule::new(per_minute),
                )
            }
            _ => return Err(ConfigError::Token(entry.to_owned())),
        };
        tokens.insert(token, Principal { name, rate });
    }

    if tokens.is_empty() {
        return Ok(Auth::Disabled);
    }
    Ok(Auth::Tokens(tokens))
}

/// Default execution concurrency: one in-flight execution per CPU.
fn default_concurrency() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Reads an environment variable, treating empty values as unset.
fn env_str(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// Reads and parses an integer environment variable.
fn env_int<T>(var: &'static str) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr<Err = ParseIntError>,
{
    match env_str(var) {
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|source| ConfigError::Int { var, source }),
        None => Ok(None),
    }
}
