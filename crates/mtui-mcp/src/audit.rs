//! Durable audit record of `mtui-mcp` tool calls (#411).
//!
//! `mtui-mcp` executes maintenance actions on shared infrastructure; when the
//! serving process goes away, stderr diagnostics and the in-memory `show_log`
//! buffers go with it. This module is the file sink for the one record per
//! call the server layer writes around its `call_tool` dispatch.
//!
//! Design notes (settled for #411):
//!
//! * The seam sits in `mtui-mcp`'s `call_tool` dispatch, not in `mtui-core`'s:
//!   `mtui-core` has no notion of a session key or a transport, and pushing
//!   one down there purely to serve the MCP case would be the tail wagging
//!   the dog. The REPL is therefore not covered by this record.
//! * Fail mode is **refuse**: when the sink cannot be written the call is
//!   refused instead of proceeding unrecorded. The pre-flight check refuses
//!   before dispatching; a post-dispatch write failure refuses in place of
//!   the result. Terminal records of background jobs are the exception — the
//!   dispatch already answered, so a failed terminal write only warns.
//! * Secrets are **unrepresentable**, not filtered: [`sanitize_args`] never
//!   records a `config_set` value at all, so a future secret attribute cannot
//!   leak by forgetting to extend the classifier.
//!
//! Record schema (versioned by [`AUDIT_SCHEMA_VERSION`], one object per line;
//! `ts` is epoch millis taken when the call arrives, so timestamps order
//! causally even when a terminal record is appended before its cancel call's
//! own record):
//!
//! * `call`: a foreground call — `v`, `ts` (epoch millis), `session`, `tool`,
//!   `args`, `outcome` (`ok`/`error`/`unknown-tool`), `duration_ms`, `rrids`,
//!   `hosts`.
//! * `dispatch`: a backgrounded start — as `call`, plus the started `job_ids`.
//! * `terminal`: a background job reaching Done/Failed/Cancelled — `v`, `ts`,
//!   `session`, `tool`, `job_id`, `job_state`, `outcome`, `duration_ms`,
//!   `rrids`, `hosts`. No `args`: the dispatch record already carries them,
//!   and omitting them keeps a secret out of reach by construction.

use std::io;
use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use serde_json::{Map, Value};

/// Schema version stamped on every record as `v`.
pub(crate) const AUDIT_SCHEMA_VERSION: u32 = 1;

/// Marker serialised in place of a `config_set` value.
pub(crate) const REDACTED: &str = "<redacted>";

/// The `outcome` token of a `call`/`dispatch` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditOutcome {
    Ok,
    Error,
    UnknownTool,
}

impl AuditOutcome {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Error => "error",
            AuditOutcome::UnknownTool => "unknown-tool",
        }
    }
}

/// The `event` token distinguishing a foreground call from a backgrounded
/// job's dispatch record and its terminal-state record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditEvent {
    Call,
    Dispatch,
    Terminal,
}

impl AuditEvent {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            AuditEvent::Call => "call",
            AuditEvent::Dispatch => "dispatch",
            AuditEvent::Terminal => "terminal",
        }
    }
}

/// The versioned JSONL sink behind `[mcp] audit_log`.
///
/// Opened per record with `O_APPEND` (never truncated, so entries survive a
/// server restart) and `0600` (the file outlives the session and may name
/// consequential actions). Per-record open keeps concurrent sessions and
/// background workers from sharing a file offset.
#[derive(Debug, Clone)]
pub(crate) struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Open the sink for append, creating it with restrictive permissions.
    fn open_sink(&self) -> io::Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }

    /// Pre-flight check: fail when the sink cannot be opened for append, so
    /// the caller can refuse before dispatching.
    ///
    /// # Errors
    ///
    /// The underlying I/O error (missing parent, a directory as path,
    /// permission denied).
    pub(crate) fn check_writable(&self) -> io::Result<()> {
        self.open_sink().map(|_| ())
    }

    /// Append one record as a single JSON line, fsynced before returning so
    /// the response the caller sends next cannot race the record.
    ///
    /// # Errors
    ///
    /// The underlying I/O or serialisation error; nothing is retried.
    pub(crate) fn append(&self, record: &Value) -> io::Result<()> {
        use io::Write as _;
        let mut file = self.open_sink()?;
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.sync_all()?;
        Ok(())
    }
}

/// Current time as whole milliseconds since the unix epoch, for `ts`.
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// The refusal returned in place of a call's result when the sink cannot be
/// written: the call does not proceed unrecorded.
pub(crate) fn refuse_error(source: &io::Error) -> McpError {
    McpError::internal_error(
        format!("audit log unavailable ({source}): call refused"),
        None,
    )
}

/// Redact `kwargs` for the audit record.
///
/// Every tool's arguments are recorded verbatim, except `config_set` — the
/// one tool that can carry a credential. Its `value` is never recorded, for
/// any attribute: the record type has no room for it, so adding a future
/// secret attribute cannot leak it by forgetting to extend the classifier.
/// [`is_secret_attr`](mtui_core::commands::is_secret_attr) still marks whether
/// the attribute *is* a secret, so a reader can tell a token rotation from a
/// display-name change.
pub(crate) fn sanitize_args(tool: &str, kwargs: &Map<String, Value>) -> Value {
    if tool != "config_set" {
        return Value::Object(kwargs.clone());
    }
    let attribute = kwargs
        .get("attribute")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut redacted = Map::with_capacity(3);
    redacted.insert("attribute".to_owned(), Value::String(attribute.to_owned()));
    redacted.insert("value".to_owned(), Value::String(REDACTED.to_owned()));
    redacted.insert(
        "secret".to_owned(),
        Value::Bool(mtui_core::commands::is_secret_attr(attribute)),
    );
    Value::Object(redacted)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use serde_json::json;

    /// Append through a fresh handle and read the lines back.
    fn read_lines(path: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(path).expect("sink readable");
        text.lines()
            .map(|line| serde_json::from_str(line).expect("one object per line"))
            .collect()
    }

    #[test]
    fn sink_is_created_0600_and_appends_across_restarts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");

        // Two instances over the same path model a server restart: the sink
        // is opened per record, never truncated.
        AuditLog::new(path.clone())
            .append(&json!({"v": 1, "tool": "whoami"}))
            .expect("first append");
        AuditLog::new(path.clone())
            .append(&json!({"v": 1, "tool": "run"}))
            .expect("second append");

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2, "restart must append, never truncate");
        assert_eq!(lines[0]["tool"], json!("whoami"));
        assert_eq!(lines[1]["tool"], json!("run"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "sink holds consequential actions");
        }
    }

    #[test]
    fn sink_preserves_pre_existing_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "{\"v\":1,\"tool\":\"old\"}\n").expect("seed");
        AuditLog::new(path.clone())
            .append(&json!({"v": 1, "tool": "new"}))
            .expect("append");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["tool"], json!("old"));
        assert_eq!(lines[1]["tool"], json!("new"));
    }

    #[test]
    fn sink_on_a_directory_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = AuditLog::new(dir.path().to_path_buf());
        assert!(
            sink.check_writable().is_err(),
            "pre-flight must fail on a directory"
        );
        assert!(
            sink.append(&json!({"v": 1})).is_err(),
            "append must fail on a directory"
        );
    }

    #[test]
    fn tokens_are_pinned() {
        assert_eq!(AUDIT_SCHEMA_VERSION, 1);
        assert_eq!(AuditOutcome::Ok.as_str(), "ok");
        assert_eq!(AuditOutcome::Error.as_str(), "error");
        assert_eq!(AuditOutcome::UnknownTool.as_str(), "unknown-tool");
        assert_eq!(AuditEvent::Call.as_str(), "call");
        assert_eq!(AuditEvent::Dispatch.as_str(), "dispatch");
        assert_eq!(AuditEvent::Terminal.as_str(), "terminal");
    }

    #[test]
    fn sanitize_config_set_leaves_no_trace_of_a_secret_value() {
        let secret = "gitea-token-value-9f8e7d6c";
        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"attribute": "gitea_token", "value": secret}))
                .expect("object");
        let args = sanitize_args("config_set", &kwargs);
        let raw = serde_json::to_string(&args).expect("serialisable");
        assert!(
            !raw.contains(secret),
            "secret value must be unrepresentable: {raw}"
        );
        assert_eq!(args["attribute"], json!("gitea_token"));
        assert_eq!(args["value"], json!(REDACTED));
        assert_eq!(args["secret"], json!(true));
    }

    #[test]
    fn sanitize_config_set_redacts_non_secret_values_too() {
        // Future-proofing pin: a new secret attribute that misses the
        // classifier still cannot leak, because no `config_set` value is ever
        // recorded.
        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"attribute": "session_user", "value": "alice"}))
                .expect("object");
        let args = sanitize_args("config_set", &kwargs);
        let raw = serde_json::to_string(&args).expect("serialisable");
        assert!(!raw.contains("alice"), "no value is recorded: {raw}");
        assert_eq!(args["secret"], json!(false));
    }

    #[test]
    fn sanitize_other_tools_pass_arguments_through() {
        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"command": ["true"]})).expect("object");
        assert_eq!(sanitize_args("run", &kwargs), Value::Object(kwargs.clone()));
    }
}
