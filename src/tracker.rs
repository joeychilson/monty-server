//! Resource enforcement and observability for a single monty execution.
//!
//! monty ships [`monty::LimitedTracker`], but it uses `Cell` interior
//! mutability, so it is `!Sync` and its post-run counters can only be read
//! when you still own the tracker. A server needs neither restriction: it must
//! move tracker-bearing values across threads (`spawn_blocking`, the session
//! store) and it wants to report `allocations` / `peak_memory` *after* a run
//! has completed and consumed the tracker.
//!
//! [`StatsTracker`] solves both: it is an `Arc` over atomic counters, so a
//! cheap clone can be handed to monty while the server keeps its own handle to
//! read stats and limit breaches afterwards. The enforcement logic mirrors
//! `monty::LimitedTracker` exactly.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use monty::{ResourceError, ResourceTracker};

use crate::limits::ResolvedLimits;

/// How often `check_time` actually reads the clock. The monty VM calls
/// `check_time` on every instruction, so sampling keeps the overhead off the
/// hot path while still catching timeouts promptly.
const TIME_CHECK_INTERVAL: u32 = 16;

/// Which limit, if any, a tracker breached. Reported so callers can classify a
/// failed run as `limit_exceeded` rather than a plain Python exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breach {
    Time,
    Memory,
    Allocations,
    Recursion,
}

/// Observable counters captured at a point in time.
#[derive(Debug, Clone, Copy)]
pub struct ExecStats {
    /// Wall-clock time spent executing Python (excludes time between resumes).
    pub duration: Duration,
    /// Total heap allocations performed.
    pub allocations: u64,
    /// Peak approximate heap memory, in bytes.
    pub peak_memory_bytes: usize,
}

/// A shareable, thread-safe resource tracker. Cloning is a cheap `Arc` bump;
/// all clones observe the same counters.
#[derive(Debug, Clone)]
pub struct StatsTracker(Arc<State>);

#[derive(Debug)]
struct State {
    max_duration: Option<Duration>,
    max_memory: Option<usize>,
    max_allocations: Option<u64>,
    max_recursion_depth: Option<usize>,
    gc_interval: Option<usize>,

    allocations: AtomicU64,
    live_memory: AtomicUsize,
    peak_memory: AtomicUsize,
    time_check_counter: AtomicU32,
    /// First breach observed, encoded via [`State::breach_code`]. `0` means none.
    breach: AtomicU8,
    /// Execution-time accounting. A monty execution runs on exactly one thread
    /// at a time, so this is uncontended in practice.
    clock: Mutex<Clock>,
}

#[derive(Debug)]
struct Clock {
    /// Start of the currently running segment.
    segment_start: Instant,
    /// Execution time from previously finished segments (resumed sessions).
    accumulated: Duration,
}

impl StatsTracker {
    /// Creates a tracker enforcing `limits`, with the execution clock started now.
    #[must_use]
    pub fn new(limits: &ResolvedLimits) -> Self {
        StatsTracker(Arc::new(State {
            max_duration: Some(limits.max_duration),
            max_memory: Some(limits.max_memory_bytes),
            max_allocations: Some(limits.max_allocations as u64),
            max_recursion_depth: Some(limits.max_recursion_depth),
            gc_interval: limits.gc_interval,
            allocations: AtomicU64::new(0),
            live_memory: AtomicUsize::new(0),
            peak_memory: AtomicUsize::new(0),
            time_check_counter: AtomicU32::new(0),
            breach: AtomicU8::new(0),
            clock: Mutex::new(Clock {
                segment_start: Instant::now(),
                accumulated: Duration::ZERO,
            }),
        }))
    }

    /// Marks the start of an execution segment. Call immediately before handing
    /// the tracker to `MontyRun::run` / `start` / a `resume`, so that wall time
    /// the client spends *between* resumes is not charged against the limit.
    pub fn begin_segment(&self) {
        let mut clock = self.lock_clock();
        clock.segment_start = Instant::now();
    }

    /// Marks the end of an execution segment, folding its elapsed time into the
    /// accumulated total.
    ///
    /// `segment_start` is reset so that [`StatsTracker::elapsed`] called *after*
    /// the segment (e.g. by [`StatsTracker::stats`]) does not count the just-
    /// folded segment a second time, nor charge the idle time until the next
    /// `begin_segment`.
    pub fn end_segment(&self) {
        let mut clock = self.lock_clock();
        let elapsed = clock.segment_start.elapsed();
        clock.accumulated += elapsed;
        clock.segment_start = Instant::now();
    }

    /// Total execution time across all segments.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        let clock = self.lock_clock();
        clock.accumulated + clock.segment_start.elapsed()
    }

    /// Snapshot of the current counters.
    #[must_use]
    pub fn stats(&self) -> ExecStats {
        ExecStats {
            duration: self.elapsed(),
            allocations: self.0.allocations.load(Ordering::Relaxed),
            peak_memory_bytes: self.0.peak_memory.load(Ordering::Relaxed),
        }
    }

    /// The first limit breach observed, if any.
    #[must_use]
    pub fn breach(&self) -> Option<Breach> {
        match self.0.breach.load(Ordering::Relaxed) {
            1 => Some(Breach::Time),
            2 => Some(Breach::Memory),
            3 => Some(Breach::Allocations),
            4 => Some(Breach::Recursion),
            _ => None,
        }
    }

    fn lock_clock(&self) -> std::sync::MutexGuard<'_, Clock> {
        // The clock mutex is only ever poisoned if a panic happens while held,
        // which would only mis-report timing; recover rather than crash.
        self.0.clock.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl State {
    /// Records the first breach seen; later breaches are ignored.
    fn record_breach(&self, breach: Breach) {
        let code = match breach {
            Breach::Time => 1,
            Breach::Memory => 2,
            Breach::Allocations => 3,
            Breach::Recursion => 4,
        };
        let _ = self
            .breach
            .compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
    }
}

// Enforcement mirrors `monty::LimitedTracker`. A monty execution is single
// threaded, so the load/modify/store sequences here race with nothing; the
// atomics exist only so the server thread can *read* counters concurrently.
impl ResourceTracker for StatsTracker {
    fn on_allocate(&self, get_size: impl FnOnce() -> usize) -> Result<(), ResourceError> {
        let count = self.0.allocations.load(Ordering::Relaxed);
        if let Some(max) = self.0.max_allocations
            && count >= max
        {
            self.0.record_breach(Breach::Allocations);
            return Err(ResourceError::Allocation {
                limit: max as usize,
                count: count as usize + 1,
            });
        }

        let size = get_size();
        let live = self.0.live_memory.load(Ordering::Relaxed);
        let new_memory = live + size;
        if let Some(max) = self.0.max_memory
            && new_memory > max
        {
            self.0.record_breach(Breach::Memory);
            return Err(ResourceError::Memory {
                limit: max,
                used: new_memory,
            });
        }

        self.0.allocations.store(count + 1, Ordering::Relaxed);
        self.store_memory(new_memory);
        Ok(())
    }

    fn on_free(&self, get_size: impl FnOnce() -> usize) {
        let live = self.0.live_memory.load(Ordering::Relaxed);
        self.0
            .live_memory
            .store(live.saturating_sub(get_size()), Ordering::Relaxed);
    }

    fn on_grow(&self, additional_bytes: usize) -> Result<(), ResourceError> {
        let live = self.0.live_memory.load(Ordering::Relaxed);
        let new_memory = live.saturating_add(additional_bytes);
        if let Some(max) = self.0.max_memory
            && new_memory > max
        {
            self.0.record_breach(Breach::Memory);
            return Err(ResourceError::Memory {
                limit: max,
                used: new_memory,
            });
        }
        self.store_memory(new_memory);
        Ok(())
    }

    fn check_time(&self) -> Result<(), ResourceError> {
        let Some(max) = self.0.max_duration else {
            return Ok(());
        };

        let count = self.0.time_check_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if !count.is_multiple_of(TIME_CHECK_INTERVAL) {
            return Ok(());
        }

        let elapsed = self.elapsed();
        if elapsed > max {
            // Re-arm the counter so the next call rechecks: some callers catch
            // this error and return normally, and the VM loop must re-detect it.
            self.0
                .time_check_counter
                .store(TIME_CHECK_INTERVAL - 1, Ordering::Relaxed);
            self.0.record_breach(Breach::Time);
            return Err(ResourceError::Time {
                limit: max,
                elapsed,
            });
        }
        Ok(())
    }

    fn check_recursion_depth(&self, current_depth: usize) -> Result<(), ResourceError> {
        if let Some(max) = self.0.max_recursion_depth
            && current_depth >= max
        {
            self.0.record_breach(Breach::Recursion);
            return Err(ResourceError::Recursion {
                limit: max,
                depth: current_depth + 1,
            });
        }
        Ok(())
    }

    fn check_large_result(&self, estimated_bytes: usize) -> Result<(), ResourceError> {
        if let Some(max) = self.0.max_memory {
            let projected = self
                .0
                .live_memory
                .load(Ordering::Relaxed)
                .saturating_add(estimated_bytes);
            if projected > max {
                self.0.record_breach(Breach::Memory);
                return Err(ResourceError::Memory {
                    limit: max,
                    used: projected,
                });
            }
        }
        Ok(())
    }

    fn gc_interval(&self) -> Option<usize> {
        self.0.gc_interval
    }
}

impl StatsTracker {
    /// Stores live memory and bumps the peak watermark.
    fn store_memory(&self, new_memory: usize) {
        self.0.live_memory.store(new_memory, Ordering::Relaxed);
        self.0.peak_memory.fetch_max(new_memory, Ordering::Relaxed);
    }
}
