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
//!   leak by forgetting to extend the classifier. File-body payloads (`put`
//!   `content`/`content_b64`, `testreport_write` `content`, `testreport_patch`
//!   `replacement`) never land verbatim either: each records `{bytes, sha256}`
//!   over the original string, so a credentials file or SSH key uploaded via
//!   `put` is correlatable without being persisted.
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
use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::ErrorData as McpError;
use serde_json::{Map, Value};

/// Schema version stamped on every record as `v`.
pub(crate) const AUDIT_SCHEMA_VERSION: u32 = 1;

/// Marker serialised in place of a `config_set` value.
pub(crate) const REDACTED: &str = "<redacted>";

/// In-record-only string cap (chars): tool/id/arg strings longer than this
/// are truncated for the record; dispatch uses the original.
pub(crate) const MAX_AUDIT_STRING_LEN: usize = 1024;
/// Size-only reduction thresholds: longer arrays/objects become `{"_len": n}`.
pub(crate) const MAX_AUDIT_ARRAY_LEN: usize = 100;
pub(crate) const MAX_AUDIT_OBJECT_KEYS: usize = 100;

/// Process-global audit sequence: file-first, OTLP second, same `seq` on
/// both so the two streams join. Only consumed when auditing is on.
static NEXT_AUDIT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Test-only blocking delay (millis) injected at the top of [`AuditLog::open_sink`].
///
/// Lets the slow-sink test prove the async dispatch never blocks on a down/slow
/// disk: the sleep runs wherever `open_sink` runs, so inline dispatch stalls the
/// worker while `*_async` sleeps on the blocking pool. Gated on a `slow-sink`
/// path fragment so concurrent tests on other temp paths never observe it.
#[cfg(test)]
pub(crate) static AUDIT_TEST_DELAY_MS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_seq() -> u64 {
    NEXT_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Truncate to [`MAX_AUDIT_STRING_LEN`] chars on a char boundary.
pub(crate) fn cap_str(raw: &str) -> String {
    if raw.chars().count() <= MAX_AUDIT_STRING_LEN {
        return raw.to_owned();
    }
    raw.chars().take(MAX_AUDIT_STRING_LEN).collect()
}

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
    ///
    /// Blocking (open/chmod): async callers must go through
    /// [`check_writable_async`](Self::check_writable_async) /
    /// [`append_async`](Self::append_async), never call this inline.
    fn open_sink(&self) -> io::Result<std::fs::File> {
        #[cfg(test)]
        {
            if AUDIT_TEST_DELAY_MS.load(Ordering::Relaxed) > 0
                && self.path.to_string_lossy().contains("slow-sink")
            {
                std::thread::sleep(std::time::Duration::from_millis(
                    AUDIT_TEST_DELAY_MS.load(Ordering::Relaxed),
                ));
            }
        }
        // Atomically restrictive create: mode 0600 applies at creation, so a
        // new sink is never visible with umask-derived group/other bits in
        // the window before the hardening below. Mode is masked by the umask,
        // which can only remove bits from 0600, never add.
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&self.path)?
        };
        #[cfg(not(unix))]
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // Harden a pre-existing sink (created by an older release or by hand)
        // via the open fd, not the path, so a swapped symlink cannot divert it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(file)
    }

    /// Pre-flight check: fail when the sink cannot be opened for append, so
    /// the caller can refuse before dispatching.
    ///
    /// Blocking; async dispatch uses [`check_writable_async`](Self::check_writable_async).
    ///
    /// # Errors
    ///
    /// The underlying I/O error (missing parent, a directory as path,
    /// permission denied).
    pub(crate) fn check_writable(&self) -> io::Result<()> {
        self.open_sink().map(|_| ())
    }

    /// [`check_writable`](Self::check_writable) on the blocking pool, so a
    /// down/slow disk never stalls the dispatch worker.
    pub(crate) async fn check_writable_async(&self) -> io::Result<()> {
        let owned = self.clone();
        tokio::task::spawn_blocking(move || owned.check_writable())
            .await
            .map_err(io::Error::other)?
    }

    /// Append one record as a single JSON line, fsynced before returning so
    /// the response the caller sends next cannot race the record.
    ///
    /// Blocking; async dispatch uses [`append_async`](Self::append_async).
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

    /// [`append`](Self::append) on the blocking pool: per-record
    /// open/chmod/write/fsync never runs inline in async dispatch.
    pub(crate) async fn append_async(&self, record: Value) -> io::Result<()> {
        let owned = self.clone();
        tokio::task::spawn_blocking(move || owned.append(&record))
            .await
            .map_err(io::Error::other)?
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

/// Recursively bound an argument value for the record: strings cap at
/// [`MAX_AUDIT_STRING_LEN`] chars, arrays/objects beyond their thresholds
/// reduce to `{"_len": n}`, and object keys longer than the string cap are
/// relocated to `_overlong_keys` (truncated names, values dropped to bound
/// size). Small values pass through unchanged.
pub(crate) fn sanitize_value(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(cap_str(&s)),
        Value::Array(items) => {
            if items.len() > MAX_AUDIT_ARRAY_LEN {
                return serde_json::json!({"_len": items.len()});
            }
            Value::Array(items.into_iter().map(sanitize_value).collect())
        }
        Value::Object(map) => {
            let total = map.len();
            if total > MAX_AUDIT_OBJECT_KEYS {
                return serde_json::json!({"_len": total});
            }
            let mut out = Map::with_capacity(map.len() + 1);
            let mut overlong = Vec::new();
            for (key, val) in map {
                if key.chars().count() > MAX_AUDIT_STRING_LEN {
                    overlong.push(Value::String(cap_str(&key)));
                } else {
                    out.insert(key, sanitize_value(val));
                }
            }
            if !overlong.is_empty() {
                out.insert("_overlong_keys".to_owned(), Value::Array(overlong));
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// Redact `kwargs` for the audit record.
///
/// Every tool's arguments are recorded through [`sanitize_value`] (caps,
/// size-only reduction, overlong-key relocation), except the two shapes that
/// can carry secrets in bulk:
///
/// * `config_set` — the one tool that can carry a credential. Its `value` is
///   never recorded, for any attribute: the record type has no room for it,
///   so adding a future secret attribute cannot leak it by forgetting to
///   extend the classifier. [`is_secret_attr`](mtui_core::commands::is_secret_attr)
///   still marks whether the attribute *is* a secret, so a reader can tell a
///   token rotation from a display-name change.
/// * File-body payloads — `put` `content`/`content_b64`, `testreport_write`
///   `content`, `testreport_patch` `replacement`. Each records
///   `{bytes, sha256}` over the original string: correlatable across the file
///   and OTLP bodies without persisting a credentials file or SSH key.
///   Always fingerprinted, never verbatim regardless of size, so small secrets
///   cannot leak by staying under a threshold.
pub(crate) fn sanitize_args(tool: &str, kwargs: &Map<String, Value>) -> Value {
    if tool == "config_set" {
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
        return Value::Object(redacted);
    }
    if let Some(payload_keys) = payload_keys_for(tool) {
        let mut out = Map::with_capacity(kwargs.len());
        for (key, val) in kwargs {
            if payload_keys.contains(&key.as_str()) {
                out.insert(key.clone(), fingerprint_value(val));
            } else {
                out.insert(key.clone(), sanitize_value(val.clone()));
            }
        }
        return Value::Object(out);
    }
    sanitize_value(Value::Object(kwargs.clone()))
}

/// File-body payload keys fingerprinted per tool, never recorded verbatim.
fn payload_keys_for(tool: &str) -> Option<&'static [&'static str]> {
    match tool {
        "put" => Some(&["content", "content_b64"]),
        "testreport_write" => Some(&["content"]),
        "testreport_patch" => Some(&["replacement"]),
        _ => None,
    }
}

/// Fingerprint one payload value as `{bytes, sha256}`.
///
/// Strings hash as their raw bytes (`bytes` is the byte length); any other
/// JSON shape hashes as its canonical encoding, so a mistyped payload still
/// cannot leak verbatim.
fn fingerprint_value(value: &Value) -> Value {
    use sha2::{Digest as _, Sha256};
    let raw: Vec<u8> = match value {
        Value::String(s) => s.as_bytes().to_vec(),
        other => serde_json::to_vec(other).unwrap_or_default(),
    };
    let mut hasher = Sha256::new();
    hasher.update(&raw);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    serde_json::json!({"bytes": raw.len(), "sha256": hex})
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
    fn sink_tightens_a_pre_existing_loose_sink_to_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "{\"v\":1}\n").expect("seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("loosen seed");
        }
        AuditLog::new(path.clone())
            .append(&json!({"v": 1, "tool": "new"}))
            .expect("append");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "pre-existing sink hardened");
        }
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

    #[test]
    fn fingerprint_is_a_known_sha256_vector() {
        // Known-answer pin, not a rehash of the impl: sha256("abc").
        let out = fingerprint_value(&json!("abc"));
        assert_eq!(out["bytes"], json!(3));
        assert_eq!(
            out["sha256"],
            json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn sanitize_put_never_records_payload_verbatim() {
        let secret = "-----BEGIN OPENSSH PRIVATE KEY----\nSECRET-KEY-MATERIAL\n-----END OPENSSH PRIVATE KEY-----";
        for kwargs in [
            json!({"filename": "id_rsa", "content": secret}),
            json!({"filename": "id_rsa", "content_b64": secret}),
        ] {
            let kwargs: Map<String, Value> = serde_json::from_value(kwargs).expect("object");
            let args = sanitize_args("put", &kwargs);
            let raw = serde_json::to_string(&args).expect("serialisable");
            assert!(
                !raw.contains("SECRET-KEY-MATERIAL"),
                "payload fingerprinted: {raw}"
            );
            let key = if kwargs.contains_key("content") {
                "content"
            } else {
                "content_b64"
            };
            assert_eq!(args[key]["bytes"], json!(secret.len()));
            assert_eq!(args[key]["sha256"].as_str().expect("hex").len(), 64);
            assert_eq!(args["filename"], json!("id_rsa"));
        }
    }

    #[test]
    fn sanitize_testreport_bodies_are_fingerprinted_not_stored() {
        let body = "SECRET-CREDENTIALS-FILE-BODY";
        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"content": body, "relpath": "log"})).expect("object");
        let args = sanitize_args("testreport_write", &kwargs);
        let raw = serde_json::to_string(&args).expect("serialisable");
        assert!(!raw.contains(body), "write body fingerprinted: {raw}");
        assert_eq!(args["content"]["bytes"], json!(body.len()));
        assert_eq!(args["relpath"], json!("log"));

        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"replacement": body, "start_line": 1})).expect("object");
        let args = sanitize_args("testreport_patch", &kwargs);
        let raw = serde_json::to_string(&args).expect("serialisable");
        assert!(!raw.contains(body), "patch body fingerprinted: {raw}");
        assert_eq!(args["replacement"]["bytes"], json!(body.len()));
        assert_eq!(args["start_line"], json!(1));
    }

    #[test]
    fn sanitize_small_payloads_are_fingerprinted_too() {
        // No reconstructability threshold: even a two-byte payload that could
        // be inlined safely records only its fingerprint, so a small secret
        // cannot leak by staying under a cap.
        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"filename": "f", "content": "hi"})).expect("object");
        let args = sanitize_args("put", &kwargs);
        let raw = serde_json::to_string(&args).expect("serialisable");
        assert!(!raw.contains("\"hi\""), "no verbatim even when tiny: {raw}");
        assert_eq!(args["content"]["bytes"], json!(2));
    }

    #[test]
    fn cap_str_truncates_on_char_boundary() {
        assert_eq!(cap_str("abc"), "abc");
        let long = "é".repeat(MAX_AUDIT_STRING_LEN + 10);
        let capped = cap_str(&long);
        assert_eq!(capped.chars().count(), MAX_AUDIT_STRING_LEN);
        // Anti-vacuity: the fixture really exceeds the cap.
        assert!(long.chars().count() > MAX_AUDIT_STRING_LEN);
    }

    #[test]
    fn sanitize_long_strings_cap_in_record_only() {
        let long = "x".repeat(MAX_AUDIT_STRING_LEN + 5);
        let kwargs: Map<String, Value> =
            serde_json::from_value(json!({"note": long})).expect("object");
        let args = sanitize_args("run", &kwargs);
        assert_eq!(
            args["note"].as_str().expect("string").chars().count(),
            MAX_AUDIT_STRING_LEN
        );
    }

    #[test]
    fn sanitize_oversize_array_reduces_to_len() {
        let big: Vec<Value> = (0..MAX_AUDIT_ARRAY_LEN + 1).map(|i| json!(i)).collect();
        let reduced = sanitize_value(Value::Array(big));
        assert_eq!(reduced, json!({"_len": MAX_AUDIT_ARRAY_LEN + 1}));
    }

    #[test]
    fn sanitize_oversize_object_reduces_to_len() {
        let mut map = Map::new();
        for i in 0..MAX_AUDIT_OBJECT_KEYS + 1 {
            map.insert(format!("k{i}"), json!(i));
        }
        assert_eq!(
            sanitize_value(Value::Object(map)),
            json!({"_len": MAX_AUDIT_OBJECT_KEYS + 1})
        );
    }

    #[test]
    fn sanitize_overlong_keys_relocate() {
        let overlong = "k".repeat(MAX_AUDIT_STRING_LEN + 1);
        let mut map = Map::new();
        map.insert(overlong.clone(), json!("v"));
        map.insert("ok".to_owned(), json!(1));
        let out = sanitize_value(Value::Object(map));
        assert_eq!(out["ok"], json!(1));
        assert!(out.get(&overlong).is_none(), "overlong key removed");
        let relocated = out["_overlong_keys"].as_array().expect("relocated");
        assert_eq!(relocated.len(), 1);
        assert_eq!(
            relocated[0].as_str().expect("name").chars().count(),
            MAX_AUDIT_STRING_LEN
        );
    }

    #[test]
    fn next_seq_is_monotonic() {
        let a = next_seq();
        let b = next_seq();
        assert!(b > a, "seq must increase: {a} -> {b}");
    }

    #[tokio::test]
    async fn async_wrappers_write_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let sink = AuditLog::new(path.clone());
        sink.check_writable_async().await.expect("writable");
        sink.append_async(json!({"v": 1, "tool": "whoami"}))
            .await
            .expect("append");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["tool"], json!("whoami"));
    }

    #[tokio::test]
    async fn async_wrappers_surface_sink_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = AuditLog::new(dir.path().to_path_buf());
        assert!(
            sink.check_writable_async().await.is_err(),
            "pre-flight must fail on a directory"
        );
        assert!(
            sink.append_async(json!({"v": 1})).await.is_err(),
            "append must fail on a directory"
        );
    }
}
