//! Argument and schema helpers shared by the hand-written tool families.

use serde_json::{Map, Value, json};

use crate::session::McpCommandError;
use crate::tools::ToolDescriptor;

/// Shared "which template" note appended to every template-scoped tool.
pub(crate) const TEMPLATE_NOTE: &str = "Pass `template=<rrid>` to target a specific loaded template; required when more \
     than one is loaded.";

pub(crate) fn refuse(msg: impl Into<String>) -> McpCommandError {
    McpCommandError {
        stdout: String::new(),
        stderr: msg.into(),
        exit_code: 1,
    }
}

/// Build the strict (unknown fields rejected) schema for a tool.
pub(crate) fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Map<String, Value> {
    let mut properties = Map::new();
    for (name, spec) in props {
        properties.insert(name.to_owned(), spec);
    }
    let mut s = Map::new();
    s.insert("type".to_owned(), Value::String("object".to_owned()));
    s.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        s.insert(
            "required".to_owned(),
            Value::Array(required.iter().map(|r| json!(r)).collect()),
        );
    }
    s.insert("additionalProperties".to_owned(), Value::Bool(false));
    s
}

pub(crate) fn template_prop() -> Value {
    json!({
        "type": "string",
        "description": "RRID of a loaded template to target (required when >1 loaded).",
    })
}

/// `Some(value)` for a present string field, `None` when absent/null.
pub(crate) fn opt_str<'a>(
    kwargs: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, McpCommandError> {
    match kwargs.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(other) => Err(refuse(format!("{key} must be a string, got {other}"))),
    }
}

/// Reject any tool-call kwarg not in `allowed`, mirroring the strict
/// `additionalProperties: false` the advertised schema carries.
///
/// The runtime half for clients that do not validate: without it a misspelled
/// field (`temlate=`, `mesage=`) is silently dropped and the command runs with
/// it missing. Keys are reported sorted for a deterministic message.
pub(crate) fn reject_unknown_kwargs<'a>(
    kwargs: &Map<String, Value>,
    allowed: impl IntoIterator<Item = &'a str>,
) -> Result<(), McpCommandError> {
    let allowed: std::collections::BTreeSet<&str> = allowed.into_iter().collect();
    let mut unknown: Vec<&str> = kwargs
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    let names = unknown.join(", ");
    Err(McpCommandError {
        stdout: String::new(),
        stderr: format!("unknown argument(s): {names}"),
        exit_code: 1,
    })
}

/// [`reject_unknown_kwargs`] against the advertised properties of the
/// descriptor named `name`, so the check cannot drift from the tool's schema.
pub(crate) fn reject_unknown_for(
    kwargs: &Map<String, Value>,
    descriptors: Vec<ToolDescriptor>,
    name: &str,
) -> Result<(), McpCommandError> {
    let Some(desc) = descriptors.iter().find(|d| d.name == name) else {
        return Ok(());
    };
    let allowed = desc
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|props| props.keys().map(String::as_str));
    reject_unknown_kwargs(kwargs, allowed)
}
