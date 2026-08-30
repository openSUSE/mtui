//! Hand-written in-band `get`/`put` MCP tools (#434).
//!
//! The synthesised forms of these two commands exchange **server-local paths**:
//! `get` downloaded to `{report_wd}/downloads/` and returned the path, `put`
//! uploaded a file that had to already exist on the server. Under
//! `--transport http` the client is on another machine, so those paths are
//! unreachable and `put` is unusable outright. Both commands are therefore on
//! [`MCP_DENYLIST`](mtui_core::MCP_DENYLIST) and re-served here under the same
//! names — the `edit` → `testreport_*` precedent — carrying the content
//! **in-band** in both directions, on both transports.
//!
//! The REPL/CLI `get`/`put` commands are untouched: literal paths (#399), the
//! `{report_wd}/downloads/{name}.{host}` layout, and their messages all stay.
//!
//! Content encoding: UTF-8 payloads travel as `content`, anything else as
//! `content_b64`. Downloads are capped twice — per host at
//! `[mcp] max_input_bytes` (applied *after* the transfer, which buffers the whole
//! remote file per host, so point `get` at logs and configs, not images), then at
//! an equal share of `[mcp] max_output_bytes` on the wire — with an explicit
//! `truncated` flag and the full remote `size`, never a silent clip.

use std::collections::BTreeSet;
use std::path::Path;

use base64ct::{Base64, Encoding};
use mtui_core::{SingleTemplate, ambiguous_template_message};
use serde_json::{Map, Value, json};

use crate::session::{
    DEFAULT_PROGRESS_INTERVAL, McpCommandError, McpSession, ProgressSink, run_with_heartbeat,
};
use crate::tools::ToolDescriptor;

/// Shared "which template" note, kept textually aligned with the testreport
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
            "Download a file from every enabled reference host and return its content \
             in-band, per host: UTF-8 content as `content`, binary as `content_b64`, \
             always with the full remote `size` and a `truncated` flag (per-host reads \
             are capped at [mcp] max_input_bytes and each host gets an equal share of \
             [mcp] max_output_bytes on the wire). Pass `hosts` to retry or page a \
             subset. Folder paths (trailing `/`) are not supported in-band. Any host \
             failure fails the call and names the host. {TEMPLATE_NOTE}"
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
            "Upload a payload carried in the call to every enabled reference host, at \
             `<target tempdir>/<filename>` — the same place the REPL `put` command \
             uploads to. Pass the payload as `content` (UTF-8) or `content_b64` \
             (base64), exactly one of the two; payloads above [mcp] max_input_bytes \
             are refused rather than truncated. Any host failure fails the call and \
             names the host. {TEMPLATE_NOTE}"
        ),
        input_schema: schema(
            vec![
                (
                    "filename",
                    json!({ "type": "string", "description": "Bare file name to create on the hosts (no path separators)." }),
                ),
                (
                    "content",
                    json!({ "type": "string", "description": "UTF-8 payload; exactly one of content/content_b64." }),
                ),
                (
                    "content_b64",
                    json!({ "type": "string", "description": "Base64 payload; exactly one of content/content_b64." }),
                ),
                ("template", template_prop()),
            ],
            &["filename"],
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
            let filename = opt_str(kwargs, "filename")?
                .ok_or_else(|| refuse("`filename` is required"))?
                .to_owned();
            let content = opt_str(kwargs, "content")?.map(str::to_owned);
            let content_b64 = opt_str(kwargs, "content_b64")?.map(str::to_owned);
            transfer_put(session, &filename, content, content_b64, template).await
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
/// when more than one is loaded with no `template`, as the testreport tools do.
/// The ambiguous case shares its wording with every other surface via
/// [`mtui_core::ambiguous_template_message`]; the other two refusals are this
/// tool family's own, pre-existing and grepped.
fn resolve_rrid(
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

/// Restores the previously-active template *pointer* after a transfer
/// fan-out, then releases the entry guard.
///
/// The release is load-bearing (the `run_command` exclusive-arm contract): the
/// session must hold no active guard between calls, or a later forked (scoped)
/// call's `try_lock_owned` fails in `activate` and that command is refused as
/// `template busy` (#524) — before which it silently answered from the null
/// report instead.
fn restore_active(session: &mut mtui_core::Session, prev: Option<String>) {
    match prev {
        Some(rrid) => {
            let _ = session.activate(&rrid);
        }
        // Nothing was active before: clear the pointer again. An empty rrid
        // always reports `false`; there is no guard to want here.
        None => {
            let _ = session.activate("");
        }
    }
    session.release_active_guard();
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

    // Exclusive gate for fan-out parity with the synthesised commands.
    let _lock = session.exclusive_lock().await;
    let mut guard = session.session().lock().await;
    let rrid = resolve_rrid(&guard, template)?;
    let prev = guard.templates.active_rrid().map(str::to_owned);
    if !guard.activate(&rrid).is_active() {
        restore_active(&mut guard, prev);
        return Err(refuse(format!("could not activate template {rrid}")));
    }

    let outcome = {
        let targets = guard.targets_mut();
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
                restore_active(&mut guard, prev);
                return Err(refuse(format!(
                    "unknown host(s): {} (connected: {})",
                    unknown.join(", "),
                    known.iter().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        let map = match hosts {
            Some(ref names) => targets.sftp_read_selected(Path::new(remote), names).await,
            None => targets.sftp_read(Path::new(remote)).await,
        };
        restore_active(&mut guard, prev);
        drop(guard);

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

    // Source cap first, then an equal per-host share of the wire budget. Cap the
    // content string BEFORE embedding it in JSON, never the serialised JSON.
    let max_input = session.max_input_bytes();
    let wire_budget = session.max_output_bytes();
    let share = if wire_budget == 0 {
        0 // 0 disables the wire cap, matching cap_output's contract.
    } else {
        std::cmp::max(1, wire_budget / outcome.len())
    };

    let mut hosts_json = Map::new();
    for (host, res) in &outcome {
        let bytes = res.as_ref().expect("failures already returned above");
        let size = bytes.len();
        let mut source_capped = bytes.as_slice();
        let mut truncated = false;
        if max_input > 0 && source_capped.len() > max_input {
            source_capped = &source_capped[..max_input];
            truncated = true;
        }
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
                // testreport-flavoured text into the file content. `truncated` +
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

/// The in-band `put` body.
async fn transfer_put(
    session: &McpSession,
    filename: &str,
    content: Option<String>,
    content_b64: Option<String>,
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

    let payload: Vec<u8> = match (content, content_b64) {
        (Some(text), None) => text.into_bytes(),
        (None, Some(b64)) => Base64::decode_vec(&b64)
            .map_err(|e| refuse(format!("content_b64 is not valid base64: {e}")))?,
        (None, None) => {
            return Err(refuse("one of `content` or `content_b64` is required"));
        }
        (Some(_), Some(_)) => {
            return Err(refuse(
                "pass exactly one of `content` or `content_b64`, not both",
            ));
        }
    };
    let max_input = session.max_input_bytes();
    if max_input > 0 && payload.len() > max_input {
        return Err(refuse(format!(
            "payload is {} bytes, above the [mcp] max_input_bytes={max_input} budget — \
             truncating an upload would corrupt it, so it is refused",
            payload.len()
        )));
    }

    let _lock = session.exclusive_lock().await;
    let mut guard = session.session().lock().await;
    let rrid = resolve_rrid(&guard, template)?;
    let prev = guard.templates.active_rrid().map(str::to_owned);
    if !guard.activate(&rrid).is_active() {
        restore_active(&mut guard, prev);
        return Err(refuse(format!("could not activate template {rrid}")));
    }

    // The same destination the REPL `put` uses: `{target_tempdir}/{filename}`.
    let remote = guard.metadata().target_wd(&[filename]);
    let map = guard.targets_mut().sftp_put_bytes(&payload, &remote).await;
    restore_active(&mut guard, prev);
    drop(guard);

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
    Ok(json!({
        "remote": remote.display().to_string(),
        "hosts": Value::Object(hosts_json),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(both.stderr.contains("not both"), "{}", both.stderr);
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
