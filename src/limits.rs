//! Translating client-requested execution limits into enforced server limits.
//!
//! Clients may ask for *lower* limits than the server default, but never
//! higher: every requested value is clamped to the configured ceiling. A
//! request that omits limits gets the server defaults. There is no "unlimited"
//! option — every execution is always bounded.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::LimitBounds;

/// Limits as requested by a client. All fields optional; omitted fields use the
/// server default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsRequest {
    /// Maximum execution time in milliseconds.
    pub max_duration_ms: Option<u64>,
    /// Maximum approximate heap memory in bytes.
    pub max_memory_bytes: Option<usize>,
    /// Maximum number of heap allocations.
    pub max_allocations: Option<usize>,
    /// Maximum recursion (call stack) depth.
    pub max_recursion_depth: Option<usize>,
}

/// Fully resolved, server-clamped limits applied to an execution.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ResolvedLimits {
    #[serde(serialize_with = "serialize_millis")]
    pub max_duration: Duration,
    pub max_memory_bytes: usize,
    pub max_allocations: usize,
    pub max_recursion_depth: usize,
    /// Garbage-collection cadence; `None` lets monty pick its own default.
    #[serde(skip)]
    pub gc_interval: Option<usize>,
}

impl ResolvedLimits {
    /// Resolves a (possibly absent) client request against the server bounds.
    ///
    /// Each value falls back to the configured default when omitted, and is
    /// then clamped so it never exceeds the configured maximum.
    #[must_use]
    pub fn resolve(request: Option<&LimitsRequest>, bounds: &LimitBounds) -> Self {
        let request = request.cloned_or_default();

        let max_duration = request
            .max_duration_ms
            .map(Duration::from_millis)
            .unwrap_or(bounds.default_duration)
            .min(bounds.max_duration);

        let max_memory_bytes = request
            .max_memory_bytes
            .unwrap_or(bounds.default_memory_bytes)
            .min(bounds.max_memory_bytes);

        let max_allocations = request
            .max_allocations
            .unwrap_or(bounds.default_max_allocations)
            .min(bounds.max_allocations);

        let max_recursion_depth = request
            .max_recursion_depth
            .unwrap_or(bounds.default_recursion_depth)
            .min(bounds.max_recursion_depth);

        ResolvedLimits {
            max_duration,
            max_memory_bytes,
            max_allocations,
            max_recursion_depth,
            gc_interval: None,
        }
    }
}

/// Tiny helper so `resolve` reads cleanly whether or not a request was sent.
trait ClonedOrDefault {
    fn cloned_or_default(self) -> LimitsRequest;
}

impl ClonedOrDefault for Option<&LimitsRequest> {
    fn cloned_or_default(self) -> LimitsRequest {
        match self {
            Some(r) => LimitsRequest {
                max_duration_ms: r.max_duration_ms,
                max_memory_bytes: r.max_memory_bytes,
                max_allocations: r.max_allocations,
                max_recursion_depth: r.max_recursion_depth,
            },
            None => LimitsRequest::default(),
        }
    }
}

/// Serializes a `Duration` as integer milliseconds so the JSON shape matches
/// the `max_duration_ms` field clients send.
fn serialize_millis<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(d.as_millis() as u64)
}
