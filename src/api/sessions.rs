//! `/v1/sessions` — iterative ("code mode") execution.
//!
//! A session keeps a paused monty program alive between requests so a client
//! can service host-function calls one step at a time:
//!
//! 1. `POST /v1/sessions` starts a program; it runs until it finishes or pauses.
//! 2. `POST /v1/sessions/{id}/resume` answers the pause; it runs to the next one.
//! 3. `GET /v1/sessions/{id}` reads the current state without resuming.
//! 4. `DELETE /v1/sessions/{id}` discards the session.
//!
//! Sessions are owned by the API token that created them, single-flight per
//! session, and TTL-evicted (see [`crate::session`]).

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::dto::{SourceFields, SourceSpec, Stats, breach_name, resolve_limits};
use crate::api::spawn_execution;
use crate::auth::Caller;
use crate::config::SessionConfig;
use crate::convert::{
    ApiException, exception_from_parts, json_to_monty, monty_pairs_to_json, monty_slice_to_json,
    monty_to_json,
};
use crate::engine::{self, FutureOutcome, ResumeInput, ResumeResult, SessionStep};
use crate::error::{ApiError, ApiResult};
use crate::session::{AcquireError, SessionEntry, StepSnapshot};
use crate::state::AppState;
use crate::tracker::StatsTracker;

/// Request body for `POST /v1/sessions`.
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// What to execute — same `code`/`snapshot`/`inputs`/`limits` shape as `/v1/run`.
    #[serde(flatten)]
    pub source: SourceFields,
    /// How long the session may stay idle before eviction. Clamped to the
    /// server maximum; defaults to the server default.
    pub ttl_seconds: Option<u64>,
}

/// Request body for `POST /v1/sessions/{id}/resume`.
///
/// Exactly one field must be set, and it must match what the session is paused
/// on (see [`crate::engine::ResumeInput`]).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeRequest {
    /// Return value for a paused function call.
    #[serde(rename = "return")]
    pub return_value: Option<Value>,
    /// Exception to raise from a paused function call.
    pub exception: Option<ExceptionInput>,
    /// Set `true` when the paused function is an async coroutine.
    pub pending: Option<bool>,
    /// Resolved value for a paused name lookup.
    pub value: Option<Value>,
    /// Set `true` to report a paused name lookup as undefined (raises `NameError`).
    pub undefined: Option<bool>,
    /// Results for pending futures.
    pub futures: Option<Vec<FutureInput>>,
}

/// A client-supplied exception, used to raise into a paused session.
#[derive(Debug, Deserialize)]
pub struct ExceptionInput {
    /// Python exception type name, e.g. `"ValueError"`.
    #[serde(rename = "type")]
    pub exc_type: String,
    /// Optional exception message.
    pub message: Option<String>,
}

/// One pending future's result.
#[derive(Debug, Deserialize)]
pub struct FutureInput {
    /// The `call_id` of the pending future, from an earlier `function_call` pause.
    pub call_id: u32,
    /// The future's return value.
    #[serde(rename = "return")]
    pub return_value: Option<Value>,
    /// The exception the future raised, instead of a return value.
    pub exception: Option<ExceptionInput>,
}

impl ExceptionInput {
    fn into_exception(self) -> ApiResult<monty::MontyException> {
        exception_from_parts(&self.exc_type, self.message)
            .map_err(|e| ApiError::BadRequest(e.message().to_owned()))
    }
}

impl FutureInput {
    fn into_outcome(self) -> ApiResult<(u32, FutureOutcome)> {
        match (self.return_value, self.exception) {
            (Some(value), None) => {
                let value = json_to_monty(&value).map_err(bad_value)?;
                Ok((self.call_id, FutureOutcome::Return(value)))
            }
            (None, Some(exc)) => Ok((self.call_id, FutureOutcome::Raise(exc.into_exception()?))),
            _ => Err(ApiError::BadRequest(format!(
                "future {} must set exactly one of `return` or `exception`",
                self.call_id
            ))),
        }
    }
}

impl ResumeRequest {
    /// Validates that exactly one resume action is set and converts it into an
    /// engine [`ResumeInput`].
    fn into_input(self) -> ApiResult<ResumeInput> {
        let set = [
            self.return_value.is_some(),
            self.exception.is_some(),
            self.pending == Some(true),
            self.value.is_some(),
            self.undefined == Some(true),
            self.futures.is_some(),
        ];
        if set.iter().filter(|s| **s).count() != 1 {
            return Err(ApiError::BadRequest(
                "resume body must set exactly one of: `return`, `exception`, `pending`, \
                 `value`, `undefined`, `futures`"
                    .into(),
            ));
        }

        if let Some(value) = self.return_value {
            return Ok(ResumeInput::Return(
                json_to_monty(&value).map_err(bad_value)?,
            ));
        }
        if let Some(exc) = self.exception {
            return Ok(ResumeInput::Raise(exc.into_exception()?));
        }
        if self.pending == Some(true) {
            return Ok(ResumeInput::Pending);
        }
        if let Some(value) = self.value {
            return Ok(ResumeInput::Name(json_to_monty(&value).map_err(bad_value)?));
        }
        if self.undefined == Some(true) {
            return Ok(ResumeInput::NameUndefined);
        }
        if let Some(futures) = self.futures {
            let outcomes = futures
                .into_iter()
                .map(FutureInput::into_outcome)
                .collect::<ApiResult<Vec<_>>>()?;
            return Ok(ResumeInput::Futures(outcomes));
        }
        // Unreachable: the count check above guarantees one branch fired.
        Err(ApiError::Internal)
    }
}

fn bad_value(err: crate::convert::ConvertError) -> ApiError {
    ApiError::BadRequest(format!("invalid value: {}", err.message()))
}

/// A session's state, as returned by create / resume / status.
#[derive(Debug, Serialize)]
pub struct SessionView {
    pub session_id: Uuid,
    /// Seconds until the session is evicted if left idle.
    pub expires_in_seconds: u64,
    #[serde(flatten)]
    pub state: SessionState,
}

/// The `status`-tagged body of a [`SessionView`].
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionState {
    /// Paused awaiting host input; answer it with `POST .../resume`.
    Paused {
        pause: Pause,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
    /// Finished with a value.
    Completed {
        result: Value,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
    /// Finished with an uncaught exception.
    Exception {
        exception: ApiException,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
    /// Finished by tripping a resource limit.
    LimitExceeded {
        limit: &'static str,
        exception: ApiException,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
}

/// Why a session is paused, and the data needed to answer it.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pause {
    /// The program called a host function. Answer with `return`/`exception`/`pending`.
    FunctionCall {
        call_id: u32,
        function: String,
        /// `true` if this is a method call (the receiver is `args[0]`).
        method_call: bool,
        /// Positional arguments, in natural-form JSON.
        args: Value,
        /// Keyword arguments, as a JSON object.
        kwargs: Value,
    },
    /// The program referenced an undefined name. Answer with `value`/`undefined`.
    NameLookup { name: String },
    /// All async tasks are blocked on futures. Answer with `futures`.
    ResolveFutures { pending_call_ids: Vec<u32> },
}

/// `POST /v1/sessions` either creates a session or reports a compile failure.
pub enum CreateOutcome {
    Created(SessionView),
    CompileFailed(ApiException),
}

impl IntoResponse for CreateOutcome {
    fn into_response(self) -> Response {
        match self {
            CreateOutcome::Created(view) => (StatusCode::CREATED, Json(view)).into_response(),
            CreateOutcome::CompileFailed(exception) => (
                StatusCode::OK,
                Json(json!({ "status": "compile_error", "exception": exception })),
            )
                .into_response(),
        }
    }
}

/// Handles `POST /v1/sessions`.
pub async fn create(
    State(state): State<AppState>,
    axum::Extension(caller): axum::Extension<Caller>,
    Json(request): Json<CreateSessionRequest>,
) -> ApiResult<CreateOutcome> {
    let limits = resolve_limits(request.source.limits.as_ref(), &state.config.limits);
    let ttl = resolve_ttl(request.ttl_seconds, &state.config.sessions);
    let spec = request.source.into_spec()?;

    // Compile + start: a compile failure short-circuits to a `compile_error`.
    let started = spawn_execution(&state, move || -> ApiResult<StartOutcome> {
        let (run, inputs) = match spec {
            SourceSpec::Code {
                code,
                filename,
                input_names,
                inputs,
            } => match engine::compile(code, &filename, input_names) {
                Ok(run) => (run, inputs),
                Err(exc) => return Ok(StartOutcome::CompileFailed(ApiException::from(&exc))),
            },
            SourceSpec::Snapshot { run, inputs } => (*run, inputs),
        };
        let (tracker, step) = engine::start_session(run, inputs, &limits);
        Ok(StartOutcome::Started { tracker, step })
    })
    .await?;

    let (tracker, step) = match started {
        StartOutcome::CompileFailed(exc) => return Ok(CreateOutcome::CompileFailed(exc)),
        StartOutcome::Started { tracker, step } => (tracker, step),
    };

    let id = state
        .sessions
        .create(caller.id.clone(), ttl, tracker, step)
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;

    let session = state
        .sessions
        .get(&id, &caller.id)
        .ok_or(ApiError::Internal)?;
    let guard = lock(&session)?;
    let view = render(id, &guard)?;
    drop(guard);
    Ok(CreateOutcome::Created(view))
}

/// Result of the compile-and-start blocking step.
enum StartOutcome {
    Started {
        tracker: StatsTracker,
        step: SessionStep,
    },
    CompileFailed(ApiException),
}

/// Handles `POST /v1/sessions/{id}/resume`.
pub async fn resume(
    State(state): State<AppState>,
    axum::Extension(caller): axum::Extension<Caller>,
    Path(id): Path<Uuid>,
    Json(request): Json<ResumeRequest>,
) -> ApiResult<Json<SessionView>> {
    let input = request.into_input()?;
    let session = state
        .sessions
        .get(&id, &caller.id)
        .ok_or(ApiError::SessionNotFound)?;

    // Take the progress out under the lock; this marks the session busy so a
    // concurrent resume is rejected rather than racing.
    let (progress, tracker) = lock(&session)?
        .acquire_for_resume()
        .map_err(acquire_error)?;

    let result = spawn_execution(&state, move || {
        Ok(engine::resume_session(progress, &tracker, input))
    })
    .await?;

    // Put the session back into a consistent state before responding.
    match result {
        ResumeResult::Stepped(step) => lock(&session)?.store_step(step),
        ResumeResult::Mismatch { progress, error } => {
            lock(&session)?.restore(progress);
            return Err(ApiError::BadRequest(error.message().to_owned()));
        }
    }

    let guard = lock(&session)?;
    let view = render(id, &guard)?;
    drop(guard);
    Ok(Json(view))
}

/// Handles `GET /v1/sessions/{id}`.
pub async fn status(
    State(state): State<AppState>,
    axum::Extension(caller): axum::Extension<Caller>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SessionView>> {
    let session = state
        .sessions
        .get(&id, &caller.id)
        .ok_or(ApiError::SessionNotFound)?;
    let guard = lock(&session)?;
    let view = render(id, &guard)?;
    drop(guard);
    Ok(Json(view))
}

/// Handles `DELETE /v1/sessions/{id}`.
pub async fn delete(
    State(state): State<AppState>,
    axum::Extension(caller): axum::Extension<Caller>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if state.sessions.remove(&id, &caller.id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::SessionNotFound)
    }
}

/// Locks a session entry, recovering from a poisoned mutex rather than crashing.
fn lock(
    session: &crate::session::SharedSession,
) -> ApiResult<std::sync::MutexGuard<'_, SessionEntry>> {
    Ok(session.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Builds the API view of a session from its stored entry.
fn render(id: Uuid, entry: &SessionEntry) -> ApiResult<SessionView> {
    let expires_in_seconds = entry
        .expires_at()
        .saturating_duration_since(Instant::now())
        .as_secs();
    let state = render_state(entry.snapshot(), entry.progress())?;
    Ok(SessionView {
        session_id: id,
        expires_in_seconds,
        state,
    })
}

/// Converts a stored [`StepSnapshot`] (plus the live progress, when paused) into
/// the serializable [`SessionState`].
fn render_state(
    snapshot: &StepSnapshot,
    progress: Option<&monty::RunProgress<StatsTracker>>,
) -> ApiResult<SessionState> {
    match snapshot {
        StepSnapshot::Paused { output, stats } => {
            // A paused snapshot must always have its progress kept alongside it.
            let progress = progress.ok_or(ApiError::Internal)?;
            Ok(SessionState::Paused {
                pause: describe_pause(progress)?,
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                stats: (*stats).into(),
            })
        }
        StepSnapshot::Completed {
            result,
            output,
            stats,
        } => Ok(SessionState::Completed {
            result: monty_to_json(result).map_err(|_| ApiError::Internal)?,
            stdout: output.stdout.clone(),
            stderr: output.stderr.clone(),
            stats: (*stats).into(),
        }),
        StepSnapshot::Failed {
            exception,
            breach,
            output,
            stats,
        } => {
            let exception = ApiException::from(exception);
            let (stdout, stderr) = (output.stdout.clone(), output.stderr.clone());
            let stats = (*stats).into();
            Ok(match breach {
                Some(breach) => SessionState::LimitExceeded {
                    limit: breach_name(*breach),
                    exception,
                    stdout,
                    stderr,
                    stats,
                },
                None => SessionState::Exception {
                    exception,
                    stdout,
                    stderr,
                    stats,
                },
            })
        }
    }
}

/// Describes the pause a live progress represents.
fn describe_pause(progress: &monty::RunProgress<StatsTracker>) -> ApiResult<Pause> {
    use monty::RunProgress;
    match progress {
        RunProgress::FunctionCall(call) => Ok(Pause::FunctionCall {
            call_id: call.call_id,
            function: call.function_name.clone(),
            method_call: call.method_call,
            args: monty_slice_to_json(&call.args).map_err(|_| ApiError::Internal)?,
            kwargs: monty_pairs_to_json(&call.kwargs).map_err(|_| ApiError::Internal)?,
        }),
        RunProgress::NameLookup(lookup) => Ok(Pause::NameLookup {
            name: lookup.name.clone(),
        }),
        RunProgress::ResolveFutures(futures) => Ok(Pause::ResolveFutures {
            pending_call_ids: futures.pending_call_ids().to_vec(),
        }),
        // OS calls are auto-rejected by the engine and `Complete` is terminal,
        // so neither is ever stored as a live, resumable progress.
        RunProgress::OsCall(_) | RunProgress::Complete(_) => Err(ApiError::Internal),
    }
}

/// Maps a session-store acquire failure to the right HTTP error.
fn acquire_error(err: AcquireError) -> ApiError {
    match err {
        AcquireError::NotFound => ApiError::SessionNotFound,
        AcquireError::Busy => ApiError::SessionBusy,
        AcquireError::Finished => {
            ApiError::BadRequest("session has already finished and cannot be resumed".into())
        }
    }
}

/// Resolves the session TTL: the requested value, defaulted and clamped.
fn resolve_ttl(requested: Option<u64>, config: &SessionConfig) -> Duration {
    requested
        .map(Duration::from_secs)
        .unwrap_or(config.default_ttl)
        .min(config.max_ttl)
}
