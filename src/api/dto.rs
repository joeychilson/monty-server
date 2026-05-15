//! Request/response types shared across the `/v1` endpoints.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use monty::{MontyObject, MontyRun};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::LimitBounds;
use crate::convert::{ApiException, json_to_monty, monty_to_json};
use crate::engine::ExecResult;
use crate::error::ApiError;
use crate::limits::{LimitsRequest, ResolvedLimits};
use crate::tracker::{Breach, ExecStats};

/// The default script name used when a request omits `filename`.
const DEFAULT_FILENAME: &str = "main.py";

/// Fields, common to `/v1/run` and `/v1/sessions`, that say *what* to execute.
#[derive(Debug, Deserialize)]
pub struct SourceFields {
    /// Python source to compile and run.
    pub code: Option<String>,
    /// A base64-encoded snapshot from `/v1/compile`, used instead of `code`.
    pub snapshot: Option<String>,
    /// Script name shown in tracebacks. Defaults to `main.py`.
    pub filename: Option<String>,
    /// Inputs bound before execution. With `code`, a JSON object of
    /// `name -> value`; with `snapshot`, a positional JSON array.
    pub inputs: Option<Value>,
    /// Optional per-execution resource limits, clamped to the server maximum.
    pub limits: Option<LimitsRequest>,
}

/// A validated, ready-to-execute program plus its bound inputs.
pub enum SourceSpec {
    /// Source that still needs compiling (a compile failure is a `200` outcome).
    Code {
        code: String,
        filename: String,
        input_names: Vec<String>,
        inputs: Vec<MontyObject>,
    },
    /// An already-compiled snapshot. `run` is boxed because `MontyRun` is large
    /// and this keeps the `SourceSpec` enum from being dominated by one variant.
    Snapshot {
        run: Box<MontyRun>,
        inputs: Vec<MontyObject>,
    },
}

impl SourceFields {
    /// Validates the source fields and converts inputs into monty values.
    ///
    /// This only reports *request* problems (`400`): exactly one of `code` or
    /// `snapshot` must be set, a snapshot must decode, and inputs must have the
    /// shape required by the chosen source. A *compile* failure is deferred to
    /// the caller because it is a normal `200` execution outcome.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::BadRequest`] for any malformed field.
    pub fn into_spec(self) -> Result<SourceSpec, ApiError> {
        match (self.code, self.snapshot) {
            (Some(_), Some(_)) => Err(ApiError::BadRequest(
                "provide either `code` or `snapshot`, not both".into(),
            )),
            (None, None) => Err(ApiError::BadRequest(
                "one of `code` or `snapshot` is required".into(),
            )),
            (Some(code), None) => {
                let (input_names, inputs) = inputs_for_code(self.inputs)?;
                Ok(SourceSpec::Code {
                    code,
                    filename: self.filename.unwrap_or_else(|| DEFAULT_FILENAME.to_owned()),
                    input_names,
                    inputs,
                })
            }
            (None, Some(snapshot)) => {
                let run = decode_snapshot(&snapshot)?;
                let inputs = inputs_for_snapshot(self.inputs)?;
                Ok(SourceSpec::Snapshot {
                    run: Box::new(run),
                    inputs,
                })
            }
        }
    }
}

/// Decodes a base64 `MontyRun` snapshot.
fn decode_snapshot(snapshot: &str) -> Result<MontyRun, ApiError> {
    let bytes = BASE64
        .decode(snapshot)
        .map_err(|e| ApiError::BadRequest(format!("`snapshot` is not valid base64: {e}")))?;
    MontyRun::load(&bytes)
        .map_err(|e| ApiError::BadRequest(format!("`snapshot` is not a valid monty snapshot: {e}")))
}

/// Parses `inputs` for a `code` request: a JSON object of `name -> value`.
fn inputs_for_code(inputs: Option<Value>) -> Result<(Vec<String>, Vec<MontyObject>), ApiError> {
    match inputs {
        None => Ok((Vec::new(), Vec::new())),
        Some(Value::Object(map)) => {
            let mut names = Vec::with_capacity(map.len());
            let mut values = Vec::with_capacity(map.len());
            for (name, value) in map {
                let value = json_to_monty(&value).map_err(bad_input)?;
                names.push(name);
                values.push(value);
            }
            Ok((names, values))
        }
        Some(Value::Array(items)) if items.is_empty() => Ok((Vec::new(), Vec::new())),
        Some(_) => Err(ApiError::BadRequest(
            "with `code`, `inputs` must be a JSON object mapping input names to values".into(),
        )),
    }
}

/// Parses `inputs` for a `snapshot` request: a positional JSON array (the
/// snapshot already records the input names).
fn inputs_for_snapshot(inputs: Option<Value>) -> Result<Vec<MontyObject>, ApiError> {
    match inputs {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| json_to_monty(v).map_err(bad_input))
            .collect(),
        Some(Value::Object(map)) if map.is_empty() => Ok(Vec::new()),
        Some(_) => Err(ApiError::BadRequest(
            "with `snapshot`, `inputs` must be a positional JSON array".into(),
        )),
    }
}

fn bad_input(err: crate::convert::ConvertError) -> ApiError {
    ApiError::BadRequest(format!("invalid input value: {}", err.message()))
}

/// Observable execution counters, in the response JSON shape.
#[derive(Debug, Serialize)]
pub struct Stats {
    /// Execution time in milliseconds (excludes time between session resumes).
    pub duration_ms: f64,
    /// Total heap allocations performed.
    pub allocations: u64,
    /// Peak approximate heap memory, in bytes.
    pub peak_memory_bytes: usize,
}

impl From<ExecStats> for Stats {
    fn from(stats: ExecStats) -> Self {
        Stats {
            duration_ms: stats.duration.as_secs_f64() * 1000.0,
            allocations: stats.allocations,
            peak_memory_bytes: stats.peak_memory_bytes,
        }
    }
}

/// The name of a breached resource limit, for the `limit` response field.
#[must_use]
pub fn breach_name(breach: Breach) -> &'static str {
    match breach {
        Breach::Time => "time",
        Breach::Memory => "memory",
        Breach::Allocations => "allocations",
        Breach::Recursion => "recursion",
    }
}

/// A terminal execution outcome, tagged by `status`. Shared by `/v1/run` and a
/// session's final state.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// The program produced a value.
    Completed {
        result: Value,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
    /// The program raised an uncaught exception.
    Exception {
        exception: ApiException,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
    /// The program tripped a resource limit.
    LimitExceeded {
        /// Which limit was hit: `time`, `memory`, `allocations`, or `recursion`.
        limit: &'static str,
        exception: ApiException,
        stdout: String,
        stderr: String,
        stats: Stats,
    },
    /// The source failed to compile (only reachable from a `code` request).
    CompileError { exception: ApiException },
}

impl Outcome {
    /// Builds an [`Outcome`] from an engine [`ExecResult`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Internal`] only if a produced value cannot be encoded
    /// as JSON, which should not happen for real execution output.
    pub fn from_exec_result(result: ExecResult) -> Result<Outcome, ApiError> {
        match result {
            ExecResult::Completed {
                result,
                output,
                stats,
            } => {
                let result = monty_to_json(&result).map_err(|_| ApiError::Internal)?;
                Ok(Outcome::Completed {
                    result,
                    stdout: output.stdout,
                    stderr: output.stderr,
                    stats: stats.into(),
                })
            }
            ExecResult::Failed {
                exception,
                breach,
                output,
                stats,
            } => {
                let api_exc = ApiException::from(&exception);
                Ok(match breach {
                    Some(breach) => Outcome::LimitExceeded {
                        limit: breach_name(breach),
                        exception: api_exc,
                        stdout: output.stdout,
                        stderr: output.stderr,
                        stats: stats.into(),
                    },
                    None => Outcome::Exception {
                        exception: api_exc,
                        stdout: output.stdout,
                        stderr: output.stderr,
                        stats: stats.into(),
                    },
                })
            }
        }
    }

    /// Builds a `compile_error` outcome from a monty compile failure.
    #[must_use]
    pub fn compile_error(exception: &monty::MontyException) -> Outcome {
        Outcome::CompileError {
            exception: ApiException::from(exception),
        }
    }
}

/// Resolves the effective limits for a request against the server bounds.
#[must_use]
pub fn resolve_limits(request: Option<&LimitsRequest>, bounds: &LimitBounds) -> ResolvedLimits {
    ResolvedLimits::resolve(request, bounds)
}
