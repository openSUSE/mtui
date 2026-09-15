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
//! * Durable and append-only, but **not tamper-evident**: there is no hash
//!   chain and no HMAC, so anyone writing as the sink's owner can rewrite
//!   history undetected. Tamper evidence means shipping each record off-host
//!   as it lands (OTLP). The sink is opened `O_NOFOLLOW`, which covers the
//!   path's final component only, so the sink's *directory* must be writable
//!   by the serving user alone.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// Rotated generations kept beside the live sink (`<path>.1` .. `<path>.5`).
/// The oldest is discarded on each rotation; archiving beyond it is the
/// operator's job.
pub(crate) const AUDIT_LOG_KEEP: u32 = 5;

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
/// consequential actions), and vetted on the resulting fd — see
/// [`open_vetted`](Self::open_vetted). Per-record open keeps concurrent
/// sessions and background workers from sharing a file offset. At
/// `max_bytes` the sink rotates through [`AUDIT_LOG_KEEP`] generations; see
/// [`rotate`](Self::rotate).
#[derive(Debug, Clone)]
pub(crate) struct AuditLog {
    path: PathBuf,
    /// `[mcp] audit_log_max_bytes`; `0` disables rotation.
    max_bytes: u64,
}

impl AuditLog {
    pub(crate) fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self { path, max_bytes }
    }

    /// Open the sink for append, creating it with restrictive permissions.
    ///
    /// Blocking (open/stat/chmod): async callers must go through
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
        let file = self.open_vetted()?;
        if self.max_bytes == 0 || file.metadata()?.len() < self.max_bytes {
            return Ok(file);
        }
        Ok(self.rotate(file))
    }

    /// `<path>.<n>`, the nth rotated generation.
    fn generation(&self, n: u32) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    /// Shift the generations down and hand back a live sink.
    ///
    /// **Infallible by policy, and it never waits.** The cap is housekeeping;
    /// the record is the point. So every way this can go wrong — a lock another
    /// writer holds, a mount whose `flock` is unimplemented (an NFS export
    /// without `lockd` answers `ENOLCK`), a rename that cannot happen — warns
    /// and hands back the oversize file to append to. An `Err` escaping here
    /// would reach [`refuse_error`] and turn a directory left at `<path>.1`, or
    /// one unlucky mount, into a refusal of every mutating call past the cap
    /// and a server that will not start: the outage the cap exists to prevent.
    ///
    /// The advisory lock is taken on the **oversize file itself**, not on a
    /// process-local mutex: two stdio servers pointed at one path are two
    /// processes, and only a lock on the inode orders them. It is a `try_lock`,
    /// never a blocking `lock` — whoever holds it rotates, and whoever does not
    /// appends past the cap and rotates on a later call, so a contended
    /// boundary costs the dispatch worker nothing and still rotates once.
    fn rotate(&self, full: std::fs::File) -> std::fs::File {
        match full.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                warn_rotation_failed("another writer holds the sink lock");
                return full;
            }
            Err(std::fs::TryLockError::Error(err)) => {
                warn_rotation_failed(err);
                return full;
            }
        }
        // Re-open the path under the lock: a different inode means someone else
        // rotated first, so use theirs rather than rotate a second time.
        let current = match self.open_vetted() {
            Ok(current) => current,
            Err(err) => {
                warn_rotation_failed(err);
                return full;
            }
        };
        match same_file(&full, &current) {
            Ok(false) => return current,
            Ok(true) => drop(current),
            Err(err) => {
                warn_rotation_failed(err);
                return full;
            }
        }
        // A failed shift leaves the chain half-moved, so stop at the first one
        // rather than reporting the same outage four more times.
        for n in (1..AUDIT_LOG_KEEP).rev() {
            let from = self.generation(n);
            if from.exists() && !renamed(&from, &self.generation(n + 1)) {
                return full;
            }
        }
        if !renamed(&self.path, &self.generation(1)) {
            return full;
        }
        match self.open_vetted() {
            Ok(fresh) => {
                ROTATION_WARNED.store(false, Ordering::Relaxed);
                fresh
            }
            // The live sink is `<path>.1` now, so `full` is where this record
            // lands: the documented boundary line, one file early.
            Err(err) => {
                warn_rotation_failed(err);
                full
            }
        }
    }

    /// Open the sink and refuse anything that is not this user's own regular
    /// file, before a single byte or permission bit is written to it.
    ///
    /// `O_NOFOLLOW` makes a planted symlink fail rather than divert the append
    /// — and the `0600` tightening below — onto whatever it names.
    /// `O_NONBLOCK` makes a planted FIFO fail `ENXIO` instead of blocking
    /// `open(2)` forever on the blocking pool; a regular file ignores it.
    /// `O_CLOEXEC` keeps the fd out of every subprocess mtui spawns.
    ///
    /// Mode `0600` at creation is what makes the create atomically restrictive:
    /// a new sink is never visible with umask-derived group/other bits in the
    /// window before the tightening. The umask can only remove bits from
    /// `0600`, never add.
    #[cfg(unix)]
    fn open_vetted(&self) -> io::Result<std::fs::File> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&self.path)?;
        vet_sink(&file.metadata()?, nix::unistd::geteuid().as_raw())?;
        // Harden a pre-existing sink (created by an older release or by hand)
        // through the open fd, not the path — and only now that the fd is known
        // to be our own regular file, so we never chmod a file we did not make.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }

    /// Non-unix fallback: no `O_NOFOLLOW`/owner notion to vet against.
    #[cfg(not(unix))]
    fn open_vetted(&self) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }

    /// Pre-flight check: fail when the sink cannot be opened for append, so
    /// the caller can refuse before dispatching.
    ///
    /// Opens the sink exactly as an append does, so a sink already at the cap
    /// rotates here too — [`rotate`](Self::rotate) cannot fail, so that adds
    /// nothing to the errors below.
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

/// Whether two open handles name the same file.
///
/// The rotation race check. Off unix there is no inode to compare, so it
/// degrades to "assume nobody rotated underneath us" — the same answer the
/// single-process case gives.
#[cfg(unix)]
fn same_file(a: &std::fs::File, b: &std::fs::File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(a.metadata()?.ino() == b.metadata()?.ino())
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::File, _b: &std::fs::File) -> io::Result<bool> {
    Ok(true)
}

/// Set while a rotation outage is already reported.
///
/// A sink that cannot be rotated stays that way, and the cap is re-tested on
/// every append past it, so an unlatched warning would arrive once per mutating
/// call for the life of the server — burying the log it exists to protect.
/// Cleared by a rotation that completes, so the next outage is reported again.
static ROTATION_WARNED: AtomicBool = AtomicBool::new(false);

/// Report a rotation failure, at most once per outage.
///
/// Closed text: the reason, never the path — the line also rides the OTLP
/// diagnostics queue.
fn warn_rotation_failed(reason: impl std::fmt::Display) {
    if ROTATION_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::warn!(
        reason = %reason,
        "audit log: rotation failed; appending past the cap"
    );
}

/// Rename one generation; `false` when it did not happen.
fn renamed(from: &std::path::Path, to: &std::path::Path) -> bool {
    match std::fs::rename(from, to) {
        Ok(()) => true,
        Err(err) => {
            warn_rotation_failed(err);
            false
        }
    }
}

/// Refuse an opened sink that is not a regular file owned by this process.
///
/// Pure, so the boundary is testable without planting a device or a foreign
/// file. `O_NOFOLLOW` covers only the path's final component, so these two
/// checks close what is left of it: a planted FIFO or device node, and a file
/// another user owns — which is also the reachable hardlink variant, since a
/// link to someone else's file keeps their uid.
///
/// A *same-uid* hardlink is deliberately allowed. It crosses no boundary, and
/// refusing it (an `nlink > 1` check) would turn an operator's
/// `ln audit.jsonl backup` into an outage.
#[cfg(unix)]
fn vet_sink(meta: &std::fs::Metadata, euid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "audit sink is not a regular file",
        ));
    }
    if meta.uid() != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "audit sink is owned by another user",
        ));
    }
    Ok(())
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
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0x0f) as usize] as char);
    }
    serde_json::json!({"bytes": raw.len(), "sha256": hex})
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use serde_json::json;
    use serial_test::serial;

    use crate::test_log::capture_logs_blocking;

    /// The one captured warning starting with `prefix`, in full; required to be
    /// unique, so a warning emitted once per call cannot pass as one per
    /// outage.
    fn warn_line(logs: &str, prefix: &str) -> String {
        let mut hits = logs.lines().filter(|l| l.starts_with(prefix));
        let line = hits
            .next()
            .unwrap_or_else(|| panic!("no {prefix:?} warning was captured, got: {logs:?}"));
        assert!(
            hits.next().is_none(),
            "exactly one {prefix:?} warning: {logs:?}"
        );
        line.to_owned()
    }

    /// Re-arm the process-global one-warning-per-outage latch. Every test that
    /// reads or moves it runs `#[serial(audit_rotation)]`.
    fn rearm_rotation_warning() {
        ROTATION_WARNED.store(false, Ordering::Relaxed);
    }

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
        AuditLog::new(path.clone(), 0)
            .append(&json!({"v": 1, "tool": "whoami"}))
            .expect("first append");
        AuditLog::new(path.clone(), 0)
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
        AuditLog::new(path.clone(), 0)
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
        AuditLog::new(path.clone(), 0)
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

    #[cfg(unix)]
    #[test]
    fn sink_refuses_to_follow_a_symlink_and_leaves_its_target_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("victim.txt");
        std::fs::write(&target, "VICTIM\n").expect("seed");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .expect("loosen target");
        let link = dir.path().join("audit.jsonl");
        std::os::unix::fs::symlink(&target, &link).expect("plant symlink");

        let err = AuditLog::new(link, 0)
            .append(&json!({"v": 1, "tool": "run"}))
            .expect_err("a planted symlink must not be followed");
        // `io::ErrorKind::FilesystemLoop` is still unstable, so pin the errno
        // `O_NOFOLLOW` raises on a symlink directly.
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));

        assert_eq!(
            std::fs::read_to_string(&target).expect("target readable"),
            "VICTIM\n",
            "the symlink's target must not be appended to"
        );
        let mode = std::fs::metadata(&target)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "the target's mode must not be tightened");
    }

    #[cfg(unix)]
    #[test]
    fn sink_refuses_a_dangling_symlink_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nowhere.jsonl");
        let link = dir.path().join("audit.jsonl");
        std::os::unix::fs::symlink(&missing, &link).expect("plant symlink");

        let err = AuditLog::new(link, 0)
            .append(&json!({"v": 1}))
            .expect_err("a dangling symlink must not be created through");
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP));
        assert!(
            !missing.exists(),
            "nothing was created at the link's target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sink_refuses_a_planted_fifo_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600))
            .expect("plant fifo");

        // Without `O_NONBLOCK`, `open(2)` on a reader-less FIFO blocks forever,
        // and in production it runs on the blocking pool — so the failure mode
        // is a wedged worker, not an error. Probe it off-thread with a deadline;
        // a blocked probe thread simply never answers.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(AuditLog::new(path, 0).append(&json!({"v": 1})).is_err());
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("opening a planted FIFO must fail, not block");
        assert!(refused, "a FIFO is not a regular file");
    }

    #[cfg(unix)]
    #[test]
    fn sink_appends_through_a_same_user_hardlink() {
        // The boundary the owner check draws. A link the serving user made to
        // its own file crosses nothing, so refusing it (an `nlink > 1` check)
        // would turn an operator's `ln audit.jsonl backup` into an outage.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "").expect("seed");
        let link = dir.path().join("backup.jsonl");
        std::fs::hard_link(&path, &link).expect("hardlink");

        AuditLog::new(link, 0)
            .append(&json!({"v": 1, "tool": "run"}))
            .expect("a same-user hardlink still appends");
        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1, "the record landed on the shared inode");
        assert_eq!(lines[0]["tool"], json!("run"));
    }

    #[cfg(unix)]
    #[test]
    fn vet_sink_refuses_a_non_regular_file_and_a_foreign_owner() {
        use std::os::unix::fs::MetadataExt as _;

        let meta = std::fs::metadata("/dev/null").expect("/dev/null");
        let euid = nix::unistd::geteuid().as_raw();
        let err = vet_sink(&meta, euid).expect_err("a device is not a regular file");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "audit sink is not a regular file");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        std::fs::write(&path, "").expect("seed");
        let meta = std::fs::metadata(&path).expect("metadata");
        // Creating a foreign-owned file needs `CAP_CHOWN`, so the honest way to
        // exercise the check is to move the expected uid instead.
        let err = vet_sink(&meta, meta.uid() + 1).expect_err("a foreign owner is refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(err.to_string(), "audit sink is owned by another user");
        vet_sink(&meta, meta.uid()).expect("our own regular file passes");
    }

    #[test]
    fn sink_on_a_directory_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sink = AuditLog::new(dir.path().to_path_buf(), 0);
        assert!(
            sink.check_writable().is_err(),
            "pre-flight must fail on a directory"
        );
        assert!(
            sink.append(&json!({"v": 1})).is_err(),
            "append must fail on a directory"
        );
    }

    /// `<path>.<n>`, the rotated generation the assertions read back.
    fn generation_path(path: &Path, n: u32) -> std::path::PathBuf {
        let mut name = path.to_path_buf().into_os_string();
        name.push(format!(".{n}"));
        std::path::PathBuf::from(name)
    }

    #[test]
    #[serial(audit_rotation)]
    fn sink_rotates_when_the_cap_is_reached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        // Cap 1: the first append lands on an empty file, the second finds it
        // over the cap and rotates before writing.
        let sink = AuditLog::new(path.clone(), 1);
        sink.append(&json!({"v": 1, "tool": "first"}))
            .expect("first");
        sink.append(&json!({"v": 1, "tool": "second"}))
            .expect("second");

        let rotated = generation_path(&path, 1);
        assert_eq!(read_lines(&rotated).len(), 1, "generation .1 holds line 1");
        assert_eq!(read_lines(&rotated)[0]["tool"], json!("first"));
        let live = read_lines(&path);
        assert_eq!(live.len(), 1, "the live sink restarts after rotation");
        assert_eq!(live[0]["tool"], json!("second"));
    }

    #[cfg(unix)]
    #[test]
    #[serial(audit_rotation)]
    fn rotated_generations_keep_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let sink = AuditLog::new(path.clone(), 1);
        sink.append(&json!({"v": 1})).expect("first");
        sink.append(&json!({"v": 1})).expect("second");

        let mode = std::fs::metadata(generation_path(&path, 1))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "a rotated generation is no less restricted");
    }

    #[test]
    #[serial(audit_rotation)]
    fn rotation_keeps_five_generations_and_no_more() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let sink = AuditLog::new(path.clone(), 1);
        for i in 0..7 {
            sink.append(&json!({"v": 1, "n": i})).expect("append");
        }

        for n in 1..=AUDIT_LOG_KEEP {
            assert!(
                generation_path(&path, n).exists(),
                "generation .{n} must exist"
            );
        }
        assert!(
            !generation_path(&path, AUDIT_LOG_KEEP + 1).exists(),
            "archival past .{AUDIT_LOG_KEEP} is the operator's job"
        );
        // Newest first: the live file holds the last append, .1 the one before.
        assert_eq!(read_lines(&path)[0]["n"], json!(6));
        assert_eq!(read_lines(&generation_path(&path, 1))[0]["n"], json!(5));
        assert_eq!(read_lines(&generation_path(&path, 5))[0]["n"], json!(1));
    }

    #[test]
    fn a_zero_cap_never_rotates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let sink = AuditLog::new(path.clone(), 0);
        for i in 0..4 {
            sink.append(&json!({"v": 1, "n": i})).expect("append");
        }
        assert_eq!(read_lines(&path).len(), 4, "everything stays in one file");
        assert!(
            !generation_path(&path, 1).exists(),
            "0 means unbounded, not 'rotate every time'"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(audit_rotation)]
    async fn concurrent_appends_over_the_cap_rotate_exactly_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let seed = json!({"v": 1, "tool": "seed"});
        std::fs::write(&path, format!("{seed}\n")).expect("seed over the cap");
        // The cap sits above one record and below the seed, so the rotated
        // sink is under the cap again: whichever writer opens the fresh file
        // must not find a second rotation waiting for it.
        let cap = std::fs::metadata(&path).expect("metadata").len();
        let sink = AuditLog::new(path.clone(), cap);

        rearm_rotation_warning();
        let (a, b) = tokio::join!(
            sink.append_async(json!({"v": 1, "tool": "a"})),
            sink.append_async(json!({"v": 1, "tool": "b"}))
        );
        a.expect("first append");
        b.expect("second append");

        let rotated = read_lines(generation_path(&path, 1).as_path());
        assert_eq!(rotated[0], seed, "the seeded file rotated: {rotated:?}");
        assert!(
            !generation_path(&path, 2).exists(),
            "the flock keeps the two rotations from becoming two shifts"
        );
        // The loser of the lock appends past the cap into the file the winner
        // then renames, so its line lands in `.1` — the documented boundary
        // case. Both records exist; which file holds which is not pinned.
        let live = read_lines(&path);
        let mut tools: Vec<&str> = rotated
            .iter()
            .chain(live.iter())
            .filter_map(|l| l["tool"].as_str())
            .filter(|t| *t != "seed")
            .collect();
        tools.sort_unstable();
        assert_eq!(tools, vec!["a", "b"], "neither record was lost");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial(audit_rotation)]
    async fn rotation_never_waits_for_a_lock_another_writer_holds() {
        // The advisory lock exists for two *processes* sharing one sink path,
        // which no same-process race can demonstrate: two threads reach the
        // rename microseconds apart and usually miss each other. So hold the
        // sink's lock from outside — the one state that is deterministic — and
        // pin what a contended sink costs the call, which is nothing: the
        // record still lands, past the cap, without a rotation and without a
        // wait. A blocking `lock` here parks the blocking pool until the holder
        // goes away, and dropping the lock altogether rotates under the holder.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        let seed = json!({"v": 1, "tool": "seed"});
        std::fs::write(&path, format!("{seed}\n")).expect("seed over the cap");
        let cap = std::fs::metadata(&path).expect("metadata").len();

        let holder = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("second writer opens the same inode");
        holder.lock().expect("hold the sink lock");

        let sink = AuditLog::new(path.clone(), cap);
        let append = tokio::spawn(async move { sink.append_async(json!({"v": 1})).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), append)
            .await
            .expect("a contended sink must not park the append")
            .expect("the appending task did not panic")
            .expect("the append lands while the lock is held");

        assert!(
            !generation_path(&path, 1).exists(),
            "nothing was rotated while another writer held the lock"
        );
        assert_eq!(
            read_lines(&path),
            vec![seed, json!({"v": 1})],
            "the record landed past the cap instead of being refused"
        );
        holder.unlock().expect("release");
    }

    /// Put a directory where the shift must rename a generation *to*.
    ///
    /// `<path>.5` is the one generation nothing shifts out of the way, so a
    /// directory there makes the shift's first step (`.4` -> `.5`) fail
    /// `EISDIR` however often it is retried.
    fn block_the_shift(path: &Path) {
        std::fs::write(generation_path(path, 4), "{\"v\":1,\"tool\":\"old\"}\n")
            .expect("a previous generation to shift");
        std::fs::create_dir(generation_path(path, 5)).expect("a directory in its way");
    }

    /// An over-cap sink whose rotation cannot rename anything. Cap 1, so every
    /// append past the first re-tries the rotation — which is the point: the
    /// warning must not follow the calls.
    fn sink_with_a_blocked_shift(dir: &Path) -> (std::path::PathBuf, AuditLog, Value) {
        let path = dir.join("audit.jsonl");
        let seed = json!({"v": 1, "tool": "seed"});
        std::fs::write(&path, format!("{seed}\n")).expect("seed over the cap");
        block_the_shift(&path);
        (path.clone(), AuditLog::new(path, 1), seed)
    }

    #[test]
    #[serial(audit_rotation)]
    fn a_rotation_that_cannot_rename_still_appends_past_the_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, sink, seed) = sink_with_a_blocked_shift(dir.path());

        rearm_rotation_warning();
        let (landed, logs) =
            capture_logs_blocking(|| sink.append(&json!({"v": 1, "tool": "kept"})));
        landed.expect("a rotation it cannot perform is not a failed append");

        assert_eq!(
            read_lines(&path),
            vec![seed, json!({"v": 1, "tool": "kept"})],
            "the record is the point; the cap is housekeeping"
        );
        assert_eq!(
            read_lines(generation_path(&path, 4).as_path()),
            vec![json!({"v": 1, "tool": "old"})],
            "the generation that could not move is untouched"
        );
        assert!(
            generation_path(&path, 5).is_dir(),
            "and nothing was written over the directory in its way"
        );
        assert!(
            !generation_path(&path, 1).exists(),
            "the shift stopped at the first failure instead of half-rotating"
        );
        let line = warn_line(&logs, "audit log: rotation failed");
        let (message, reason) = line
            .rsplit_once(" reason=")
            .unwrap_or_else(|| panic!("warn line shape: {line:?}"));
        assert_eq!(
            message,
            "audit log: rotation failed; appending past the cap"
        );
        assert!(!reason.is_empty(), "the reason is reported: {line:?}");
        assert!(
            !line.contains(&*path.to_string_lossy()),
            "closed text: the reason, never the path — it also rides the OTLP \
             diagnostics queue: {line:?}"
        );
    }

    #[test]
    #[serial(audit_rotation)]
    fn a_rotation_outage_warns_once_and_re_arms_on_recovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, sink, _seed) = sink_with_a_blocked_shift(dir.path());

        rearm_rotation_warning();
        let (_, logs) = capture_logs_blocking(|| {
            sink.append(&json!({"v": 1, "tool": "a"})).expect("first");
            sink.append(&json!({"v": 1, "tool": "b"})).expect("second");
        });
        // Every append past the cap re-tries the rotation, so without the latch
        // this is one warning per mutating call for the life of the server.
        warn_line(&logs, "audit log: rotation failed");

        // Clear the obstruction: the next append rotates, and a rotation that
        // completes re-arms the report so a later outage is not swallowed.
        std::fs::remove_dir(generation_path(&path, 5)).expect("clear the way");
        sink.append(&json!({"v": 1, "tool": "c"}))
            .expect("rotation succeeds now");
        assert!(
            generation_path(&path, 1).exists(),
            "the rotation really happened"
        );

        std::fs::remove_file(generation_path(&path, 5)).expect("the shifted generation");
        block_the_shift(&path);
        let (_, again) =
            capture_logs_blocking(|| sink.append(&json!({"v": 1, "tool": "d"})).expect("fourth"));
        warn_line(&again, "audit log: rotation failed");
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
        let sink = AuditLog::new(path.clone(), 0);
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
        let sink = AuditLog::new(dir.path().to_path_buf(), 0);
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
