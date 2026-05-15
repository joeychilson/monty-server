//! In-memory store for iterative ("code mode") execution sessions.
//!
//! A session holds a paused monty `RunProgress` between HTTP requests so a
//! client can answer host-function calls one resume at a time. Sessions are:
//!
//! * **owned by their creator** — a session is only visible to the principal
//!   that created it, so ids cannot be probed across tenants.
//! * **single-flight** — at most one resume runs at a time per session; a
//!   concurrent resume gets `409 session_busy`.
//! * **bounded** — capped in count and evicted on a TTL by a background reaper,
//!   so a crashed or abandoned client cannot leak memory.
//!
//! The store is intentionally process-local. Horizontal scaling would need
//! sticky routing or an external store; that trade-off is documented in the
//! README rather than hidden behind a database dependency.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use monty::{MontyException, MontyObject, RunProgress};
use uuid::Uuid;

use crate::engine::{Output, SessionStep};
use crate::tracker::{Breach, ExecStats, StatsTracker};

/// A reference-counted, individually locked session entry.
pub type SharedSession = Arc<Mutex<SessionEntry>>;

/// Process-local registry of live sessions.
#[derive(Debug)]
pub struct SessionStore {
    sessions: DashMap<Uuid, SharedSession>,
    max_sessions: usize,
}

/// One session's mutable state. Guarded by a `Mutex` so resumes are serialized.
#[derive(Debug)]
pub struct SessionEntry {
    /// Principal that created the session; only it may see or resume the session.
    principal: String,
    /// How long each resume extends the session's life.
    ttl: Duration,
    created_at: Instant,
    expires_at: Instant,
    /// Shared resource tracker, kept so stats survive past a consuming resume.
    tracker: StatsTracker,
    /// The resumable progress. `Some` only when paused **and** idle; taken out
    /// while a resume is in flight, and left `None` once terminal. Boxed because
    /// monty's `RunProgress` is large (it holds a full VM snapshot).
    progress: Option<Box<RunProgress<StatsTracker>>>,
    /// Whether the session has reached a terminal (completed/failed) state.
    terminal: bool,
    /// Summary of the latest step, so a status read needs no re-execution.
    snapshot: StepSnapshot,
}

/// Serializable summary of a session's most recent step.
#[derive(Debug)]
pub enum StepSnapshot {
    /// Paused awaiting host input. The pause details come from [`SessionEntry::progress`].
    Paused { output: Output, stats: ExecStats },
    /// Finished with a value.
    Completed {
        result: MontyObject,
        output: Output,
        stats: ExecStats,
    },
    /// Finished with an exception (a resource-limit breach if `breach` is set).
    Failed {
        exception: MontyException,
        breach: Option<Breach>,
        output: Output,
        stats: ExecStats,
    },
}

/// Why a session could not be created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("session capacity reached ({0} live sessions); retry later")]
    AtCapacity(usize),
}

/// Why a session could not be taken for a resume.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error("session not found")]
    NotFound,
    #[error("session is busy with another resume request")]
    Busy,
    #[error("session has already finished and cannot be resumed")]
    Finished,
}

impl SessionStore {
    /// Creates an empty store that holds at most `max_sessions` sessions.
    #[must_use]
    pub fn new(max_sessions: usize) -> Self {
        SessionStore {
            sessions: DashMap::new(),
            max_sessions,
        }
    }

    /// Number of sessions currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the store holds no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Inserts a freshly started session and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`CreateError::AtCapacity`] when the store is full.
    pub fn create(
        &self,
        principal: String,
        ttl: Duration,
        tracker: StatsTracker,
        step: SessionStep,
    ) -> Result<Uuid, CreateError> {
        if self.sessions.len() >= self.max_sessions {
            return Err(CreateError::AtCapacity(self.max_sessions));
        }

        let now = Instant::now();
        let (progress, terminal, snapshot) = split_step(step);
        let entry = SessionEntry {
            principal,
            ttl,
            created_at: now,
            expires_at: now + ttl,
            tracker,
            progress,
            terminal,
            snapshot,
        };

        let id = Uuid::new_v4();
        self.sessions.insert(id, Arc::new(Mutex::new(entry)));
        Ok(id)
    }

    /// Fetches a session visible to `principal`, or `None` if it does not exist
    /// or belongs to someone else.
    #[must_use]
    pub fn get(&self, id: &Uuid, principal: &str) -> Option<SharedSession> {
        let session = self.sessions.get(id)?;
        let owned = session
            .lock()
            .map(|e| e.principal == principal)
            .unwrap_or(false);
        owned.then(|| Arc::clone(&session))
    }

    /// Removes a session visible to `principal`. Returns whether one was removed.
    pub fn remove(&self, id: &Uuid, principal: &str) -> bool {
        let owned = self
            .sessions
            .get(id)
            .and_then(|s| s.lock().ok().map(|e| e.principal == principal))
            .unwrap_or(false);
        owned && self.sessions.remove(id).is_some()
    }

    /// Evicts every session past its expiry. Returns how many were removed.
    pub fn reap_expired(&self) -> usize {
        let now = Instant::now();
        let expired: Vec<Uuid> = self
            .sessions
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .lock()
                    .map(|e| now >= e.expires_at)
                    .unwrap_or(true)
            })
            .map(|entry| *entry.key())
            .collect();

        for id in &expired {
            self.sessions.remove(id);
        }
        expired.len()
    }
}

impl SessionEntry {
    /// The latest step summary, for a status read.
    #[must_use]
    pub fn snapshot(&self) -> &StepSnapshot {
        &self.snapshot
    }

    /// The resumable progress, present only while paused and idle.
    #[must_use]
    pub fn progress(&self) -> Option<&RunProgress<StatsTracker>> {
        self.progress.as_deref()
    }

    /// Whether the session has reached a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    #[must_use]
    pub fn created_at(&self) -> Instant {
        self.created_at
    }

    #[must_use]
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }

    /// Takes the progress and tracker out for a resume, marking the session
    /// busy. The caller **must** later call [`SessionEntry::store_step`] or
    /// [`SessionEntry::restore`] to leave the session in a consistent state.
    ///
    /// # Errors
    ///
    /// Fails if the session is already finished or already has a resume in flight.
    pub fn acquire_for_resume(
        &mut self,
    ) -> Result<(Box<RunProgress<StatsTracker>>, StatsTracker), AcquireError> {
        if self.terminal {
            return Err(AcquireError::Finished);
        }
        let progress = self.progress.take().ok_or(AcquireError::Busy)?;
        Ok((progress, self.tracker.clone()))
    }

    /// Puts an un-stepped progress back (used when a resume is rejected before
    /// it runs, e.g. a kind mismatch), leaving the session resumable again.
    pub fn restore(&mut self, progress: Box<RunProgress<StatsTracker>>) {
        self.progress = Some(progress);
    }

    /// Records the result of a completed resume step and extends the TTL.
    pub fn store_step(&mut self, step: SessionStep) {
        let (progress, terminal, snapshot) = split_step(step);
        self.progress = progress;
        self.terminal = terminal;
        self.snapshot = snapshot;
        self.expires_at = Instant::now() + self.ttl;
    }
}

/// Splits an engine [`SessionStep`] into the pieces a [`SessionEntry`] stores:
/// the resumable progress (if any), whether it is terminal, and the snapshot.
type SplitStep = (Option<Box<RunProgress<StatsTracker>>>, bool, StepSnapshot);

fn split_step(step: SessionStep) -> SplitStep {
    match step {
        SessionStep::Paused {
            progress,
            output,
            stats,
        } => (
            Some(progress),
            false,
            StepSnapshot::Paused { output, stats },
        ),
        SessionStep::Complete {
            result,
            output,
            stats,
        } => (
            None,
            true,
            StepSnapshot::Completed {
                result,
                output,
                stats,
            },
        ),
        SessionStep::Failed {
            exception,
            breach,
            output,
            stats,
        } => (
            None,
            true,
            StepSnapshot::Failed {
                exception,
                breach,
                output,
                stats,
            },
        ),
    }
}
