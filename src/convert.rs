//! Conversions between JSON (what HTTP clients speak) and monty's value and
//! exception types.
//!
//! # Value mapping
//!
//! The JSON shape matches monty's own "natural form" (see [`monty::JsonMontyObject`]):
//! JSON-native values map directly (`null`, `true`, `42`, `"s"`, `[...]`,
//! `{...}`), and a few non-JSON-native Python types use a single-key
//! `{"$tag": ...}` object on input:
//!
//! | JSON                          | monty                |
//! |-------------------------------|----------------------|
//! | `{"$tuple": [...]}`           | `tuple`              |
//! | `{"$set": [...]}`             | `set`                |
//! | `{"$frozenset": [...]}`       | `frozenset`          |
//! | `{"$bytes": "base64"}` or `[ints]` | `bytes`         |
//! | `{"$ellipsis": "..."}`        | `...`                |
//! | `{"$float": "nan"\|"inf"\|"-inf"}` | non-finite `float` |
//! | `{"$dict": [[k, v], ...]}`    | `dict` (non-string keys) |
//! | `{"$function": "name"}`       | host function (answers a `name_lookup`) |
//! | `{"$path": "p"}`              | `pathlib.Path`       |
//!
//! Integer inputs are limited to the `i64` range; larger integers are
//! rejected rather than silently truncated. Output uses [`monty::JsonMontyObject`]
//! directly and can represent every monty value.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use monty::{JsonMontyArray, JsonMontyObject, MontyException, MontyObject};
use serde::Serialize;
use serde_json::{Map, Value};

/// A value could not be converted between JSON and monty's representation.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConvertError(String);

impl ConvertError {
    fn new(msg: impl Into<String>) -> Self {
        ConvertError(msg.into())
    }

    /// The human-readable reason, for surfacing in an API error.
    pub fn message(&self) -> &str {
        &self.0
    }
}

/// Converts a JSON value into a monty object.
///
/// # Errors
///
/// Returns [`ConvertError`] for integers outside `i64`, malformed `$`-tagged
/// objects, or other shapes monty cannot represent.
pub fn json_to_monty(value: &Value) -> Result<MontyObject, ConvertError> {
    match value {
        Value::Null => Ok(MontyObject::None),
        Value::Bool(b) => Ok(MontyObject::Bool(*b)),
        Value::Number(n) => number_to_monty(n),
        Value::String(s) => Ok(MontyObject::String(s.clone())),
        Value::Array(items) => Ok(MontyObject::List(json_array_to_monty(items)?)),
        Value::Object(map) => object_to_monty(map),
    }
}

/// Converts a monty object into its natural-form JSON value.
///
/// # Errors
///
/// Returns [`ConvertError`] only if monty's serializer fails, which should not
/// happen for values produced by execution.
pub fn monty_to_json(object: &MontyObject) -> Result<Value, ConvertError> {
    serde_json::to_value(JsonMontyObject(object))
        .map_err(|e| ConvertError::new(format!("failed to encode result as JSON: {e}")))
}

/// Converts a slice of monty objects (e.g. call args) into a JSON array.
///
/// # Errors
///
/// See [`monty_to_json`].
pub fn monty_slice_to_json(objects: &[MontyObject]) -> Result<Value, ConvertError> {
    serde_json::to_value(JsonMontyArray(objects))
        .map_err(|e| ConvertError::new(format!("failed to encode values as JSON: {e}")))
}

/// Converts monty key/value pairs (e.g. call kwargs) into a JSON object.
///
/// Python keyword argument names are always strings, so any non-string key is
/// reported as an error rather than silently dropped.
///
/// # Errors
///
/// Returns [`ConvertError`] for a non-string key or a value monty cannot encode.
pub fn monty_pairs_to_json(pairs: &[(MontyObject, MontyObject)]) -> Result<Value, ConvertError> {
    let mut map = Map::with_capacity(pairs.len());
    for (key, value) in pairs {
        let MontyObject::String(name) = key else {
            return Err(ConvertError::new("keyword argument name was not a string"));
        };
        map.insert(name.clone(), monty_to_json(value)?);
    }
    Ok(Value::Object(map))
}

fn json_array_to_monty(items: &[Value]) -> Result<Vec<MontyObject>, ConvertError> {
    items.iter().map(json_to_monty).collect()
}

fn number_to_monty(n: &serde_json::Number) -> Result<MontyObject, ConvertError> {
    if let Some(i) = n.as_i64() {
        return Ok(MontyObject::Int(i));
    }

    // Not an `i64`. Decide float-vs-integer from the literal itself so the
    // result does not depend on serde_json's `arbitrary_precision` feature.
    let literal = n.to_string();
    if literal.contains(['.', 'e', 'E']) {
        n.as_f64()
            .map(MontyObject::Float)
            .ok_or_else(|| ConvertError::new(format!("unsupported JSON number: {n}")))
    } else {
        // A whole number too large for `i64`. monty's public API exposes no way
        // to build a `BigInt`, so reject rather than silently truncate.
        Err(ConvertError::new(format!(
            "integer {n} is outside the supported i64 input range"
        )))
    }
}

/// Handles JSON objects: either a single-key `$`-tagged special form, or a
/// plain object that becomes a string-keyed dict.
fn object_to_monty(map: &Map<String, Value>) -> Result<MontyObject, ConvertError> {
    if let Some((tag, body)) = single_dollar_tag(map) {
        return tagged_to_monty(tag, body);
    }

    let mut pairs = Vec::with_capacity(map.len());
    for (key, value) in map {
        pairs.push((MontyObject::String(key.clone()), json_to_monty(value)?));
    }
    Ok(MontyObject::Dict(pairs.into()))
}

/// Returns the `(tag, body)` of a single-key object whose key starts with `$`.
fn single_dollar_tag(map: &Map<String, Value>) -> Option<(&str, &Value)> {
    if map.len() != 1 {
        return None;
    }
    let (key, value) = map.iter().next()?;
    key.strip_prefix('$').map(|_| (key.as_str(), value))
}

fn tagged_to_monty(tag: &str, body: &Value) -> Result<MontyObject, ConvertError> {
    match tag {
        "$tuple" => Ok(MontyObject::Tuple(expect_array(tag, body)?)),
        "$set" => Ok(MontyObject::Set(expect_array(tag, body)?)),
        "$frozenset" => Ok(MontyObject::FrozenSet(expect_array(tag, body)?)),
        "$bytes" => bytes_to_monty(body),
        "$ellipsis" => Ok(MontyObject::Ellipsis),
        "$float" => float_tag_to_monty(body),
        "$dict" => dict_tag_to_monty(body),
        "$function" => function_tag_to_monty(body),
        "$path" => path_tag_to_monty(body),
        other => Err(ConvertError::new(format!(
            "unsupported tagged value {other:?}"
        ))),
    }
}

fn expect_array(tag: &str, body: &Value) -> Result<Vec<MontyObject>, ConvertError> {
    match body {
        Value::Array(items) => json_array_to_monty(items),
        _ => Err(ConvertError::new(format!("{tag} expects an array"))),
    }
}

/// Accepts `bytes` as either a base64 string or an array of byte-valued integers.
fn bytes_to_monty(body: &Value) -> Result<MontyObject, ConvertError> {
    match body {
        Value::String(encoded) => BASE64
            .decode(encoded)
            .map(MontyObject::Bytes)
            .map_err(|e| ConvertError::new(format!("$bytes is not valid base64: {e}"))),
        Value::Array(items) => {
            let mut bytes = Vec::with_capacity(items.len());
            for item in items {
                let byte = item
                    .as_u64()
                    .filter(|n| *n <= 255)
                    .ok_or_else(|| ConvertError::new("$bytes array contains a non-byte value"))?;
                bytes.push(byte as u8);
            }
            Ok(MontyObject::Bytes(bytes))
        }
        _ => Err(ConvertError::new(
            "$bytes expects a base64 string or an array of bytes",
        )),
    }
}

fn float_tag_to_monty(body: &Value) -> Result<MontyObject, ConvertError> {
    match body.as_str() {
        Some("nan") => Ok(MontyObject::Float(f64::NAN)),
        Some("inf") => Ok(MontyObject::Float(f64::INFINITY)),
        Some("-inf") => Ok(MontyObject::Float(f64::NEG_INFINITY)),
        _ => Err(ConvertError::new(
            r#"$float expects "nan", "inf", or "-inf""#,
        )),
    }
}

/// `{"$function": "name"}` — a callable host function. This is how a client
/// answers a `name_lookup` pause that resolves to a function it will service.
fn function_tag_to_monty(body: &Value) -> Result<MontyObject, ConvertError> {
    match body.as_str() {
        Some(name) => Ok(MontyObject::Function {
            name: name.to_owned(),
            docstring: None,
        }),
        None => Err(ConvertError::new(
            "$function expects a function name string",
        )),
    }
}

/// `{"$path": "some/path"}` — a `pathlib.Path` value.
fn path_tag_to_monty(body: &Value) -> Result<MontyObject, ConvertError> {
    match body.as_str() {
        Some(path) => Ok(MontyObject::Path(path.to_owned())),
        None => Err(ConvertError::new("$path expects a path string")),
    }
}

/// `{"$dict": [[k, v], ...]}` — a dict whose keys may be non-strings.
fn dict_tag_to_monty(body: &Value) -> Result<MontyObject, ConvertError> {
    let Value::Array(entries) = body else {
        return Err(ConvertError::new(
            "$dict expects an array of [key, value] pairs",
        ));
    };
    let mut pairs = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            Value::Array(kv) if kv.len() == 2 => {
                pairs.push((json_to_monty(&kv[0])?, json_to_monty(&kv[1])?));
            }
            _ => {
                return Err(ConvertError::new(
                    "$dict entries must be [key, value] pairs",
                ));
            }
        }
    }
    Ok(MontyObject::Dict(pairs.into()))
}

/// A monty exception in a stable, client-friendly JSON shape.
#[derive(Debug, Serialize)]
pub struct ApiException {
    /// Python exception type name, e.g. `"ValueError"`.
    #[serde(rename = "type")]
    pub exc_type: String,
    /// The exception message, if any.
    pub message: Option<String>,
    /// `"Type: message"`, matching Python's one-line summary.
    pub summary: String,
    /// Structured stack frames, outermost first.
    pub traceback: Vec<ApiFrame>,
    /// The full, Python-style formatted traceback.
    pub traceback_text: String,
}

/// One frame of a structured traceback.
#[derive(Debug, Serialize)]
pub struct ApiFrame {
    pub filename: String,
    /// Function name, or `null` for module-level code.
    pub function: Option<String>,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// The source line for this frame, if monty captured a preview.
    pub source: Option<String>,
}

impl From<&MontyException> for ApiException {
    fn from(exc: &MontyException) -> Self {
        let exc_type: &'static str = exc.exc_type().into();
        ApiException {
            exc_type: exc_type.to_owned(),
            message: exc.message().map(str::to_owned),
            summary: exc.summary(),
            traceback: exc.traceback().iter().map(ApiFrame::from).collect(),
            traceback_text: exc.to_string(),
        }
    }
}

impl From<&monty::StackFrame> for ApiFrame {
    fn from(frame: &monty::StackFrame) -> Self {
        ApiFrame {
            filename: frame.filename.clone(),
            function: frame.frame_name.clone(),
            line: frame.start.line,
            column: frame.start.column,
            end_line: frame.end.line,
            end_column: frame.end.column,
            source: frame.preview_line.clone(),
        }
    }
}

/// Builds a monty exception from a client-supplied `{type, message}` pair, for
/// raising an exception back into a paused session.
///
/// # Errors
///
/// Returns [`ConvertError`] if `type` is not a known Python exception name.
pub fn exception_from_parts(
    exc_type: &str,
    message: Option<String>,
) -> Result<MontyException, ConvertError> {
    let parsed = exc_type
        .parse()
        .map_err(|_| ConvertError::new(format!("unknown exception type {exc_type:?}")))?;
    Ok(MontyException::new(parsed, message))
}
