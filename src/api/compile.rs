//! `POST /v1/compile` — parse Python source into a reusable snapshot.
//!
//! A snapshot is monty's compiled program state. Compiling once and then
//! running the returned `snapshot` repeatedly (via `/v1/run` or `/v1/sessions`)
//! skips re-parsing on every call — useful for hot paths and for validating
//! code without executing it.

use axum::Json;
use axum::extract::State;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::api::spawn_execution;
use crate::convert::ApiException;
use crate::engine;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Request body for `POST /v1/compile`.
#[derive(Debug, Deserialize)]
pub struct CompileRequest {
    /// Python source to compile.
    pub code: String,
    /// Script name shown in tracebacks. Defaults to `main.py`.
    pub filename: Option<String>,
    /// Names of the inputs the program expects. These are baked into the
    /// snapshot; a later `/v1/run` supplies the matching values positionally.
    pub inputs: Option<Vec<String>>,
}

/// Response body for `POST /v1/compile`.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompileResponse {
    /// The source compiled; `snapshot` is base64 and reusable as a `snapshot` field.
    Compiled {
        snapshot: String,
        /// The input names baked into the snapshot, echoed back for convenience.
        input_names: Vec<String>,
    },
    /// The source did not parse.
    CompileError { exception: ApiException },
}

/// Handles `POST /v1/compile`. Responds `200 OK` whether or not the code
/// compiles — a `compile_error` is a normal outcome, not a request error.
pub async fn compile(
    State(state): State<AppState>,
    Json(request): Json<CompileRequest>,
) -> ApiResult<Json<CompileResponse>> {
    let filename = request.filename.unwrap_or_else(|| "main.py".to_owned());
    let input_names = request.inputs.unwrap_or_default();
    let code = request.code;

    let response = spawn_execution(&state, move || {
        match engine::compile(code, &filename, input_names.clone()) {
            Ok(run) => {
                let bytes = run.dump().map_err(|_| ApiError::Internal)?;
                Ok(CompileResponse::Compiled {
                    snapshot: BASE64.encode(bytes),
                    input_names,
                })
            }
            Err(exc) => Ok(CompileResponse::CompileError {
                exception: ApiException::from(&exc),
            }),
        }
    })
    .await?;

    Ok(Json(response))
}
