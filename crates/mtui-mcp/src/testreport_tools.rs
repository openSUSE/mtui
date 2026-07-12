//! Hand-written MCP tools for editing the loaded testreport checkout file(s).
//!
//! Port of upstream `mtui/mcp/testreport_tools.py`. The auto-generated command
//! tools ([`crate::tools`]) cover every `Command`, but the REPL `edit` command
//! spawns `$EDITOR` on `metadata.path` — meaningless under MCP (and hence
//! deny-listed). This module replaces it with five explicit tools that operate
//! directly on the file path tracked by the loaded [`TestReport`]:
//!
//! * [`testreport_read`] — return a checkout file's content plus a line count
//!   (defaults to the `log` file; `relpath` reads any other checkout file,
//!   traversal-guarded; `offset`/`limit` page a 1-indexed line window).
//! * [`testreport_logs`] — list the `build_checks/` and `install_logs/` files.
//! * [`testreport_patch`] — splice an inclusive 1-indexed line range, atomically.
//! * [`testreport_write`] — full-file atomic overwrite.
//! * [`testreport_fill`] — bulk-set the repetitive per-bug placeholder tokens an
//!   exported testreport ships with, idempotently.
//!
//! ## Locking (interim)
//!
//! Upstream acquires the *per-RRID* `scoped_lock(template)` so a tool serialises
//! only against same-template dispatch. The Rust [`McpSession`] has no
//! `scoped_lock` yet (per-RRID locking is bead `mtui-rs-76e.11`), so every file
//! op here takes the single session-wide `Mutex<Session>` via
//! [`McpSession::session`]. This is coarser (it serialises against *all* command
//! dispatch, not just same-template work) but correct; P7.11 refines it.
//!
//! ## Multi-template resolution
//!
//! Resolution mirrors the auto-generated tools' `-T/--template` contract even
//! though locking is coarse: `template=<rrid>` selects a loaded report; omitted
//! with >1 loaded refuses (no client-addressable "active" pointer under MCP);
//! omitted with 0/1 loaded falls back to the active report. Progress heartbeats
//! (`ctx`) are bead `mtui-rs-76e.14` and intentionally absent here.

use std::path::{Path, PathBuf};

use mtui_core::Session;
use mtui_testreport::{TestReport, atomic_write_file};
use serde_json::{Map, Value, json};

use crate::session::{McpCommandError, McpSession};
use crate::slim::cap_output;
use crate::tools::ToolDescriptor;

/// Warning glued onto every tool description so the LLM re-reads before patching.
const READ_FIRST_WARNING: &str = "Always call `testreport_read` immediately before `testreport_patch` to get current \
     line numbers; line numbers shift after every patch.";

/// Glued onto every description: with several templates loaded a tool must be
/// told which one to act on (there is no client-addressable active pointer).
const TEMPLATE_NOTE: &str = "Pass `template=<rrid>` to target a specific loaded template; required when more \
     than one template is loaded.";

/// Valid single-value codes for the per-bug `STATUS:` field.
const STATUS_CODES: &[&str] = &[
    "FIXED",
    "NOT_FIXED",
    "HYPOTHETICAL",
    "NOT_REPRODUCIBLE",
    "NO_ENVIRONMENT",
    "TOO_COMPLEX",
    "SKIPPED",
    "OTHER",
];

// --------------------------------------------------------------------------- //
// Helpers                                                                     //
// --------------------------------------------------------------------------- //

/// A uniform refusal envelope: empty stdout, one-sentence stderr, exit 1.
fn refuse(msg: impl Into<String>) -> McpCommandError {
    McpCommandError {
        stdout: String::new(),
        stderr: msg.into(),
        exit_code: 1,
    }
}

/// Resolve the on-disk path of the report a tool call should act on.
///
/// Mirrors upstream `_resolve_report` + `_resolve_testreport_path`:
/// * `template` given → that loaded template (`templates.get`); unknown → refuse
///   `"template not loaded: <rrid>"`.
/// * `template` omitted with >1 loaded → refuse `"multiple templates loaded (…)"`.
/// * `template` omitted with 0/1 loaded → the active report.
///
/// Then validates the report is loaded and has a path, else
/// `"no testreport loaded; run `load_template` first"`.
fn resolve_path(session: &Session, template: Option<&str>) -> Result<PathBuf, McpCommandError> {
    let report: &(dyn TestReport + Send + Sync) = if let Some(rrid) = template {
        session
            .templates
            .get(rrid)
            .ok_or_else(|| refuse(format!("template not loaded: {rrid}")))?
    } else {
        if session.templates.len() > 1 {
            let rrids = session.templates.rrids().join(", ");
            return Err(refuse(format!(
                "multiple templates loaded ({rrids}); pass template=<rrid>"
            )));
        }
        session.metadata()
    };

    if !report.is_loaded() {
        return Err(refuse("no testreport loaded; run `load_template` first"));
    }
    report
        .base()
        .path
        .clone()
        .ok_or_else(|| refuse("no testreport loaded; run `load_template` first"))
}

/// The checkout directory (parent of the `log` file) for the resolved report.
fn resolve_dir(session: &Session, template: Option<&str>) -> Result<PathBuf, McpCommandError> {
    let path = resolve_path(session, template)?;
    Ok(path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("")))
}

/// Resolve `relpath` under `base`, refusing anything that escapes it.
///
/// Guards against `..` traversal and absolute paths. Mirrors upstream
/// `_safe_template_file`: the target must be `base` itself or a descendant of it.
fn safe_template_file(base: &Path, relpath: &str) -> Result<PathBuf, McpCommandError> {
    let base_resolved = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let joined = base_resolved.join(relpath);
    // Normalise `.`/`..` lexically so a not-yet-existing file still resolves
    // (canonicalize would fail on a missing path).
    let target = normalize(&joined);
    if target != base_resolved && !target.starts_with(&base_resolved) {
        return Err(refuse(format!(
            "path {relpath:?} escapes the testreport directory"
        )));
    }
    Ok(target)
}

/// Lexically normalise a path, collapsing `.` and `..` without touching disk.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Count lines with the `splitlines` convention: `"a\nb\n"`→2, `"a\nb"`→2,
/// `""`→0. Shared across read/patch/write/fill so counts never drift.
fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.lines().count()
}

/// Split `text` into lines *keeping* the trailing `\n` on each (upstream
/// `splitlines(keepends=True)`), so a splice preserves the newline invariant.
fn split_keepends(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(text[start..=i].to_owned());
            start = i + 1;
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_owned());
    }
    lines
}

/// Atomically write `text` to `path`, returning the byte count written.
fn write_atomic(path: &Path, text: &str) -> Result<usize, McpCommandError> {
    let bytes = text.as_bytes();
    atomic_write_file(bytes, path)
        .map_err(|e| refuse(format!("failed to write {}: {e}", path.display())))?;
    Ok(bytes.len())
}

/// Read a checkout file as UTF-8, replacing invalid sequences.
fn read_lossy(path: &Path) -> Result<String, McpCommandError> {
    let bytes = std::fs::read(path)
        .map_err(|e| refuse(format!("failed to read {}: {e}", path.display())))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// --------------------------------------------------------------------------- //
// Tools                                                                       //
// --------------------------------------------------------------------------- //

/// Read a testreport checkout file's content and line count.
///
/// Defaults to the report's `log` file; `relpath` reads any other file under the
/// checkout directory (traversal-guarded). `offset` (1-based, ≥1) / `limit` (≥0)
/// request a 1-indexed inclusive line window. `line_count` is always the file's
/// total; a windowed read also carries `offset`/`returned_lines`.
///
/// # Errors
/// Refuses on bad `offset`/`limit`, no loaded report, ambiguous/unknown
/// template, path traversal, or a missing `relpath` file.
pub async fn testreport_read(
    session: &McpSession,
    relpath: Option<&str>,
    offset: usize,
    limit: Option<usize>,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    if offset < 1 {
        return Err(refuse(format!("offset must be >= 1 (got {offset})")));
    }

    let (path, content) = {
        let guard = session.session().lock().await;
        let path = if let Some(rel) = relpath {
            let base = resolve_dir(&guard, template)?;
            let p = safe_template_file(&base, rel)?;
            if !p.is_file() {
                return Err(refuse(format!(
                    "no such file in testreport checkout: {rel}"
                )));
            }
            p
        } else {
            resolve_path(&guard, template)?
        };
        let content = read_lossy(&path)?;
        (path, content)
    };

    let cap = session.max_output_bytes();
    let windowed = offset != 1 || limit.is_some();
    if !windowed {
        return Ok(json!({
            "path": path.to_string_lossy(),
            "line_count": count_lines(&content),
            "content": cap_output(content, cap),
        }));
    }

    let lines = split_keepends(&content);
    let start = offset - 1;
    let sliced: String = match limit {
        Some(n) => lines.iter().skip(start).take(n).cloned().collect(),
        None => lines.iter().skip(start).cloned().collect(),
    };
    let returned = match limit {
        Some(n) => lines.len().saturating_sub(start).min(n),
        None => lines.len().saturating_sub(start),
    };
    Ok(json!({
        "path": path.to_string_lossy(),
        "line_count": lines.len(),
        "offset": offset,
        "returned_lines": returned,
        "content": cap_output(sliced, cap),
    }))
}

/// List the auxiliary log files (`build_checks/`, `install_logs/`) in the
/// loaded testreport's checkout.
///
/// # Errors
/// Refuses when no report is loaded or the template is ambiguous/unknown.
pub async fn testreport_logs(
    session: &McpSession,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    let guard = session.session().lock().await;
    let base = resolve_dir(&guard, template)?;

    let listing = |sub: &str| -> Vec<Value> {
        let dir = base.join(sub);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut items: Vec<(String, u64)> = rd
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file())
            .map(|e| {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                (e.file_name().to_string_lossy().into_owned(), size)
            })
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        items
            .into_iter()
            .map(|(name, size)| json!({ "name": name, "size": size }))
            .collect()
    };

    Ok(json!({
        "path": base.to_string_lossy(),
        "build_checks": listing("build_checks"),
        "install_logs": listing("install_logs"),
    }))
}

/// Replace an inclusive 1-indexed line range with `replacement`, atomically.
///
/// `end_line == start_line - 1` is a pure insert before `start_line`. A
/// non-empty `replacement` is forced to end in exactly one `\n`.
///
/// # Errors
/// Refuses on an out-of-bounds range, no loaded report, or ambiguous/unknown
/// template.
pub async fn testreport_patch(
    session: &McpSession,
    start_line: i64,
    end_line: i64,
    replacement: &str,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    let (path, new_text) = {
        let guard = session.session().lock().await;
        let path = resolve_path(&guard, template)?;
        let content = read_lossy(&path)?;
        let lines = split_keepends(&content);
        let n = lines.len() as i64;
        if start_line < 1 || end_line < start_line - 1 || end_line > n {
            return Err(refuse(format!(
                "line range out of bounds: start_line={start_line}, end_line={end_line}, \
                 file has {n} line(s)"
            )));
        }

        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len() + 1);
        new_lines.extend_from_slice(&lines[..(start_line - 1) as usize]);
        if !replacement.is_empty() {
            let normalized = if replacement.ends_with('\n') {
                replacement.to_owned()
            } else {
                format!("{replacement}\n")
            };
            new_lines.push(normalized);
        }
        new_lines.extend_from_slice(&lines[end_line as usize..]);
        let new_text: String = new_lines.concat();
        write_atomic(&path, &new_text)?;
        (path, new_text)
    };

    let replaced_lines = (end_line - start_line + 1).max(0);
    let inserted_lines = if replacement.is_empty() {
        0
    } else {
        count_lines(replacement)
    };
    let bytes_written = new_text.len();
    Ok(json!({
        "path": path.to_string_lossy(),
        "new_line_count": count_lines(&new_text),
        "replaced_lines": replaced_lines,
        "inserted_lines": inserted_lines,
        "bytes_written": bytes_written,
    }))
}

/// Overwrite the loaded testreport file with `content`, atomically.
///
/// Fallback for when line drift makes [`testreport_patch`] unreliable.
///
/// # Errors
/// Refuses when no report is loaded or the template is ambiguous/unknown.
pub async fn testreport_write(
    session: &McpSession,
    content: &str,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    let (path, bytes_written) = {
        let guard = session.session().lock().await;
        let path = resolve_path(&guard, template)?;
        let bytes = write_atomic(&path, content)?;
        (path, bytes)
    };
    Ok(json!({
        "path": path.to_string_lossy(),
        "bytes_written": bytes_written,
        "line_count": count_lines(content),
    }))
}

/// Bulk-fill the repetitive `SUMMARY:`/`REPRODUCER_PRESENT:`/`STATUS:`
/// placeholder tokens an exported testreport ships with, in one atomic write.
///
/// Only the *exact* template placeholder strings are replaced, so the call is
/// idempotent and never clobbers a value already filled by hand. At least one of
/// `reproducer`/`status`/`summary` must be given.
///
/// # Errors
/// Refuses on an invalid value, nothing-to-fill, no loaded report, or an
/// ambiguous/unknown template.
pub async fn testreport_fill(
    session: &McpSession,
    reproducer: Option<&str>,
    status: Option<&str>,
    summary: Option<&str>,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    if let Some(r) = reproducer
        && r != "YES"
        && r != "NO"
    {
        return Err(refuse(format!("reproducer must be YES or NO, got {r:?}")));
    }
    if let Some(s) = status
        && !STATUS_CODES.contains(&s)
    {
        return Err(refuse(format!(
            "status must be one of {STATUS_CODES:?}, got {s:?}"
        )));
    }
    if let Some(s) = summary
        && s != "PASSED"
        && s != "FAILED"
    {
        return Err(refuse(format!(
            "summary must be PASSED or FAILED, got {s:?}"
        )));
    }
    if reproducer.is_none() && status.is_none() && summary.is_none() {
        return Err(refuse(
            "nothing to fill: pass at least one of reproducer/status/summary",
        ));
    }

    let (path, new_text, counts) = {
        let guard = session.session().lock().await;
        let path = resolve_path(&guard, template)?;
        let content = read_lossy(&path)?;
        let lines = split_keepends(&content);
        let mut counts = (0u64, 0u64, 0u64); // (summary, reproducer, status)
        let new_lines: Vec<String> = lines
            .into_iter()
            .map(|line| {
                let (body, nl) = match line.strip_suffix('\n') {
                    Some(b) => (b, "\n"),
                    None => (line.as_str(), ""),
                };
                if let Some(s) = summary
                    && let Some(pre) = match_placeholder(body, "SUMMARY:", &["PASSED/FAILED"])
                {
                    counts.0 += 1;
                    return format!("{pre}{s}{nl}");
                }
                if let Some(r) = reproducer
                    && let Some(pre) = match_placeholder(body, "REPRODUCER_PRESENT:", &["YES/NO"])
                {
                    counts.1 += 1;
                    return format!("{pre}{r}{nl}");
                }
                if let Some(s) = status
                    && let Some(pre) = match_placeholder(
                        body,
                        "STATUS:",
                        &["FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/\
                             NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER"],
                    )
                {
                    counts.2 += 1;
                    return format!("{pre}{s}{nl}");
                }
                line
            })
            .collect();
        let new_text: String = new_lines.concat();
        write_atomic(&path, &new_text)?;
        (path, new_text, counts)
    };

    Ok(json!({
        "path": path.to_string_lossy(),
        "filled": {
            "summary": counts.0,
            "reproducer": counts.1,
            "status": counts.2,
        },
        "bytes_written": new_text.len(),
        "line_count": count_lines(&new_text),
    }))
}

/// If `body` is exactly `<ws>LABEL<ws>VALUE` for one of `values`, return the
/// leading `label + padding` (the upstream `pre` group) so the replacement keeps
/// the template's column alignment. Only the exact placeholder value matches, so
/// an already-filled line is never touched (idempotent).
fn match_placeholder<'a>(body: &'a str, label: &str, values: &[&str]) -> Option<&'a str> {
    let idx = body.find(label)?;
    // Leading portion must be whitespace only.
    if !body[..idx].chars().all(char::is_whitespace) {
        return None;
    }
    let after_label = &body[idx + label.len()..];
    let value = after_label.trim_start();
    let pad_len = after_label.len() - value.len();
    // Trailing must be whitespace only after the value token.
    for v in values {
        if let Some(rest) = value.strip_prefix(v)
            && rest.chars().all(char::is_whitespace)
        {
            // `pre` = everything up to and including the label + padding.
            let pre_end = idx + label.len() + pad_len;
            return Some(&body[..pre_end]);
        }
    }
    None
}

// --------------------------------------------------------------------------- //
// Descriptors + dispatch                                                      //
// --------------------------------------------------------------------------- //

/// Build the schema for a testreport tool from its property entries + required.
fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Map<String, Value> {
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
    s
}

/// The five testreport tool descriptors (transport-free), sorted by name.
#[must_use]
pub fn testreport_tool_descriptors() -> Vec<ToolDescriptor> {
    let template_prop = || {
        json!({
            "type": "string",
            "description": "RRID of a loaded template to target (required when >1 loaded).",
        })
    };

    let read = ToolDescriptor {
        name: "testreport_read".to_owned(),
        description: format!(
            "Read a file from the loaded testreport's checkout. Returns the path, total \
             line count, and content (utf-8, errors replaced). By default (no `relpath`) \
             reads the report's `log` file; pass `relpath` to read another checkout file \
             instead, e.g. 'build_checks/<pkg>.<arch>.log', 'install_logs/<host>.log', \
             'source.diff' or 'patchinfo.xml' — the path may not escape the checkout \
             directory. Pass `offset` (1-based first line) and/or `limit` (max lines) to \
             read a line window instead of the whole file. {READ_FIRST_WARNING} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "relpath",
                    json!({ "type": "string", "description": "Checkout-relative file to read; defaults to the `log` file." }),
                ),
                (
                    "offset",
                    json!({ "type": "integer", "minimum": 1, "default": 1, "description": "1-based first line to return." }),
                ),
                (
                    "limit",
                    json!({ "type": "integer", "minimum": 0, "description": "Max lines to return (default: to end of file)." }),
                ),
                ("template", template_prop()),
            ],
            &[],
        ),
        read_only: true,
    };

    let logs = ToolDescriptor {
        name: "testreport_logs".to_owned(),
        description: format!(
            "List the auxiliary log files in the loaded testreport's checkout: the \
             per-package/arch build-check logs (build_checks/) and the per-refhost install \
             logs (install_logs/). Returns each file's name and size; fetch one with \
             testreport_read (pass relpath). {TEMPLATE_NOTE}"
        ),
        input_schema: schema(vec![("template", template_prop())], &[]),
        read_only: true,
    };

    let patch = ToolDescriptor {
        name: "testreport_patch".to_owned(),
        description: format!(
            "Splice an inclusive 1-indexed line range in the currently loaded testreport \
             file. `end_line == start_line - 1` inserts before `start_line` without \
             replacing anything. The write is atomic. {READ_FIRST_WARNING} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "start_line",
                    json!({ "type": "integer", "description": "First line of the inclusive range (1-based)." }),
                ),
                (
                    "end_line",
                    json!({ "type": "integer", "description": "Last line of the inclusive range; start_line-1 to insert." }),
                ),
                (
                    "replacement",
                    json!({ "type": "string", "description": "Replacement text; empty deletes the range." }),
                ),
                ("template", template_prop()),
            ],
            &["start_line", "end_line", "replacement"],
        ),
        read_only: false,
    };

    let write = ToolDescriptor {
        name: "testreport_write".to_owned(),
        description: format!(
            "Overwrite the currently loaded testreport file with the given content. \
             Atomic. Use this as the fallback when patching would require tracking \
             line-number drift across many edits. {READ_FIRST_WARNING} {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "content",
                    json!({ "type": "string", "description": "The full new file content." }),
                ),
                ("template", template_prop()),
            ],
            &["content"],
        ),
        read_only: false,
    };

    let fill = ToolDescriptor {
        name: "testreport_fill".to_owned(),
        description: format!(
            "Bulk-set the repetitive per-bug placeholder tokens an exported testreport \
             ships with, in one atomic write. `reproducer` (YES/NO) sets every unfilled \
             `REPRODUCER_PRESENT:` line; `status` (one of FIXED, NOT_FIXED, HYPOTHETICAL, \
             NOT_REPRODUCIBLE, NO_ENVIRONMENT, TOO_COMPLEX, SKIPPED, OTHER) sets every \
             unfilled templated `STATUS:` line; `summary` (PASSED/FAILED) sets the top \
             `SUMMARY:` line. Only exact template placeholders are touched, so it is \
             idempotent and never overwrites a value you already set by hand. Returns a \
             `filled` count per token. {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "reproducer",
                    json!({ "type": "string", "enum": ["YES", "NO"], "description": "Set every unfilled REPRODUCER_PRESENT: line." }),
                ),
                (
                    "status",
                    json!({ "type": "string", "enum": STATUS_CODES, "description": "Set every unfilled templated STATUS: line." }),
                ),
                (
                    "summary",
                    json!({ "type": "string", "enum": ["PASSED", "FAILED"], "description": "Set the top SUMMARY: line." }),
                ),
                ("template", template_prop()),
            ],
            &[],
        ),
        read_only: false,
    };

    let mut tools = vec![read, logs, patch, write, fill];
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// Decode a `Value` string field, refusing a non-string.
fn opt_str<'a>(
    kwargs: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, McpCommandError> {
    match kwargs.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(other) => Err(refuse(format!("{key} must be a string, got {other}"))),
    }
}

/// Decode a required `Value` string field.
fn req_str<'a>(kwargs: &'a Map<String, Value>, key: &str) -> Result<&'a str, McpCommandError> {
    opt_str(kwargs, key)?.ok_or_else(|| refuse(format!("missing required argument: {key}")))
}

/// Decode an integer field (JSON number), with a default when absent.
fn int_field(kwargs: &Map<String, Value>, key: &str, default: i64) -> Result<i64, McpCommandError> {
    match kwargs.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| refuse(format!("{key} must be an integer"))),
        Some(other) => Err(refuse(format!("{key} must be an integer, got {other}"))),
    }
}

/// Dispatch a testreport tool call by name, decoding `kwargs` to typed args.
///
/// # Errors
/// Returns [`McpCommandError`] for an unknown testreport tool name, a bad
/// argument type/value, or any failure surfaced by the tool itself.
pub async fn dispatch_testreport_tool(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
) -> Result<Value, McpCommandError> {
    let template = opt_str(kwargs, "template")?;
    match name {
        "testreport_read" => {
            let relpath = opt_str(kwargs, "relpath")?;
            let offset = int_field(kwargs, "offset", 1)?;
            if offset < 1 {
                return Err(refuse(format!("offset must be >= 1 (got {offset})")));
            }
            let limit = match kwargs.get("limit") {
                None | Some(Value::Null) => None,
                Some(Value::Number(n)) => {
                    let v = n
                        .as_i64()
                        .ok_or_else(|| refuse("limit must be an integer"))?;
                    if v < 0 {
                        return Err(refuse(format!("limit must be >= 0 (got {v})")));
                    }
                    Some(v as usize)
                }
                Some(other) => {
                    return Err(refuse(format!("limit must be an integer, got {other}")));
                }
            };
            testreport_read(session, relpath, offset as usize, limit, template).await
        }
        "testreport_logs" => testreport_logs(session, template).await,
        "testreport_patch" => {
            let start_line = int_field(kwargs, "start_line", 0)?;
            let end_line = int_field(kwargs, "end_line", 0)?;
            let replacement = req_str(kwargs, "replacement")?;
            testreport_patch(session, start_line, end_line, replacement, template).await
        }
        "testreport_write" => {
            let content = req_str(kwargs, "content")?;
            testreport_write(session, content, template).await
        }
        "testreport_fill" => {
            let reproducer = opt_str(kwargs, "reproducer")?;
            let status = opt_str(kwargs, "status")?;
            let summary = opt_str(kwargs, "summary")?;
            testreport_fill(session, reproducer, status, summary, template).await
        }
        other => Err(refuse(format!("unknown testreport tool: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use mtui_config::Config;
    use mtui_testreport::ObsReport;
    use mtui_types::RequestReviewID;

    /// Build an `McpSession` whose `template_dir` is a fresh temp dir; returns
    /// the session plus the tempdir handle (kept alive for the test).
    fn session_with_tmp() -> (std::sync::Arc<McpSession>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.template_dir = tmp.path().to_path_buf();
        (McpSession::new(config), tmp)
    }

    /// Add a loaded `ObsReport` for `rrid` whose `log` file lives at `path`
    /// (with initial `content`), making it active. Creates the file on disk.
    async fn load_report(session: &McpSession, rrid: &str, path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        let mut guard = session.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        report.base_mut().path = Some(path.to_path_buf());
        guard.templates.add(Box::new(report));
        guard.templates.set_active(rrid);
    }

    const RRID: &str = "SUSE:Maintenance:1:1";

    fn log_path(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        tmp.path().join("checkout").join("log")
    }

    // ---- refusal without a loaded report ---------------------------------- //

    #[tokio::test]
    async fn read_refuses_without_loaded_report() {
        let (session, _tmp) = session_with_tmp();
        let err = testreport_read(&session, None, 1, None, None)
            .await
            .expect_err("null report refuses");
        assert_eq!(err.exit_code, 1);
        assert!(err.stderr.contains("no testreport loaded"), "{err:?}");
    }

    #[tokio::test]
    async fn patch_refuses_without_loaded_report() {
        let (session, _tmp) = session_with_tmp();
        let err = testreport_patch(&session, 1, 1, "x", None)
            .await
            .expect_err("null report refuses");
        assert!(err.stderr.contains("no testreport loaded"), "{err:?}");
    }

    #[tokio::test]
    async fn write_refuses_without_loaded_report() {
        let (session, _tmp) = session_with_tmp();
        let err = testreport_write(&session, "x", None)
            .await
            .expect_err("null report refuses");
        assert!(err.stderr.contains("no testreport loaded"), "{err:?}");
    }

    // ---- read ------------------------------------------------------------- //

    #[tokio::test]
    async fn read_returns_file_contents_and_line_count() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "l1\nl2\nl3\nl4\nl5\n").await;

        let res = testreport_read(&session, None, 1, None, None)
            .await
            .unwrap();
        assert_eq!(res["line_count"], 5);
        assert_eq!(res["content"], "l1\nl2\nl3\nl4\nl5\n");
        assert!(res.get("returned_lines").is_none(), "no window: {res}");
    }

    #[tokio::test]
    async fn read_window_offset_and_limit() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "l1\nl2\nl3\nl4\nl5\n").await;

        let res = testreport_read(&session, None, 2, Some(2), None)
            .await
            .unwrap();
        assert_eq!(res["line_count"], 5, "total, not window size");
        assert_eq!(res["offset"], 2);
        assert_eq!(res["returned_lines"], 2);
        assert_eq!(res["content"], "l2\nl3\n");
    }

    #[tokio::test]
    async fn read_window_offset_to_end() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "l1\nl2\nl3\nl4\nl5\n").await;

        let res = testreport_read(&session, None, 4, None, None)
            .await
            .unwrap();
        assert_eq!(res["returned_lines"], 2);
        assert_eq!(res["line_count"], 5);
        assert_eq!(res["content"], "l4\nl5\n");
    }

    #[tokio::test]
    async fn read_window_offset_past_end_is_empty() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "l1\nl2\n").await;

        let res = testreport_read(&session, None, 99, None, None)
            .await
            .unwrap();
        assert_eq!(res["returned_lines"], 0);
        assert_eq!(res["line_count"], 2);
        assert_eq!(res["content"], "");
    }

    #[tokio::test]
    async fn read_rejects_bad_offset() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "l1\n").await;
        let err = testreport_read(&session, None, 0, None, None)
            .await
            .expect_err("offset 0 refused");
        assert!(err.stderr.contains("offset must be >= 1"), "{err:?}");
    }

    #[tokio::test]
    async fn read_relpath_missing_and_traversal() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "l1\n").await;

        let missing = testreport_read(&session, Some("build_checks/nope.log"), 1, None, None)
            .await
            .expect_err("missing file");
        assert!(
            missing
                .stderr
                .contains("no such file in testreport checkout"),
            "{missing:?}"
        );

        let escape = testreport_read(&session, Some("../../etc/passwd"), 1, None, None)
            .await
            .expect_err("traversal refused");
        assert!(escape.stderr.contains("escapes"), "{escape:?}");
    }

    // ---- logs ------------------------------------------------------------- //

    #[tokio::test]
    async fn logs_and_read_file_roundtrip() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "log\n").await;
        let checkout = path.parent().unwrap();
        std::fs::create_dir_all(checkout.join("build_checks")).unwrap();
        std::fs::write(checkout.join("build_checks/pkg.x86_64.log"), "a\nb\n").unwrap();

        let listed = testreport_logs(&session, None).await.unwrap();
        let bc = listed["build_checks"].as_array().unwrap();
        assert_eq!(bc.len(), 1);
        assert_eq!(bc[0]["name"], "pkg.x86_64.log");
        assert_eq!(bc[0]["size"], 4);
        assert!(listed["install_logs"].as_array().unwrap().is_empty());

        let out = testreport_read(&session, Some("build_checks/pkg.x86_64.log"), 1, None, None)
            .await
            .unwrap();
        assert_eq!(out["line_count"], 2);
        assert_eq!(out["content"], "a\nb\n");
    }

    #[tokio::test]
    async fn logs_empty_when_subdirs_absent() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "log\n").await;
        let listed = testreport_logs(&session, None).await.unwrap();
        assert!(listed["build_checks"].as_array().unwrap().is_empty());
        assert!(listed["install_logs"].as_array().unwrap().is_empty());
    }

    // ---- patch ------------------------------------------------------------ //

    #[tokio::test]
    async fn patch_replaces_range() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "a\nb\nc\nd\n").await;

        let res = testreport_patch(&session, 2, 3, "X\nY\nZ", None)
            .await
            .unwrap();
        assert_eq!(res["new_line_count"], 5);
        assert_eq!(res["replaced_lines"], 2);
        assert_eq!(res["inserted_lines"], 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nX\nY\nZ\nd\n");
    }

    #[tokio::test]
    async fn patch_insert_before_first_line() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "a\nb\n").await;

        let res = testreport_patch(&session, 1, 0, "HEAD", None)
            .await
            .unwrap();
        assert_eq!(res["new_line_count"], 3);
        assert_eq!(res["replaced_lines"], 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "HEAD\na\nb\n");
    }

    #[tokio::test]
    async fn patch_normalises_missing_trailing_newline() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "a\nb\n").await;
        testreport_patch(&session, 1, 1, "no-newline", None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "no-newline\nb\n");
    }

    #[tokio::test]
    async fn patch_empty_replacement_is_pure_delete() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "a\nb\nc\n").await;
        let res = testreport_patch(&session, 2, 2, "", None).await.unwrap();
        assert_eq!(res["new_line_count"], 2);
        assert_eq!(res["inserted_lines"], 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nc\n");
    }

    #[tokio::test]
    async fn patch_out_of_range_names_the_line_count() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "a\nb\n").await;
        let err = testreport_patch(&session, 1, 9, "x", None)
            .await
            .expect_err("out of range");
        assert!(err.stderr.contains("file has 2 line(s)"), "{err:?}");
    }

    // ---- write ------------------------------------------------------------ //

    #[tokio::test]
    async fn write_overwrites_atomically() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "old\n").await;
        let res = testreport_write(&session, "new1\nnew2\n", None)
            .await
            .unwrap();
        assert_eq!(res["line_count"], 2);
        assert_eq!(res["bytes_written"], 10);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new1\nnew2\n");
    }

    // ---- fill ------------------------------------------------------------- //

    const FILL_TEMPLATE: &str = "SUMMARY:            PASSED/FAILED\n\
         REPRODUCER_PRESENT: YES/NO\n\
         STATUS:             FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER\n\
         STATUS:             SKIPPED\n\
         REPRODUCER_PRESENT: YES/NO\n\
         REPRODUCER_PRESENT: YES\n\
         STATUS:             FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER\n";

    #[tokio::test]
    async fn fill_sets_unfilled_placeholders_only() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, FILL_TEMPLATE).await;

        let res = testreport_fill(&session, Some("NO"), Some("SKIPPED"), Some("PASSED"), None)
            .await
            .unwrap();
        // summary x1; two unfilled REPRODUCER lines; two templated STATUS lines.
        assert_eq!(res["filled"]["summary"], 1);
        assert_eq!(res["filled"]["reproducer"], 2);
        assert_eq!(res["filled"]["status"], 2);
        let out = std::fs::read_to_string(&path).unwrap();
        // Already-filled lines untouched, column alignment preserved.
        assert!(out.contains("SUMMARY:            PASSED\n"), "{out}");
        assert!(out.contains("REPRODUCER_PRESENT: NO\n"), "{out}");
        assert!(out.contains("REPRODUCER_PRESENT: YES\n"), "kept: {out}");
        assert!(out.contains("STATUS:             SKIPPED\n"), "{out}");
    }

    #[tokio::test]
    async fn fill_is_idempotent() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, FILL_TEMPLATE).await;
        testreport_fill(&session, Some("NO"), Some("SKIPPED"), Some("PASSED"), None)
            .await
            .unwrap();
        let res2 = testreport_fill(&session, Some("NO"), Some("SKIPPED"), Some("PASSED"), None)
            .await
            .unwrap();
        assert_eq!(res2["filled"]["summary"], 0);
        assert_eq!(res2["filled"]["reproducer"], 0);
        assert_eq!(res2["filled"]["status"], 0);
    }

    #[tokio::test]
    async fn fill_partial_only_requested_tokens() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, FILL_TEMPLATE).await;
        let res = testreport_fill(&session, Some("NO"), None, None, None)
            .await
            .unwrap();
        assert_eq!(res["filled"]["summary"], 0);
        assert_eq!(res["filled"]["reproducer"], 2);
        assert_eq!(res["filled"]["status"], 0);
    }

    #[tokio::test]
    async fn fill_rejects_bad_values_and_empty() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, FILL_TEMPLATE).await;

        assert!(
            testreport_fill(&session, Some("MAYBE"), None, None, None)
                .await
                .is_err()
        );
        assert!(
            testreport_fill(&session, None, Some("BOGUS"), None, None)
                .await
                .is_err()
        );
        let empty = testreport_fill(&session, None, None, None, None)
            .await
            .expect_err("nothing to fill");
        assert!(empty.stderr.contains("nothing to fill"), "{empty:?}");
    }

    // ---- multi-template resolution ---------------------------------------- //

    #[tokio::test]
    async fn read_refuses_when_multiple_loaded_without_template() {
        let (session, tmp) = session_with_tmp();
        let p1 = tmp.path().join("c1").join("log");
        let p2 = tmp.path().join("c2").join("log");
        load_report(&session, "SUSE:Maintenance:1:1", &p1, "one\n").await;
        load_report(&session, "SUSE:Maintenance:2:2", &p2, "two\n").await;

        let err = testreport_read(&session, None, 1, None, None)
            .await
            .expect_err("ambiguous");
        assert!(err.stderr.contains("multiple templates loaded"), "{err:?}");
    }

    #[tokio::test]
    async fn read_with_template_selects_that_report() {
        let (session, tmp) = session_with_tmp();
        let p1 = tmp.path().join("c1").join("log");
        let p2 = tmp.path().join("c2").join("log");
        load_report(&session, "SUSE:Maintenance:1:1", &p1, "one\n").await;
        load_report(&session, "SUSE:Maintenance:2:2", &p2, "two\n").await;

        let res = testreport_read(&session, None, 1, None, Some("SUSE:Maintenance:2:2"))
            .await
            .unwrap();
        assert_eq!(res["content"], "two\n");
    }

    #[tokio::test]
    async fn read_with_unknown_template_raises() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "x\n").await;
        let err = testreport_read(&session, None, 1, None, Some("SUSE:Maintenance:9:9"))
            .await
            .expect_err("unknown template");
        assert!(err.stderr.contains("template not loaded"), "{err:?}");
    }

    // ---- helpers ---------------------------------------------------------- //

    #[test]
    fn count_lines_matches_splitlines() {
        assert_eq!(count_lines("a\nb\n"), 2);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines(""), 0);
    }

    #[test]
    fn descriptors_expose_all_five_with_hints() {
        let d = testreport_tool_descriptors();
        let names: Vec<&str> = d.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "testreport_fill",
                "testreport_logs",
                "testreport_patch",
                "testreport_read",
                "testreport_write",
            ]
        );
        let by = |n: &str| d.iter().find(|t| t.name == n).unwrap();
        assert!(by("testreport_read").read_only);
        assert!(by("testreport_logs").read_only);
        assert!(!by("testreport_patch").read_only);
        assert!(!by("testreport_write").read_only);
        // patch requires start_line/end_line/replacement; write requires content.
        let req = |n: &str| {
            by(n).input_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let patch_req = req("testreport_patch");
        for f in ["start_line", "end_line", "replacement"] {
            assert!(patch_req.contains(&f.to_owned()), "patch needs {f}");
        }
        assert_eq!(req("testreport_write"), vec!["content"]);
    }

    #[tokio::test]
    async fn dispatch_routes_by_name_and_reports_unknown() {
        let (session, tmp) = session_with_tmp();
        let path = log_path(&tmp);
        load_report(&session, RRID, &path, "a\nb\n").await;

        let mut kwargs = Map::new();
        let res = dispatch_testreport_tool(&session, "testreport_read", &kwargs)
            .await
            .unwrap();
        assert_eq!(res["line_count"], 2);

        kwargs.insert("start_line".into(), json!(1));
        kwargs.insert("end_line".into(), json!(1));
        kwargs.insert("replacement".into(), json!("X"));
        let patched = dispatch_testreport_tool(&session, "testreport_patch", &kwargs)
            .await
            .unwrap();
        assert_eq!(patched["new_line_count"], 2);

        let err = dispatch_testreport_tool(&session, "testreport_bogus", &Map::new())
            .await
            .expect_err("unknown tool");
        assert!(err.stderr.contains("unknown testreport tool"), "{err:?}");
    }

    #[test]
    fn safe_template_file_allows_nested_and_blocks_escape() {
        let base = Path::new("/tmp/checkout");
        assert!(safe_template_file(base, "build_checks/x.log").is_ok());
        assert!(safe_template_file(base, "../escape").is_err());
        assert!(safe_template_file(base, "/etc/passwd").is_err());
    }
}
