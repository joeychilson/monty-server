//! `POST /v1/run` — compile (or load) a program and run it to completion.
//!
//! This is the simple, stateless endpoint: one request in, one result out.
//! Host functions are not available here; a program that calls an undefined
//! function raises `NameError`. Use `/v1/sessions` for host-function ("code
//! mode") execution.

use axum::Json;
use axum::extract::State;

use crate::api::dto::{Outcome, SourceFields, SourceSpec, resolve_limits};
use crate::api::spawn_execution;
use crate::engine;
use crate::error::ApiResult;
use crate::state::AppState;

/// Handles `POST /v1/run`.
///
/// Always responds `200 OK` with a `status`-tagged [`Outcome`] when the request
/// is well-formed — `completed`, `exception`, `limit_exceeded`, or
/// `compile_error` all describe what the *code* did. Only a malformed *request*
/// produces a `4xx`.
pub async fn run(
    State(state): State<AppState>,
    Json(request): Json<SourceFields>,
) -> ApiResult<Json<Outcome>> {
    let limits = resolve_limits(request.limits.as_ref(), &state.config.limits);
    let spec = request.into_spec()?;

    let outcome = spawn_execution(&state, move || {
        let (run, inputs) = match spec {
            SourceSpec::Code {
                code,
                filename,
                input_names,
                inputs,
            } => match engine::compile(code, &filename, input_names) {
                Ok(run) => (run, inputs),
                Err(exc) => return Ok(Outcome::compile_error(&exc)),
            },
            SourceSpec::Snapshot { run, inputs } => (*run, inputs),
        };

        let result = engine::run_to_completion(&run, inputs, &limits);
        Outcome::from_exec_result(result)
    })
    .await?;

    Ok(Json(outcome))
}
