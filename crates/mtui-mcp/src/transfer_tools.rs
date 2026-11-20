//! Hand-written in-band `get`/`put` MCP tools (#434).
//!
//! The synthesised forms of these two commands exchange **server-local paths**:
//! `get` downloaded to `{report_wd}/downloads/` and returned the path, `put`
//! uploaded a file that had to already exist on the server. Under
//! `--transport http` the client is on another machine, so those paths are
//! unreachable and `put` is unusable outright. Both commands are therefore on
//! [`MCP_DENYLIST`](mtui_core::MCP_DENYLIST) and re-served here under the same
//! names — the `edit` → document tools precedent — carrying the content
//! **in-band** in both directions, on both transports.
//!
//! The REPL/CLI `get`/`put` commands are untouched: literal paths (#399), the
//! `{report_wd}/downloads/{name}.{host}` layout, and their messages all stay.
//!
//! Content encoding: UTF-8 payloads travel as `content`, anything else as
//! `content_b64`. The one exception is `put`'s `source`: a file already on the
//! server, inside an operator-listed `[mcp] put_dirs` directory, which is read
//! there and uploaded without passing through the call. Downloads are capped twice — per host at
//! `[mcp] max_input_bytes` (enforced while reading, so a host never buffers more
//! than the cap), then at an equal share of `[mcp] max_output_bytes` on the wire —
//! with an explicit `truncated` flag and the remote `size`, never a silent clip.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use base64ct::{Base64, Encoding};
use mtui_core::{
    Activation, ColorMode, CommandPromptDisplay, SingleTemplate, ambiguous_template_message,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::audit::sha256_hex;
use crate::capture::SharedBuf;
use crate::report_files::normalize;
use crate::session::{
    CommandLock, DEFAULT_PROGRESS_INTERVAL, McpCommandError, McpSession, ProgressSink,
    run_with_heartbeat,
};
use crate::tools::ToolDescriptor;

/// Shared "which template" note, kept textually aligned with the document
/// tools' `TEMPLATE_NOTE`.
const TEMPLATE_NOTE: &str = "Pass `template=<rrid>` to target a specific loaded template; required when more \
     than one is loaded.";

fn refuse(msg: impl Into<String>) -> McpCommandError {
    McpCommandError {
        stdout: String::new(),
        stderr: msg.into(),
        exit_code: 1,
    }
}

/// Build the schema for a transfer tool (strict: unknown fields rejected).
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
    s.insert("additionalProperties".to_owned(), Value::Bool(false));
    s
}

/// The two transfer tool descriptors (transport-free).
#[must_use]
pub fn transfer_tool_descriptors() -> Vec<ToolDescriptor> {
    let template_prop = || {
        json!({
            "type": "string",
            "description": "RRID of a loaded template to target (required when >1 loaded).",
        })
    };

    let get = ToolDescriptor {
        name: "get".to_owned(),
        description: format!(
            "Download a file from every enabled host, content in-band per host \
             (`content` utf-8, `content_b64` binary, always with full remote `size` and \
             `truncated`). Capped per host at [mcp] max_input_bytes plus an equal share \
             of [mcp] max_output_bytes; pass `hosts` to retry or page a subset. No \
             folders in-band (trailing / unsupported). Any host failure fails the call, naming the host. {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "remote",
                    json!({ "type": "string", "description": "Absolute path of the file on the reference hosts." }),
                ),
                (
                    "hosts",
                    json!({
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Subset of connected hostnames; unknown or disabled names refuse the call.",
                    }),
                ),
                ("template", template_prop()),
            ],
            &["remote"],
        ),
        read_only: true,
    };

    let put = ToolDescriptor {
        name: "put".to_owned(),
        description: format!(
            "Upload `content` (utf-8), `content_b64` or `source` (exactly one) to \
             `<target tempdir>/<filename>` on every enabled host. `source` is an absolute \
             path of a file on the mtui-mcp server inside an [mcp] put_dirs directory \
             (refused when put_dirs is unset); it is uploaded byte for byte without \
             passing through the call, `filename` defaults to its basename, and the reply \
             carries `source` {{path, bytes, sha256}}. Payloads above [mcp] \
             max_input_bytes are refused, not truncated. Any host failure fails the \
             call, naming the host. {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "filename",
                    json!({ "type": "string", "description": "Bare file name to create on the hosts (no path separators); defaults to the basename of `source`." }),
                ),
                (
                    "content",
                    json!({ "type": "string", "description": "UTF-8 payload; exactly one of content/content_b64/source." }),
                ),
                (
                    "content_b64",
                    json!({ "type": "string", "description": "Base64 payload; exactly one of content/content_b64/source." }),
                ),
                (
                    "source",
                    json!({
                        "type": "string",
                        "description": "Absolute path of a file on the mtui-mcp server, inside an [mcp] put_dirs directory; uploaded byte for byte without passing through the call. Exactly one of content/content_b64/source.",
                    }),
                ),
                ("template", template_prop()),
            ],
            &[],
        ),
        read_only: false,
    };

    vec![get, put]
}

/// Dispatch a transfer tool by name, heartbeat-wrapped when a sink is given.
pub async fn dispatch_transfer_tool(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
    sink: Option<&dyn ProgressSink>,
) -> Result<Value, McpCommandError> {
    let body = dispatch_transfer_tool_inner(session, name, kwargs);
    match sink {
        None => body.await,
        Some(sink) => run_with_heartbeat(body, sink, name, DEFAULT_PROGRESS_INTERVAL).await,
    }
}

/// The undecorated dispatch, wrapped by [`dispatch_transfer_tool`].
async fn dispatch_transfer_tool_inner(
    session: &McpSession,
    name: &str,
    kwargs: &Map<String, Value>,
) -> Result<Value, McpCommandError> {
    // The allowed keys are the descriptor's advertised properties.
    if let Some(desc) = transfer_tool_descriptors()
        .into_iter()
        .find(|d| d.name == name)
    {
        let allowed = desc
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|props| props.keys().map(String::as_str));
        crate::tools::reject_unknown_kwargs(kwargs, allowed)?;
    }

    let template = opt_str(kwargs, "template")?;
    match name {
        "get" => {
            let remote = opt_str(kwargs, "remote")?
                .ok_or_else(|| refuse("`remote` is required"))?
                .to_owned();
            let hosts = match kwargs.get("hosts") {
                None | Some(Value::Null) => None,
                Some(Value::Array(items)) => {
                    if items.is_empty() {
                        return Err(refuse("hosts must name at least one connected host"));
                    }
                    let mut names = BTreeSet::new();
                    for item in items {
                        let Some(s) = item.as_str() else {
                            return Err(refuse(format!(
                                "hosts entries must be strings, got {item}"
                            )));
                        };
                        names.insert(s.to_owned());
                    }
                    Some(names)
                }
                Some(other) => {
                    return Err(refuse(format!(
                        "hosts must be an array of strings, got {other}"
                    )));
                }
            };
            transfer_get(session, &remote, hosts, template).await
        }
        "put" => {
            let content = opt_str(kwargs, "content")?;
            let content_b64 = opt_str(kwargs, "content_b64")?;
            let source = opt_str(kwargs, "source")?;
            let payload = match (content, content_b64, source) {
                (Some(text), None, None) => PutPayload::Content(text.to_owned()),
                (None, Some(b64), None) => PutPayload::ContentB64(b64.to_owned()),
                (None, None, Some(src)) => PutPayload::Source(src.to_owned()),
                (None, None, None) => {
                    return Err(refuse(
                        "one of `content`, `content_b64` or `source` is required",
                    ));
                }
                _ => {
                    return Err(refuse(
                        "pass exactly one of `content`, `content_b64` or `source`",
                    ));
                }
            };
            let filename = match (opt_str(kwargs, "filename")?, &payload) {
                (Some(name), _) => name.to_owned(),
                (None, PutPayload::Source(src)) => Path::new(src)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_owned)
                    .ok_or_else(|| refuse("`filename` is required: `source` has no file name"))?,
                (None, _) => return Err(refuse("`filename` is required unless `source` is given")),
            };
            transfer_put(session, &filename, payload, template).await
        }
        other => Err(refuse(format!("unknown transfer tool: {other}"))),
    }
}

/// `Some(trimmed value)` for a present string field, `None` when absent/null.
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

/// Resolves the target template rrid: the single loaded one, or the named one.
///
/// Refuses when nothing is loaded, when `template` names an unloaded rrid, and
/// when more than one is loaded with no `template`, as the document tools do.
/// The ambiguous case shares its wording with every other surface via
/// [`mtui_core::ambiguous_template_message`]; the other two refusals are this
/// tool family's own, pre-existing and grepped.
pub(crate) fn resolve_rrid(
    session: &mtui_core::Session,
    template: Option<&str>,
) -> Result<String, McpCommandError> {
    match session.resolve_single_template(template, false) {
        SingleTemplate::One(rrid) => Ok(rrid),
        SingleTemplate::NothingLoaded => {
            Err(refuse("no report loaded, please use load_template first"))
        }
        SingleTemplate::NotLoaded(t) => {
            let rrids = session.templates.rrids();
            Err(refuse(format!(
                "template {t} is not loaded (loaded: {})",
                if rrids.is_empty() {
                    "none".to_owned()
                } else {
                    rrids.join(", ")
                }
            )))
        }
        SingleTemplate::Ambiguous(loaded) => Err(refuse(ambiguous_template_message(
            &loaded,
            "pass `template=<rrid>`",
        ))),
    }
}

/// Take the shared gate plus the template's per-RRID lock and dispatch on a
/// per-call fork, returning the held lock with the activated fork.
///
/// Resolved inside the gate so the scope key and the fork stay on the same
/// template. Drops any leftover active guard first: no exclusive dispatch can
/// be in flight under the shared gate, so a guard present here is stale — and
/// it would fail the fork's activation below with `Busy`. A transient `Busy`
/// from a racing exclusive dispatch maps to the standard `template busy`
/// refusal.
async fn transfer_fork(
    session: &McpSession,
    template: Option<&str>,
) -> Result<(CommandLock, mtui_core::Session), McpCommandError> {
    let scope = session.scoped_lock(template).await;
    let mut guard = session.session().lock().await;
    let rrid = resolve_rrid(&guard, template)?;
    guard.release_active_guard();
    let call_buf = SharedBuf::with_limit(session.max_output_bytes());
    let call_display = CommandPromptDisplay::with_sink(Box::new(call_buf), ColorMode::Never);
    let mut fork = guard.fork_for_call(call_display);
    // Fresh token, never inherited: a hard-aborted exclusive dispatch can leave
    // a cancelled token on the canonical session, and `activate` pushes this
    // fork's token onto the shared `HostsGroup`.
    fork.set_cancel_token(CancellationToken::default());
    match fork.activate(&rrid) {
        Activation::Active => Ok((scope, fork)),
        Activation::Busy => Err(refuse(format!("template busy: {rrid}"))),
        Activation::NotLoaded | Activation::Empty => {
            Err(refuse(format!("could not activate template {rrid}")))
        }
    }
}

/// The in-band `get` body.
async fn transfer_get(
    session: &McpSession,
    remote: &str,
    hosts: Option<BTreeSet<String>>,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    if remote.ends_with('/') {
        return Err(refuse(
            "folder downloads cannot be returned in-band; name a file (the REPL `get` \
             command still downloads folders)",
        ));
    }

    let max_input = session.max_input_bytes();

    // Dispatch on a fork, so the SFTP fan-out does not hold the canonical
    // session mutex.
    let (scope, mut call_session) = transfer_fork(session, template).await?;

    let outcome = {
        let targets = call_session.targets_mut();
        let known: BTreeSet<String> = targets.names().into_iter().collect();
        if let Some(ref names) = hosts {
            // Unknown names refuse before any transfer; a named host absent from
            // the map (disabled) is refused after.
            let unknown: Vec<&str> = names
                .iter()
                .map(String::as_str)
                .filter(|n| !known.contains(*n))
                .collect();
            if !unknown.is_empty() {
                return Err(refuse(format!(
                    "unknown host(s): {} (connected: {})",
                    unknown.join(", "),
                    known.iter().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        let map = match hosts {
            Some(ref names) => {
                targets
                    .sftp_read_capped_selected(Path::new(remote), names, max_input)
                    .await
            }
            None => targets.sftp_read_capped(Path::new(remote), max_input).await,
        };
        // The fork (and its entry guard) is done: release it and the scope
        // before formatting — both only guarded the fan-out, and the rest
        // works on owned data.
        drop(call_session);
        drop(scope);

        // A requested host with no outcome was disabled: diagnose that before the
        // generic empty-map refusal, or an all-named-disabled call is
        // misdescribed as "no enabled hosts".
        if let Some(ref names) = hosts {
            let missing: Vec<&str> = names
                .iter()
                .map(String::as_str)
                .filter(|n| !map.contains_key(*n))
                .collect();
            if !missing.is_empty() {
                return Err(refuse(format!(
                    "host(s) disabled, no content returned: {}",
                    missing.join(", ")
                )));
            }
        }
        if map.is_empty() {
            return Err(refuse(
                "no enabled hosts connected — load a template with hosts, add_host, or \
                 enable a host first",
            ));
        }
        map
    };

    // Partial content is never returned as success (#396's lesson): any per-host
    // failure fails the call, with attribution, and `hosts` is the retry path.
    let failures: Vec<String> = outcome
        .iter()
        .filter_map(|(host, res)| res.as_ref().err().map(|e| format!("{host} ({e})")))
        .collect();
    if !failures.is_empty() {
        return Err(refuse(format!(
            "download failed on: {}",
            failures.join(", ")
        )));
    }

    // The source cap was applied while reading; now an equal per-host share of
    // the wire budget. Cap the content string BEFORE embedding it in JSON, never
    // the serialised JSON.
    let wire_budget = session.max_output_bytes();
    let share = if wire_budget == 0 {
        0 // 0 disables the wire cap, matching cap_output's contract.
    } else {
        std::cmp::max(1, wire_budget / outcome.len())
    };

    let mut hosts_json = Map::new();
    for (host, res) in &outcome {
        let read = res.as_ref().expect("failures already returned above");
        let size = read.size;
        let source_capped = read.bytes.as_slice();
        let mut truncated = read.truncated;
        // A mid-codepoint source-cap cut must not reclassify a text file as
        // binary: `error_len() == None` is only an incomplete final sequence, so
        // trim to the last whole codepoint and stay textual. A genuinely binary
        // payload errors mid-stream and takes the b64 arm.
        let text = match std::str::from_utf8(source_capped) {
            Ok(text) => Some(text),
            Err(e) if truncated && e.error_len().is_none() => {
                Some(std::str::from_utf8(&source_capped[..e.valid_up_to()]).expect("valid prefix"))
            }
            Err(_) => None,
        };
        let entry = match text {
            Some(text) => {
                // No embedded notice: `cap_output` would splice its
                // tool-flavoured text into the file content. `truncated` +
                // `size` are the signal, symmetric with the binary arm.
                let mut text = text;
                if share > 0 && text.len() > share {
                    let mut cut = share;
                    while cut > 0 && !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text = &text[..cut];
                    truncated = true;
                }
                json!({ "size": size, "truncated": truncated, "content": text })
            }
            None => {
                // Byte-truncate to 3/4 of the share before encoding (base64
                // expands 4/3), never below one byte, so a tiny wire budget
                // cannot silently disable the cap.
                let mut raw = source_capped;
                if share > 0 {
                    let byte_cap = std::cmp::max(1, share.saturating_mul(3) / 4);
                    if raw.len() > byte_cap {
                        raw = &raw[..byte_cap];
                        truncated = true;
                    }
                }
                json!({
                    "size": size,
                    "truncated": truncated,
                    "content_b64": Base64::encode_string(raw),
                })
            }
        };
        hosts_json.insert(host.clone(), entry);
    }

    Ok(json!({ "remote": remote, "hosts": Value::Object(hosts_json) }))
}

/// Where a `put` payload comes from; exactly one per call.
enum PutPayload {
    Content(String),
    ContentB64(String),
    Source(String),
}

/// The resolved path and fingerprint of a server-side `source` upload.
struct SourceUpload {
    path: PathBuf,
    sha256: String,
}

/// The directories as the caller sees them in a refusal.
fn listed(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve `source` to the canonical path of a file strictly inside one of
/// `dirs`.
///
/// The lexical containment check runs first and touches nothing the caller
/// named, so a path outside every directory is refused identically whether or
/// not it exists; only then is the path canonicalised and re-checked, which
/// catches a symlink that leaves the directory.
fn resolve_put_source(dirs: &[PathBuf], source: &str) -> Result<PathBuf, McpCommandError> {
    if dirs.is_empty() {
        return Err(refuse(
            "`source` is disabled: no [mcp] put_dirs is configured",
        ));
    }
    let path = Path::new(source);
    if !path.is_absolute() {
        return Err(refuse(format!(
            "`source` must be an absolute path: {source:?}"
        )));
    }
    let outside = || {
        refuse(format!(
            "`source` {source:?} is not inside an allowed directory (put_dirs: {})",
            listed(dirs)
        ))
    };
    let canonical_dirs: Vec<PathBuf> = dirs.iter().filter_map(|d| d.canonicalize().ok()).collect();
    let strictly_under = |p: &Path, set: &[PathBuf]| set.iter().any(|d| p != d && p.starts_with(d));

    let lexical = normalize(path);
    if !strictly_under(&lexical, dirs) && !strictly_under(&lexical, &canonical_dirs) {
        return Err(outside());
    }
    let resolved = path
        .canonicalize()
        .map_err(|e| refuse(format!("cannot resolve source {source:?}: {e}")))?;
    if !strictly_under(&resolved, &canonical_dirs) {
        return Err(outside());
    }
    Ok(resolved)
}

/// Open `path` for reading without following a final symlink or blocking on a
/// FIFO.
#[cfg(unix)]
fn open_source(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(unix))]
fn open_source(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Read a regular file in one open, so the bytes that are size-checked, hashed
/// and uploaded are the same buffer. `max_input` of `0` disables the cap.
fn read_put_source(path: &Path, max_input: usize) -> Result<Vec<u8>, McpCommandError> {
    use std::io::Read as _;

    let over = |len: u64| {
        refuse(format!(
            "source is {len} bytes, above the [mcp] max_input_bytes={max_input} budget — \
             truncating an upload would corrupt it, so it is refused"
        ))
    };
    let io_err = |e: std::io::Error| refuse(format!("cannot read source {}: {e}", path.display()));
    let file = open_source(path).map_err(io_err)?;
    let meta = file.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Err(refuse(format!(
            "source {} is not a regular file",
            path.display()
        )));
    }
    let cap = u64::try_from(max_input).unwrap_or(u64::MAX);
    if max_input > 0 && meta.len() > cap {
        return Err(over(meta.len()));
    }
    // A file that grows after the `fstat` is still bounded.
    let limit = if max_input > 0 {
        cap.saturating_add(1)
    } else {
        u64::MAX
    };
    let mut buf = Vec::new();
    file.take(limit).read_to_end(&mut buf).map_err(io_err)?;
    if max_input > 0 && buf.len() as u64 > cap {
        return Err(over(buf.len() as u64));
    }
    Ok(buf)
}

/// The in-band `put` body.
async fn transfer_put(
    session: &McpSession,
    filename: &str,
    payload: PutPayload,
    template: Option<&str>,
) -> Result<Value, McpCommandError> {
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename == "."
        || filename == ".."
    {
        return Err(refuse(
            "filename must be a bare file name (no path separators or `..`); the file \
             is placed under the report's remote working directory",
        ));
    }

    let max_input = session.max_input_bytes();
    let (payload, source): (Vec<u8>, Option<SourceUpload>) = match payload {
        PutPayload::Source(src) => {
            // Before `transfer_fork`: a refused source takes no lock and
            // transfers nothing.
            let dirs = session.put_dirs().to_vec();
            let (path, bytes) = tokio::task::spawn_blocking(move || {
                let path = resolve_put_source(&dirs, &src)?;
                let bytes = read_put_source(&path, max_input)?;
                Ok::<_, McpCommandError>((path, bytes))
            })
            .await
            .map_err(|e| refuse(format!("source read task failed: {e}")))??;
            let sha256 = sha256_hex(&bytes);
            (bytes, Some(SourceUpload { path, sha256 }))
        }
        inline => {
            let bytes = match inline {
                PutPayload::Content(text) => text.into_bytes(),
                PutPayload::ContentB64(b64) => Base64::decode_vec(&b64)
                    .map_err(|e| refuse(format!("content_b64 is not valid base64: {e}")))?,
                PutPayload::Source(_) => unreachable!("handled above"),
            };
            if max_input > 0 && bytes.len() > max_input {
                return Err(refuse(format!(
                    "payload is {} bytes, above the [mcp] max_input_bytes={max_input} budget — \
                     truncating an upload would corrupt it, so it is refused",
                    bytes.len()
                )));
            }
            (bytes, None)
        }
    };

    // Dispatch on a fork, so the SFTP fan-out does not hold the canonical
    // session mutex.
    let (scope, mut call_session) = transfer_fork(session, template).await?;
    // Destination from the activated fork — the same source the REPL `put`
    // uses — so a `target_tempdir` of another loaded report cannot leak in.
    let remote = call_session.metadata().target_wd(&[filename]);
    let map = call_session
        .targets_mut()
        .sftp_put_bytes(&payload, &remote)
        .await;
    // The fork (and its entry guard) is done: release it and the scope
    // before formatting — both only guarded the fan-out, and the rest works
    // on owned data.
    drop(call_session);
    drop(scope);

    if map.is_empty() {
        return Err(refuse(
            "no enabled hosts connected — load a template with hosts, add_host, or \
             enable a host first",
        ));
    }
    let failures: Vec<String> = map
        .iter()
        .filter_map(|(host, res)| res.as_ref().err().map(|e| format!("{host} ({e})")))
        .collect();
    if !failures.is_empty() {
        return Err(refuse(format!("upload failed on: {}", failures.join(", "))));
    }

    let hosts_json: Map<String, Value> =
        map.keys().map(|host| (host.clone(), json!("ok"))).collect();
    let mut reply = json!({
        "remote": remote.display().to_string(),
        "hosts": Value::Object(hosts_json),
    });
    if let Some(src) = source {
        reply["source"] = json!({
            "path": src.path.display().to_string(),
            "bytes": payload.len(),
            "sha256": src.sha256,
        });
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use mtui_config::Config;
    use mtui_hosts::Target;
    use mtui_hosts::connection::{MockConnection, MockSftpOp};
    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;
    use mtui_types::TargetState;

    const RRID: &str = "SUSE:Maintenance:1:1";

    /// An `McpSession` over a fresh temp `template_dir`; the handle keeps the
    /// dir alive.
    fn session_with(
        config_tweak: impl FnOnce(&mut Config),
    ) -> (std::sync::Arc<McpSession>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.template_dir = tmp.path().to_path_buf();
        config_tweak(&mut config);
        (McpSession::new(config), tmp)
    }

    /// Load an `ObsReport` for `rrid` (log file created on disk, made active)
    /// and install the given mock-backed targets.
    async fn load_with_targets(
        session: &McpSession,
        tmp: &tempfile::TempDir,
        rrid: &str,
        targets: Vec<Target>,
    ) {
        let path = tmp.path().join(rrid).join("log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "log\n").unwrap();
        let mut guard = session.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        report.base_mut().path = Some(path);
        guard.templates.add(Box::new(report));
        // `activate`, not bare `set_active`: it installs the active guard, so
        // `targets_mut()` reaches THIS report's group, not the null object.
        assert!(guard.activate(rrid).is_active(), "activate {rrid}");
        for t in targets {
            guard.targets_mut().add(t);
        }
    }

    fn enabled(host: &str, conn: MockConnection) -> Target {
        Target::with_connection(host, TargetState::Enabled, Box::new(conn))
    }

    async fn call(
        session: &McpSession,
        name: &str,
        kwargs: Value,
    ) -> Result<Value, McpCommandError> {
        let Value::Object(map) = kwargs else {
            panic!("kwargs must be an object")
        };
        dispatch_transfer_tool(session, name, &map, None).await
    }

    /// #667: a transfer on one template must not queue behind activity on
    /// another. Holding template B's scoped lock (shared gate + B's entry, as
    /// a long dispatch would), a `put` to template A must still complete
    /// instead of waiting for the exclusive gate.
    #[tokio::test]
    async fn put_on_idle_template_does_not_wait_for_busy_template() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            "SUSE:Maintenance:1:1",
            vec![enabled("h1", MockConnection::new("h1"))],
        )
        .await;
        load_with_targets(
            &session,
            &tmp,
            "SUSE:Maintenance:2:2",
            vec![enabled("h2", MockConnection::new("h2"))],
        )
        .await;

        let _busy_b = session.scoped_lock(Some("SUSE:Maintenance:2:2")).await;
        let v = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            call(
                &session,
                "put",
                json!({ "filename": "f", "content": "x", "template": "SUSE:Maintenance:1:1" }),
            ),
        )
        .await
        .expect("put on A must not wait behind B")
        .unwrap();
        assert_eq!(v["hosts"], json!({ "h1": "ok" }), "only A's hosts: {v}");
    }

    /// A same-template put waits for the template lock instead of failing:
    /// holding this template's scoped lock first, a concurrent put to it must
    /// stay parked while the holder is held, then succeed once it drops —
    /// never refuse as busy.
    #[tokio::test]
    async fn same_template_put_waits_for_the_template_lock() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled("h1", MockConnection::new("h1"))],
        )
        .await;

        let holder = session.scoped_lock(Some(RRID)).await;
        let mut put = tokio::spawn({
            let session = Arc::clone(&session);
            async move { call(&session, "put", json!({ "filename": "f", "content": "x" })).await }
        });
        // Let the spawned put reach the lock, then assert it is still parked
        // on the holder: finished-or-refused here means the scope lock was
        // never taken.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut put)
                .await
                .is_err(),
            "put must stay parked on the held template lock"
        );
        drop(holder);
        let v = tokio::time::timeout(Duration::from_secs(10), put)
            .await
            .expect("put must not hang")
            .expect("spawn ok")
            .unwrap();
        assert_eq!(v["hosts"]["h1"], "ok");
    }

    /// Short spellings take the same per-RRID lock as the full RRID: holding
    /// the canonical lock, a `put` naming `S:M:1:1` must stay parked until the
    /// holder drops. Without scope-key canonicalization the short spelling
    /// keys a different lock and the put sails through.
    #[tokio::test]
    async fn short_template_spelling_shares_the_template_lock() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled("h1", MockConnection::new("h1"))],
        )
        .await;

        let holder = session.scoped_lock(Some(RRID)).await;
        let mut put = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                call(
                    &session,
                    "put",
                    json!({ "filename": "f", "content": "x", "template": "S:M:1:1" }),
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut put)
                .await
                .is_err(),
            "short-spelled put must park on the canonical template lock"
        );
        drop(holder);
        let v = tokio::time::timeout(Duration::from_secs(10), put)
            .await
            .expect("put must not hang")
            .expect("spawn ok")
            .unwrap();
        assert_eq!(v["hosts"]["h1"], "ok");
    }

    /// The upload destination is read off the activated fork — the same resolve
    /// that keyed the scope lock — so a put naming template A lands under A's
    /// target working directory even when the canonical session's config
    /// points elsewhere.
    #[tokio::test]
    async fn put_destination_comes_from_the_target_template() {
        use std::path::PathBuf;

        let (session, tmp) = session_with(|c| {
            c.target_tempdir = PathBuf::from("/tmp/tpl-a");
        });
        let h1 = MockConnection::new("h1");
        let h1handle = h1.clone();
        load_with_targets(
            &session,
            &tmp,
            "SUSE:Maintenance:1:1",
            vec![enabled("h1", h1)],
        )
        .await;
        {
            let mut guard = session.session().lock().await;
            guard.config.target_tempdir = PathBuf::from("/tmp/tpl-b");
        }
        let h2 = MockConnection::new("h2");
        let h2handle = h2.clone();
        load_with_targets(
            &session,
            &tmp,
            "SUSE:Maintenance:2:2",
            vec![enabled("h2", h2)],
        )
        .await;

        call(
            &session,
            "put",
            json!({ "filename": "f", "content": "x", "template": "SUSE:Maintenance:1:1" }),
        )
        .await
        .unwrap();

        let puts: Vec<PathBuf> = h1handle
            .sftp_ops()
            .into_iter()
            .filter_map(|op| match op {
                MockSftpOp::PutBytes { remote, .. } => Some(remote),
                _ => None,
            })
            .collect();
        assert_eq!(puts, vec![PathBuf::from("/tmp/tpl-a/f")], "{puts:?}");
        assert!(
            h2handle
                .sftp_ops()
                .iter()
                .all(|op| !matches!(op, MockSftpOp::PutBytes { .. })),
            "nothing uploaded to the non-target template"
        );
    }

    // ---- get -------------------------------------------------------------

    /// The defect pin: content arrives in-band and DISTINCT per host (identical
    /// fixtures could pass with one host's bytes for all), and nothing lands
    /// under the report checkout's downloads/ dir.
    #[tokio::test]
    async fn get_returns_per_host_distinct_content_in_band() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![
                enabled(
                    "h1",
                    MockConnection::new("h1").with_file("/var/log/f", b"alpha-1".to_vec()),
                ),
                enabled(
                    "h2",
                    MockConnection::new("h2").with_file("/var/log/f", b"beta-2".to_vec()),
                ),
            ],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/var/log/f" }))
            .await
            .unwrap();

        assert_eq!(v["remote"], "/var/log/f");
        assert_eq!(v["hosts"]["h1"]["content"], "alpha-1");
        assert_eq!(v["hosts"]["h2"]["content"], "beta-2");
        assert_eq!(v["hosts"]["h1"]["truncated"], false);
        assert_eq!(v["hosts"]["h1"]["size"], 7);
        assert_eq!(
            v["hosts"]["h2"]["size"], 6,
            "per-host size, not the first host's"
        );
        // No server-side download artifacts (#434's original defect).
        let downloads = tmp.path().join(RRID).join("downloads");
        assert!(
            !downloads.exists(),
            "no files may be written: {downloads:?}"
        );
    }

    #[tokio::test]
    async fn get_binary_content_is_b64() {
        let payload = vec![0xFF, 0xFE, 0x00, 0x01];
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled(
                "h1",
                MockConnection::new("h1").with_file("/f.bin", payload.clone()),
            )],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/f.bin" }))
            .await
            .unwrap();

        let entry = &v["hosts"]["h1"];
        assert!(
            entry.get("content").is_none(),
            "binary must not be lossy text: {entry}"
        );
        let b64 = entry["content_b64"].as_str().unwrap();
        assert_eq!(Base64::decode_vec(b64).unwrap(), payload);
        assert_eq!(entry["truncated"], false);
    }

    #[tokio::test]
    async fn get_source_cap_truncates_with_flag() {
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = 4);
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled(
                "h1",
                MockConnection::new("h1").with_file("/f", b"abcdefgh".to_vec()),
            )],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap();

        let entry = &v["hosts"]["h1"];
        assert_eq!(entry["size"], 8, "size is the full remote length");
        assert_eq!(entry["truncated"], true, "the clip must be explicit");
        assert_eq!(entry["content"], "abcd");
    }

    /// The source budget must bound the remote read itself: a capped `get`
    /// never falls back to the whole-file `Open`, whichever fan-out branch ran.
    async fn assert_capped_get_avoids_whole_file_read(kwargs: Value) {
        let m1 = MockConnection::new("h1").with_file("/f", b"abcdefghij".to_vec());
        let h1 = m1.clone();
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = 4);
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", m1)]).await;

        let v = call(&session, "get", kwargs).await.unwrap();

        let entry = &v["hosts"]["h1"];
        assert_eq!(entry["size"], 10);
        assert_eq!(entry["truncated"], true);
        assert_eq!(entry["content"], "abcd");
        let ops = h1.sftp_ops();
        assert!(
            !ops.iter().any(|op| matches!(op, MockSftpOp::Open(_))),
            "a capped get must not read the whole file: {ops:?}"
        );
        assert_eq!(
            ops,
            [MockSftpOp::ReadCapped {
                path: "/f".into(),
                max_bytes: 4
            }]
        );
    }

    #[tokio::test]
    async fn get_all_hosts_source_cap_never_reads_whole_file() {
        assert_capped_get_avoids_whole_file_read(json!({ "remote": "/f" })).await;
    }

    #[tokio::test]
    async fn get_selected_hosts_source_cap_never_reads_whole_file() {
        assert_capped_get_avoids_whole_file_read(json!({ "remote": "/f", "hosts": ["h1"] })).await;
    }

    /// One host, one file, one `get` under the given source/wire budgets.
    async fn get_one(
        file: &[u8],
        max_input: usize,
        size_attr: Option<Option<u64>>,
    ) -> Result<Value, McpCommandError> {
        let mut conn = MockConnection::new("h1").with_file("/f", file.to_vec());
        if let Some(attr) = size_attr {
            conn = conn.with_size_attr("/f", attr);
        }
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = max_input);
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", conn)]).await;
        call(&session, "get", json!({ "remote": "/f" })).await
    }

    #[tokio::test]
    async fn get_source_cap_reports_truncation_only_when_bytes_were_omitted() {
        for (file, size, truncated, content) in [
            (&b""[..], 0, false, ""),
            (b"abcd", 4, false, "abcd"),
            (b"abcde", 5, true, "abcd"),
        ] {
            let v = get_one(file, 4, None).await.unwrap();
            let e = &v["hosts"]["h1"];
            assert_eq!(e["size"], size, "{file:?}");
            assert_eq!(e["truncated"], truncated, "{file:?}");
            assert_eq!(e["content"], content, "{file:?}");
        }
    }

    #[tokio::test]
    async fn get_zero_source_cap_is_unlimited() {
        let v = get_one(b"abcdefghij", 0, None).await.unwrap();

        let e = &v["hosts"]["h1"];
        assert_eq!(e["size"], 10);
        assert_eq!(e["truncated"], false);
        assert_eq!(e["content"], "abcdefghij");
    }

    #[tokio::test]
    async fn get_source_cap_inside_codepoint_trims_text_and_reports_full_size() {
        let v = get_one("A€B".as_bytes(), 2, None).await.unwrap();

        let e = &v["hosts"]["h1"];
        assert_eq!(e["size"], 5);
        assert_eq!(e["truncated"], true);
        assert_eq!(e["content"], "A");
        assert!(e.get("content_b64").is_none(), "must stay text: {e}");
    }

    #[tokio::test]
    async fn get_source_cap_on_binary_returns_the_prefix_as_b64() {
        let v = get_one(&[0xff, 0, 1, 2, 3], 3, None).await.unwrap();

        let e = &v["hosts"]["h1"];
        assert_eq!(e["size"], 5);
        assert_eq!(e["truncated"], true);
        assert_eq!(e["content_b64"], "/wAB");
        assert!(e.get("content").is_none(), "binary must not be text: {e}");
    }

    #[tokio::test]
    async fn get_source_cap_reports_the_server_size_not_the_prefix_length() {
        let v = get_one(b"abcde", 4, Some(Some(100))).await.unwrap();

        let e = &v["hosts"]["h1"];
        assert_eq!(e["size"], 100);
        assert_eq!(e["truncated"], true);
        assert_eq!(e["content"], "abcd");
    }

    #[tokio::test]
    async fn get_source_cap_without_size_attr_fails_attributed_and_never_reads_whole_file() {
        let m1 = MockConnection::new("h1")
            .with_file("/f", b"abcde".to_vec())
            .with_size_attr("/f", None);
        let h1 = m1.clone();
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = 4);
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", m1)]).await;

        let err = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap_err();

        assert!(err.stderr.contains("download failed on"), "{}", err.stderr);
        assert!(
            err.stderr.contains("h1 (") && err.stderr.contains("no size"),
            "{}",
            err.stderr
        );
        assert!(
            !h1.sftp_ops()
                .iter()
                .any(|op| matches!(op, MockSftpOp::Open(_))),
            "must not fall back to a whole-file read"
        );
    }

    /// The source budget is per host and the wire budget an equal share, so the
    /// two layers compose: 8 retained bytes each, 3 on the wire each.
    #[tokio::test]
    async fn get_source_and_wire_budgets_compose_per_host() {
        let (session, tmp) = session_with(|c| {
            c.mcp_max_input_bytes = 8;
            c.mcp_max_output_bytes = 6;
        });
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![
                enabled(
                    "h1",
                    MockConnection::new("h1").with_file("/f", vec![b'a'; 20]),
                ),
                enabled(
                    "h2",
                    MockConnection::new("h2").with_file("/f", vec![b'b'; 20]),
                ),
            ],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap();

        for (host, content) in [("h1", "aaa"), ("h2", "bbb")] {
            let e = &v["hosts"][host];
            assert_eq!(e["size"], 20, "{host}");
            assert_eq!(e["truncated"], true, "{host}");
            assert_eq!(
                e["content"], content,
                "{host}: equal share of the wire budget"
            );
        }
    }

    #[tokio::test]
    async fn get_one_host_failure_fails_call_with_attribution() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![
                enabled(
                    "h1",
                    MockConnection::new("h1").with_file("/f", b"alpha".to_vec()),
                ),
                // h2: no seeded file -> SftpNotFound.
                enabled("h2", MockConnection::new("h2")),
            ],
        )
        .await;

        let err = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap_err();

        assert!(err.stderr.contains("download failed on"), "{}", err.stderr);
        assert!(
            err.stderr.contains("h2 ("),
            "failing host named: {}",
            err.stderr
        );
        assert!(
            !err.stderr.contains("h1 ("),
            "healthy host not blamed: {}",
            err.stderr
        );
    }

    #[tokio::test]
    async fn get_unknown_host_refused_and_nothing_transferred() {
        let m1 = MockConnection::new("h1").with_file("/f", b"alpha".to_vec());
        let h1 = m1.clone();
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", m1)]).await;

        let err = call(
            &session,
            "get",
            json!({ "remote": "/f", "hosts": ["h1", "nope"] }),
        )
        .await
        .unwrap_err();

        assert!(
            err.stderr.contains("unknown host(s): nope"),
            "{}",
            err.stderr
        );
        assert!(
            h1.sftp_ops().is_empty(),
            "refusal must precede any transfer"
        );
    }

    #[tokio::test]
    async fn get_explicitly_named_disabled_host_refused() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![
                enabled(
                    "h1",
                    MockConnection::new("h1").with_file("/f", b"alpha".to_vec()),
                ),
                Target::with_connection(
                    "h2",
                    TargetState::Disabled,
                    Box::new(MockConnection::new("h2").with_file("/f", b"beta".to_vec())),
                ),
            ],
        )
        .await;

        let err = call(
            &session,
            "get",
            json!({ "remote": "/f", "hosts": ["h1", "h2"] }),
        )
        .await
        .unwrap_err();

        assert!(
            err.stderr.contains("disabled") && err.stderr.contains("h2"),
            "{}",
            err.stderr
        );
    }

    #[tokio::test]
    async fn get_no_hosts_refuses() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;

        let err = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap_err();

        assert!(err.stderr.contains("no enabled hosts"), "{}", err.stderr);
    }

    #[tokio::test]
    async fn get_trailing_slash_refuses() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;

        let err = call(&session, "get", json!({ "remote": "/var/log/" }))
            .await
            .unwrap_err();

        assert!(err.stderr.contains("folder downloads"), "{}", err.stderr);
    }

    #[tokio::test]
    async fn get_two_templates_without_template_refuses() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;
        load_with_targets(&session, &tmp, "SUSE:Maintenance:2:2", vec![]).await;

        let err = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap_err();

        assert!(
            err.stderr.contains("more than one template"),
            "{}",
            err.stderr
        );
    }

    #[tokio::test]
    async fn get_unknown_kwarg_refused() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;

        let err = call(&session, "get", json!({ "remote": "/f", "hots": ["h1"] }))
            .await
            .unwrap_err();

        assert!(
            err.stderr.contains("hots"),
            "misspelling named: {}",
            err.stderr
        );
    }

    // ---- put -------------------------------------------------------------

    #[tokio::test]
    async fn put_content_reaches_every_host() {
        let (m1, m2) = (MockConnection::new("h1"), MockConnection::new("h2"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled("h1", m1), enabled("h2", m2)],
        )
        .await;

        let v = call(
            &session,
            "put",
            json!({ "filename": "payload.txt", "content": "data-434" }),
        )
        .await
        .unwrap();

        assert_eq!(v["hosts"]["h1"], "ok");
        assert_eq!(v["hosts"]["h2"], "ok");
        let remote = v["remote"].as_str().unwrap();
        assert!(
            remote.ends_with("/payload.txt"),
            "remote is target_wd/filename: {remote}"
        );
        for handle in [&h1, &h2] {
            let ops = handle.sftp_ops();
            assert_eq!(ops.len(), 1, "{ops:?}");
            assert!(
                matches!(
                    &ops[0],
                    MockSftpOp::PutBytes { len, data, remote: r }
                        if *len == "data-434".len()
                            && data == b"data-434"
                            && r.ends_with("payload.txt")
                ),
                "{ops:?}"
            );
        }
    }

    #[tokio::test]
    async fn put_b64_roundtrip() {
        let payload = vec![0u8, 159, 146, 150];
        let m1 = MockConnection::new("h1");
        let h1 = m1.clone();
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", m1)]).await;

        call(
            &session,
            "put",
            json!({ "filename": "b.bin", "content_b64": Base64::encode_string(&payload) }),
        )
        .await
        .unwrap();

        let ops = h1.sftp_ops();
        assert!(
            matches!(&ops[0], MockSftpOp::PutBytes { data, .. } if *data == payload),
            "{ops:?}"
        );
    }

    #[tokio::test]
    async fn put_content_xor_b64_enforced() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;

        let neither = call(&session, "put", json!({ "filename": "f" }))
            .await
            .unwrap_err();
        assert!(neither.stderr.contains("is required"), "{}", neither.stderr);

        let both = call(
            &session,
            "put",
            json!({ "filename": "f", "content": "a", "content_b64": "YQ==" }),
        )
        .await
        .unwrap_err();
        assert!(both.stderr.contains("exactly one"), "{}", both.stderr);
    }

    #[tokio::test]
    async fn put_oversize_payload_refused_before_any_transfer() {
        let m1 = MockConnection::new("h1");
        let h1 = m1.clone();
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = 4);
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", m1)]).await;

        let err = call(
            &session,
            "put",
            json!({ "filename": "f", "content": "12345" }),
        )
        .await
        .unwrap_err();

        assert!(err.stderr.contains("max_input_bytes"), "{}", err.stderr);
        assert!(
            h1.sftp_ops().is_empty(),
            "a refused upload must transfer nothing"
        );
    }

    #[tokio::test]
    async fn put_filename_with_separator_or_dotdot_refused() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;

        for bad in ["a/b", "a\\b", "..", ".", ""] {
            let err = call(&session, "put", json!({ "filename": bad, "content": "x" }))
                .await
                .unwrap_err();
            assert!(
                err.stderr.contains("bare file name"),
                "{bad:?}: {}",
                err.stderr
            );
        }
    }

    #[tokio::test]
    async fn put_failure_attribution() {
        let ok = MockConnection::new("h1");
        let bad = MockConnection::new("h2").with_sftp_put_failure("disk full");
        let h1 = ok.clone();
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled("h1", ok), enabled("h2", bad)],
        )
        .await;

        let err = call(&session, "put", json!({ "filename": "f", "content": "x" }))
            .await
            .unwrap_err();

        assert!(err.stderr.contains("upload failed on"), "{}", err.stderr);
        assert!(
            err.stderr.contains("h2 (") && err.stderr.contains("disk full"),
            "{}",
            err.stderr
        );
        assert!(!err.stderr.contains("h1 ("), "{}", err.stderr);
        // The healthy host's upload still happened before the call failed.
        assert_eq!(h1.sftp_ops().len(), 1);
    }

    // ---- put source ------------------------------------------------------

    /// Two mock hosts under one loaded template, `[mcp] put_dirs` set to
    /// `<files>/allowed`, plus the `outside` sibling directory a refusal
    /// test points at.
    struct SourceEnv {
        session: Arc<McpSession>,
        files: tempfile::TempDir,
        h1: MockConnection,
        h2: MockConnection,
        _template_dir: tempfile::TempDir,
    }

    impl SourceEnv {
        async fn new(max_input: Option<usize>, with_put_dirs: bool) -> Self {
            let files = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(files.path().join("allowed")).unwrap();
            std::fs::create_dir_all(files.path().join("outside")).unwrap();
            let allowed = files.path().join("allowed");
            let (session, template_dir) = session_with(|c| {
                if with_put_dirs {
                    c.mcp_put_dirs = vec![allowed];
                }
                if let Some(max) = max_input {
                    c.mcp_max_input_bytes = max;
                }
            });
            let (h1, h2) = (MockConnection::new("h1"), MockConnection::new("h2"));
            load_with_targets(
                &session,
                &template_dir,
                RRID,
                vec![enabled("h1", h1.clone()), enabled("h2", h2.clone())],
            )
            .await;
            Self {
                session,
                files,
                h1,
                h2,
                _template_dir: template_dir,
            }
        }

        fn allowed(&self, name: &str) -> PathBuf {
            self.files.path().join("allowed").join(name)
        }

        fn outside(&self, name: &str) -> PathBuf {
            self.files.path().join("outside").join(name)
        }

        async fn put_source(&self, source: &Path) -> Result<Value, McpCommandError> {
            call(
                &self.session,
                "put",
                json!({ "source": source.to_str().unwrap() }),
            )
            .await
        }

        fn assert_nothing_transferred(&self) {
            for h in [&self.h1, &self.h2] {
                assert!(h.sftp_ops().is_empty(), "{:?}", h.sftp_ops());
            }
        }
    }

    /// The error text of a refused call, requiring the refusal.
    fn refusal(result: Result<Value, McpCommandError>) -> String {
        result.expect_err("the call must be refused").stderr
    }

    #[tokio::test]
    async fn put_source_uploads_the_exact_bytes_to_every_host_with_a_fingerprint() {
        let env = SourceEnv::new(None, true).await;
        let body = "#!/bin/sh\n# ünïcödé — 日本語\necho ok\n"
            .as_bytes()
            .to_vec();
        let file = env.allowed("host_baseline_check.sh");
        std::fs::write(&file, &body).unwrap();

        let v = env.put_source(&file).await.unwrap();

        assert_eq!(v["hosts"], json!({ "h1": "ok", "h2": "ok" }), "{v}");
        assert!(
            v["remote"]
                .as_str()
                .unwrap()
                .ends_with("/host_baseline_check.sh"),
            "remote ends with the basename: {v}"
        );
        for h in [&env.h1, &env.h2] {
            let ops = h.sftp_ops();
            assert_eq!(ops.len(), 1, "{ops:?}");
            assert!(
                matches!(
                    &ops[0],
                    MockSftpOp::PutBytes { len, data, remote }
                        if *len == body.len()
                            && *data == body
                            && remote.ends_with("host_baseline_check.sh")
                ),
                "{ops:?}"
            );
        }
        // `/var` is `/private/var` on macOS: compare against the canonical path.
        assert_eq!(
            v["source"]["path"],
            json!(file.canonicalize().unwrap().display().to_string())
        );
        assert_eq!(v["source"]["bytes"], json!(body.len()));
        // sha256 of the bytes, computed independently of `sha256_hex`.
        use sha2::{Digest as _, Sha256};
        let expected: String = Sha256::digest(&body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(v["source"]["sha256"], json!(expected));
    }

    #[tokio::test]
    async fn put_source_filename_overrides_the_basename() {
        let env = SourceEnv::new(None, true).await;
        let file = env.allowed("a.sh");
        std::fs::write(&file, "x").unwrap();

        let v = call(
            &env.session,
            "put",
            json!({ "source": file.to_str().unwrap(), "filename": "renamed.sh" }),
        )
        .await
        .unwrap();

        assert!(
            v["remote"].as_str().unwrap().ends_with("/renamed.sh"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn put_source_is_refused_when_put_dirs_is_unset() {
        let env = SourceEnv::new(None, false).await;
        let file = env.allowed("a.sh");
        std::fs::write(&file, "x").unwrap();

        let err = refusal(env.put_source(&file).await);

        assert!(
            err.contains("is disabled") && err.contains("put_dirs"),
            "{err}"
        );
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_in_a_sibling_directory_is_refused() {
        let env = SourceEnv::new(None, true).await;
        let file = env.outside("f");
        std::fs::write(&file, "x").unwrap();

        let err = refusal(env.put_source(&file).await);

        assert!(err.contains("not inside"), "{err}");
        assert!(err.contains("allowed"), "the refusal names put_dirs: {err}");
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_dotdot_out_of_the_directory_is_refused() {
        let env = SourceEnv::new(None, true).await;
        std::fs::write(env.outside("f"), "x").unwrap();
        let sneaky = env.allowed("../outside/f");

        let err = refusal(env.put_source(&sneaky).await);

        assert!(err.contains("not inside"), "{err}");
        env.assert_nothing_transferred();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn put_source_symlink_leaving_the_directory_is_refused() {
        let env = SourceEnv::new(None, true).await;
        std::fs::write(env.outside("secret"), "x").unwrap();
        let link = env.allowed("link");
        std::os::unix::fs::symlink(env.outside("secret"), &link).unwrap();

        let err = refusal(env.put_source(&link).await);

        assert!(err.contains("not inside"), "{err}");
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_outside_path_is_refused_whether_or_not_it_exists() {
        let env = SourceEnv::new(None, true).await;

        let err = refusal(env.put_source(&env.outside("does-not-exist")).await);

        assert!(
            err.contains("not inside") && !err.contains("cannot resolve"),
            "an absent path outside must not read differently from a present one: {err}"
        );
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_inside_but_missing_cannot_resolve() {
        let env = SourceEnv::new(None, true).await;

        let err = refusal(env.put_source(&env.allowed("absent")).await);

        assert!(err.contains("cannot resolve"), "{err}");
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_directory_is_not_a_regular_file() {
        let env = SourceEnv::new(None, true).await;
        let dir = env.allowed("sub");
        std::fs::create_dir(&dir).unwrap();

        let err = refusal(env.put_source(&dir).await);

        assert!(err.contains("not a regular file"), "{err}");
        env.assert_nothing_transferred();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn put_source_fifo_is_refused_without_blocking() {
        let env = SourceEnv::new(None, true).await;
        let fifo = env.allowed("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), env.put_source(&fifo))
            .await
            .expect("a FIFO must not block the open");

        let err = refusal(result);
        assert!(err.contains("not a regular file"), "{err}");
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_must_be_absolute() {
        let env = SourceEnv::new(None, true).await;

        let err = refusal(call(&env.session, "put", json!({ "source": "relative/f" })).await);

        assert!(err.contains("absolute"), "{err}");
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_above_the_input_cap_is_refused() {
        let env = SourceEnv::new(Some(4), true).await;
        let file = env.allowed("big");
        std::fs::write(&file, "12345").unwrap();

        let err = refusal(env.put_source(&file).await);

        assert!(err.contains("max_input_bytes"), "{err}");
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_source_at_the_input_cap_is_accepted() {
        let env = SourceEnv::new(Some(4), true).await;
        let file = env.allowed("edge");
        std::fs::write(&file, "1234").unwrap();

        let v = env.put_source(&file).await.unwrap();

        assert_eq!(v["source"]["bytes"], json!(4), "{v}");
    }

    #[tokio::test]
    async fn put_source_excludes_the_inline_payloads() {
        let env = SourceEnv::new(None, true).await;
        let file = env.allowed("a");
        std::fs::write(&file, "x").unwrap();
        let src = file.to_str().unwrap();

        for extra in [json!({ "content": "y" }), json!({ "content_b64": "eQ==" })] {
            let mut kwargs = json!({ "source": src, "filename": "f" });
            kwargs
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let err = refusal(call(&env.session, "put", kwargs).await);
            assert!(err.contains("exactly one"), "{err}");
        }
        env.assert_nothing_transferred();
    }

    #[tokio::test]
    async fn put_without_filename_or_source_names_the_missing_filename() {
        let env = SourceEnv::new(None, true).await;

        let err = refusal(call(&env.session, "put", json!({ "content": "x" })).await);

        assert!(err.contains("`filename` is required unless"), "{err}");
    }

    #[tokio::test]
    async fn put_no_hosts_refuses() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(&session, &tmp, RRID, vec![]).await;

        let err = call(&session, "put", json!({ "filename": "f", "content": "x" }))
            .await
            .unwrap_err();

        assert!(err.stderr.contains("no enabled hosts"), "{}", err.stderr);
    }

    #[tokio::test]
    async fn get_wire_cap_shares_budget_across_hosts() {
        // 2 hosts, 40-byte wire budget -> 20-byte share each. Both files are
        // longer, with DISTINCT content so a swapped-share bug is visible.
        let (session, tmp) = session_with(|c| c.mcp_max_output_bytes = 40);
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![
                enabled(
                    "h1",
                    MockConnection::new("h1").with_file("/f", vec![b'a'; 30]),
                ),
                enabled(
                    "h2",
                    MockConnection::new("h2").with_file("/f", vec![b'b'; 50]),
                ),
            ],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap();

        for (host, ch, size) in [("h1", "a", 30), ("h2", "b", 50)] {
            let e = &v["hosts"][host];
            assert_eq!(e["size"], size);
            assert_eq!(e["truncated"], true, "{host}: a cut must be flagged");
            let content = e["content"].as_str().unwrap();
            assert_eq!(content.len(), 20, "{host}: equal share of the budget");
            assert!(content.chars().all(|c| c.to_string() == ch), "{host}");
            assert!(
                !content.contains("truncated"),
                "no notice spliced into content"
            );
        }
    }

    #[tokio::test]
    async fn get_b64_wire_cap_truncates_with_flag() {
        // Binary payload + 40-byte budget, single host: share 40 -> byte cap 30.
        let payload: Vec<u8> = (0..200u16)
            .map(|i| (i % 251) as u8)
            .map(|b| b | 0x80)
            .collect();
        let (session, tmp) = session_with(|c| c.mcp_max_output_bytes = 40);
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled(
                "h1",
                MockConnection::new("h1").with_file("/f.bin", payload.clone()),
            )],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/f.bin" }))
            .await
            .unwrap();

        let e = &v["hosts"]["h1"];
        assert_eq!(e["truncated"], true);
        assert_eq!(e["size"], 200);
        let raw = Base64::decode_vec(e["content_b64"].as_str().unwrap()).unwrap();
        assert_eq!(raw, payload[..30], "byte cap = share*3/4");
    }

    /// A mid-codepoint source-cap cut stays textual, trimmed to the last whole
    /// codepoint, rather than being reclassified as binary.
    #[tokio::test]
    async fn get_source_cap_mid_codepoint_stays_text() {
        // "aaaé" = 61 61 61 c3 a9; cap 4 cuts inside the two-byte 'é'.
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = 4);
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled(
                "h1",
                MockConnection::new("h1").with_file("/f", "aaaé".as_bytes().to_vec()),
            )],
        )
        .await;

        let v = call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap();

        let e = &v["hosts"]["h1"];
        assert_eq!(e["content"], "aaa", "trimmed to the last whole codepoint");
        assert!(
            e.get("content_b64").is_none(),
            "must not flip to binary: {e}"
        );
        assert_eq!(e["truncated"], true);
    }

    /// A payload exactly at the cap is accepted (`>` not `>=`).
    #[tokio::test]
    async fn put_at_cap_boundary_accepted() {
        let m1 = MockConnection::new("h1");
        let h1 = m1.clone();
        let (session, tmp) = session_with(|c| c.mcp_max_input_bytes = 4);
        load_with_targets(&session, &tmp, RRID, vec![enabled("h1", m1)]).await;

        call(
            &session,
            "put",
            json!({ "filename": "f", "content": "1234" }),
        )
        .await
        .unwrap();

        assert_eq!(h1.sftp_ops().len(), 1);
    }

    /// A disabled host is absent from the result and receives nothing.
    #[tokio::test]
    async fn put_disabled_host_absent_and_untouched() {
        let m1 = MockConnection::new("h1");
        let m2 = MockConnection::new("h2");
        let h2 = m2.clone();
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![
                enabled("h1", m1),
                Target::with_connection("h2", TargetState::Disabled, Box::new(m2)),
            ],
        )
        .await;

        let v = call(&session, "put", json!({ "filename": "f", "content": "x" }))
            .await
            .unwrap();

        assert_eq!(v["hosts"]["h1"], "ok");
        assert!(
            v["hosts"].get("h2").is_none(),
            "disabled host must be absent: {v}"
        );
        assert!(
            h2.sftp_ops().is_empty(),
            "disabled host must not be touched"
        );
    }

    /// After a transfer call the canonical session must hold NO active entry
    /// guard, or the next scoped (forked) command's `try_lock_owned` fails and
    /// that command is refused as `template busy`.
    #[tokio::test]
    async fn transfer_leaves_no_active_guard() {
        let (session, tmp) = session_with(|_| {});
        load_with_targets(
            &session,
            &tmp,
            RRID,
            vec![enabled(
                "h1",
                MockConnection::new("h1").with_file("/f", b"x".to_vec()),
            )],
        )
        .await;
        // The loader installs a guard, so the tool must clean up regardless.
        session.session().lock().await.release_active_guard();

        call(&session, "get", json!({ "remote": "/f" }))
            .await
            .unwrap();

        let guard = session.session().lock().await;
        assert!(
            !guard.active_report_is_guarded(RRID),
            "transfer tools must release the entry guard between calls"
        );
        assert_eq!(
            guard.templates.active_rrid(),
            Some(RRID),
            "the active pointer itself is restored"
        );
    }

    // ---- surface ---------------------------------------------------------

    #[test]
    fn descriptors_are_get_and_put_with_honest_hints() {
        let d = transfer_tool_descriptors();
        let names: Vec<&str> = d.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["get", "put"]);
        assert!(d[0].read_only, "get writes nothing anywhere");
        assert!(!d[1].read_only, "put mutates the hosts");
    }
}
