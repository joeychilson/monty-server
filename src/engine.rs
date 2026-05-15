//! The bridge between HTTP handlers and the monty interpreter.
//!
//! Everything here is **synchronous and CPU-bound** — monty runs Python on the
//! calling thread. Handlers must therefore invoke these functions inside
//! `tokio::task::spawn_blocking` so the async runtime is never blocked.
//!
//! ## Outcome model
//!
//! Running user code can finish three ways, and all three are *successful* API
//! responses (HTTP 200) — they tell the client what their code did:
//!
//! * **completed** — produced a value.
//! * **exception** — raised an uncaught Python exception.
//! * **limit_exceeded** — tripped a resource limit (a special exception).
//!
//! Only malformed *requests* (bad JSON, bad snapshot bytes) are HTTP errors.
//!
//! ## OS calls
//!
//! monty can pause for filesystem/OS operations. A hosted sandbox must not
//! grant ambient filesystem access, so [`drive`] auto-rejects every OS call
//! with `PermissionError`. The client never sees them.

use monty::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, MontyRun, NameLookupResult,
    PrintStream, PrintWriter, RunProgress,
};

use crate::limits::ResolvedLimits;
use crate::tracker::{Breach, ExecStats, StatsTracker};

/// Captured `print()` output from one execution segment.
#[derive(Debug, Default, Clone)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// Result of running code to completion via [`run_to_completion`].
#[derive(Debug)]
pub enum ExecResult {
    /// The program produced a value.
    Completed {
        result: MontyObject,
        output: Output,
        stats: ExecStats,
    },
    /// The program raised, or tripped a resource limit (`breach` distinguishes them).
    Failed {
        exception: MontyException,
        breach: Option<Breach>,
        output: Output,
        stats: ExecStats,
    },
}

/// State of a session after a [`start_session`] or [`resume_session`] step.
#[derive(Debug)]
pub enum SessionStep {
    /// Execution finished with a value; the session is done.
    Complete {
        result: MontyObject,
        output: Output,
        stats: ExecStats,
    },
    /// Execution raised or tripped a limit; the session is done.
    Failed {
        exception: MontyException,
        breach: Option<Breach>,
        output: Output,
        stats: ExecStats,
    },
    /// Execution paused awaiting host input; the session stays alive.
    ///
    /// `progress` is boxed: monty's `RunProgress` holds a full VM snapshot
    /// (~1 KiB), and boxing keeps this enum — which is moved between threads and
    /// matched on often — pointer-sized in its common variants.
    Paused {
        progress: Box<RunProgress<StatsTracker>>,
        output: Output,
        stats: ExecStats,
    },
}

/// How a client wants to answer a paused session.
#[derive(Debug)]
pub enum ResumeInput {
    /// Return a value from the paused function call.
    Return(MontyObject),
    /// Raise an exception from the paused function call.
    Raise(MontyException),
    /// The paused function is an async coroutine; register a pending future.
    Pending,
    /// Resolve a name lookup to a concrete value.
    Name(MontyObject),
    /// Report the looked-up name as undefined (the VM will raise `NameError`).
    NameUndefined,
    /// Resolve one or more pending futures.
    Futures(Vec<(u32, FutureOutcome)>),
}

/// Outcome supplied for a single pending future.
#[derive(Debug)]
pub enum FutureOutcome {
    Return(MontyObject),
    Raise(MontyException),
}

/// A client's resume input did not match what the session is currently paused on.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ResumeMismatch(String);

impl ResumeMismatch {
    /// The human-readable mismatch reason.
    pub fn message(&self) -> &str {
        &self.0
    }
}

/// Compiles Python source into a reusable [`MontyRun`] snapshot.
///
/// # Errors
///
/// Returns the monty `SyntaxError` (or similar) when the source does not parse.
pub fn compile(
    code: String,
    filename: &str,
    input_names: Vec<String>,
) -> Result<MontyRun, MontyException> {
    MontyRun::new(code, filename, input_names)
}

/// Runs `run` to completion. Host functions are *not* available here — code
/// that calls an undefined function raises `NameError`; use a session instead.
#[must_use]
pub fn run_to_completion(
    run: &MontyRun,
    inputs: Vec<MontyObject>,
    limits: &ResolvedLimits,
) -> ExecResult {
    let tracker = StatsTracker::new(limits);
    let mut fragments = Vec::new();

    tracker.begin_segment();
    let result = run.run(
        inputs,
        tracker.clone(),
        PrintWriter::CollectStreams(&mut fragments),
    );
    tracker.end_segment();

    let output = join_streams(fragments);
    let stats = tracker.stats();
    match result {
        Ok(result) => ExecResult::Completed {
            result,
            output,
            stats,
        },
        Err(exception) => ExecResult::Failed {
            exception,
            breach: tracker.breach(),
            output,
            stats,
        },
    }
}

/// Begins an iterative session: runs until the program completes, fails, or
/// pauses for host input.
#[must_use]
pub fn start_session(
    run: MontyRun,
    inputs: Vec<MontyObject>,
    limits: &ResolvedLimits,
) -> (StatsTracker, SessionStep) {
    let tracker = StatsTracker::new(limits);
    let mut fragments = Vec::new();

    tracker.begin_segment();
    let started = run.start(
        inputs,
        tracker.clone(),
        PrintWriter::CollectStreams(&mut fragments),
    );
    let step = settle(started, &mut fragments);
    tracker.end_segment();

    let step = finish_step(step, &tracker, fragments);
    (tracker, step)
}

/// Outcome of a resume attempt.
#[derive(Debug)]
pub enum ResumeResult {
    /// The session advanced; here is its new state.
    Stepped(SessionStep),
    /// The resume input did not match the pause kind. The session is unchanged
    /// and its `progress` is handed back so the caller can keep it alive.
    Mismatch {
        progress: Box<RunProgress<StatsTracker>>,
        error: ResumeMismatch,
    },
}

/// Resumes a paused session with the client's answer.
///
/// On a kind mismatch (e.g. answering a name lookup with a function return
/// value) the session is left untouched and the progress is returned via
/// [`ResumeResult::Mismatch`] — a resume with the wrong shape must never
/// destroy a live session.
#[must_use]
pub fn resume_session(
    progress: Box<RunProgress<StatsTracker>>,
    tracker: &StatsTracker,
    input: ResumeInput,
) -> ResumeResult {
    if let Err(error) = check_resume(&progress, &input) {
        return ResumeResult::Mismatch { progress, error };
    }

    let mut fragments = Vec::new();
    tracker.begin_segment();
    let resumed = apply_resume(progress, input, &mut fragments);
    let settled = settle(resumed, &mut fragments);
    tracker.end_segment();

    ResumeResult::Stepped(finish_step(settled, tracker, fragments))
}

/// Validates that `input` answers the kind of pause `progress` represents,
/// without consuming either — the consuming resume happens in [`apply_resume`].
fn check_resume(
    progress: &RunProgress<StatsTracker>,
    input: &ResumeInput,
) -> Result<(), ResumeMismatch> {
    let matched = matches!(
        (progress, input),
        (
            RunProgress::FunctionCall(_),
            ResumeInput::Return(_) | ResumeInput::Raise(_) | ResumeInput::Pending,
        ) | (
            RunProgress::NameLookup(_),
            ResumeInput::Name(_) | ResumeInput::NameUndefined,
        ) | (RunProgress::ResolveFutures(_), ResumeInput::Futures(_))
    );
    if matched {
        Ok(())
    } else {
        Err(mismatch(progress, input))
    }
}

/// Applies the client's resume input to the paused progress. The pairing is
/// guaranteed valid by a prior [`check_resume`], so the fallthrough is unreachable.
fn apply_resume(
    progress: Box<RunProgress<StatsTracker>>,
    input: ResumeInput,
    fragments: &mut Vec<(PrintStream, String)>,
) -> Result<RunProgress<StatsTracker>, MontyException> {
    let writer = PrintWriter::CollectStreams(fragments);
    match (*progress, input) {
        (RunProgress::FunctionCall(call), ResumeInput::Return(value)) => call.resume(value, writer),
        (RunProgress::FunctionCall(call), ResumeInput::Raise(exc)) => call.resume(exc, writer),
        (RunProgress::FunctionCall(call), ResumeInput::Pending) => call.resume_pending(writer),
        (RunProgress::NameLookup(lookup), ResumeInput::Name(value)) => {
            lookup.resume(NameLookupResult::Value(value), writer)
        }
        (RunProgress::NameLookup(lookup), ResumeInput::NameUndefined) => {
            lookup.resume(NameLookupResult::Undefined, writer)
        }
        (RunProgress::ResolveFutures(futures), ResumeInput::Futures(results)) => {
            let results = results.into_iter().map(|(id, o)| (id, o.into())).collect();
            futures.resume(results, writer)
        }
        _ => unreachable!("resume kind/input pairing validated by check_resume"),
    }
}

/// Drives a raw progress forward, auto-rejecting OS calls, until it settles on a
/// client-actionable state, completes, or fails.
fn settle(
    progress: Result<RunProgress<StatsTracker>, MontyException>,
    fragments: &mut Vec<(PrintStream, String)>,
) -> Result<RunProgress<StatsTracker>, MontyException> {
    let mut progress = progress?;
    loop {
        match progress {
            RunProgress::OsCall(call) => {
                let denied = MontyException::new(
                    ExcType::PermissionError,
                    Some("filesystem and OS access are disabled on this server".to_owned()),
                );
                progress = call.resume(denied, PrintWriter::CollectStreams(&mut *fragments))?;
            }
            settled => return Ok(settled),
        }
    }
}

/// Converts a settled progress into a [`SessionStep`].
fn finish_step(
    settled: Result<RunProgress<StatsTracker>, MontyException>,
    tracker: &StatsTracker,
    fragments: Vec<(PrintStream, String)>,
) -> SessionStep {
    let output = join_streams(fragments);
    let stats = tracker.stats();
    match settled {
        Ok(RunProgress::Complete(result)) => SessionStep::Complete {
            result,
            output,
            stats,
        },
        Ok(progress) => SessionStep::Paused {
            progress: Box::new(progress),
            output,
            stats,
        },
        Err(exception) => SessionStep::Failed {
            exception,
            breach: tracker.breach(),
            output,
            stats,
        },
    }
}

/// Builds a descriptive mismatch error naming the pause kind and the answer kind.
fn mismatch(progress: &RunProgress<StatsTracker>, input: &ResumeInput) -> ResumeMismatch {
    let expected = match progress {
        RunProgress::FunctionCall(_) => {
            "a function call (expects `return`, `exception`, or `pending`)"
        }
        RunProgress::NameLookup(_) => "a name lookup (expects `value` or `undefined`)",
        RunProgress::ResolveFutures(_) => "pending futures (expects `futures`)",
        RunProgress::OsCall(_) => "an OS call (handled internally; should not be exposed)",
        RunProgress::Complete(_) => "already complete",
    };
    let got = match input {
        ResumeInput::Return(_) => "`return`",
        ResumeInput::Raise(_) => "`exception`",
        ResumeInput::Pending => "`pending`",
        ResumeInput::Name(_) => "`value`",
        ResumeInput::NameUndefined => "`undefined`",
        ResumeInput::Futures(_) => "`futures`",
    };
    ResumeMismatch(format!(
        "session is paused on {expected}, but resume supplied {got}"
    ))
}

impl From<FutureOutcome> for ExtFunctionResult {
    fn from(outcome: FutureOutcome) -> Self {
        match outcome {
            FutureOutcome::Return(value) => ExtFunctionResult::Return(value),
            FutureOutcome::Raise(exc) => ExtFunctionResult::Error(exc),
        }
    }
}

/// Folds monty's per-fragment print stream into whole stdout/stderr strings.
fn join_streams(fragments: Vec<(PrintStream, String)>) -> Output {
    let mut output = Output::default();
    for (stream, text) in fragments {
        match stream {
            PrintStream::Stdout => output.stdout.push_str(&text),
            PrintStream::Stderr => output.stderr.push_str(&text),
        }
    }
    output
}
