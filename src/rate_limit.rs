//! A small per-key token-bucket rate limiter.
//!
//! Each key (an API token name, or a client IP when auth is disabled) gets its
//! own bucket that refills continuously at the key's configured rate. This is a
//! few lines of arithmetic — far less than wiring in and configuring an
//! external rate-limiting crate — and it keeps the limiter's behaviour fully
//! visible to a reviewer.

use std::sync::Mutex;
use std::time::Instant;

use dashmap::DashMap;

use crate::config::RateRule;

/// Idle buckets older than this are pruned so unbounded distinct keys (e.g. IPs)
/// cannot grow the map without limit.
const IDLE_PRUNE_SECS: f64 = 600.0;

/// Per-key token-bucket rate limiter.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: DashMap<String, Mutex<Bucket>>,
}

#[derive(Debug)]
struct Bucket {
    /// Whole and fractional tokens currently available.
    tokens: f64,
    /// When `tokens` was last refilled.
    last: Instant,
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        RateLimiter::default()
    }

    /// Charges one request against `key`.
    ///
    /// Returns `Ok(())` if the request is allowed, or `Err(retry_after_secs)`
    /// with the number of whole seconds to wait before the next token is free.
    pub fn check(&self, key: &str, rule: RateRule) -> Result<(), u64> {
        let refill_per_sec = f64::from(rule.per_minute) / 60.0;
        let capacity = f64::from(rule.burst);

        let bucket = self.buckets.entry(key.to_owned()).or_insert_with(|| {
            Mutex::new(Bucket {
                tokens: capacity,
                last: Instant::now(),
            })
        });
        let mut bucket = bucket.lock().unwrap_or_else(|p| p.into_inner());

        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(capacity);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let deficit = 1.0 - bucket.tokens;
            let wait = deficit / refill_per_sec;
            Err(wait.ceil() as u64)
        }
    }

    /// Drops buckets that have been idle long enough to be fully refilled —
    /// removing them is equivalent to keeping them, but frees memory.
    pub fn prune_idle(&self) {
        let now = Instant::now();
        self.buckets.retain(|_, bucket| {
            bucket
                .try_lock()
                .map(|b| now.duration_since(b.last).as_secs_f64() < IDLE_PRUNE_SECS)
                .unwrap_or(true)
        });
    }
}
