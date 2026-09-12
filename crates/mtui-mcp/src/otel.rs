//! OTLP/HTTP LOGS export of the JSONL audit record (#411 follow-up).
//!
//! Hand-rolled OTLP/HTTP, zero new shipped crates: no `opentelemetry-*`
//! dependency, one `LogRecord` per audit event over the workspace `reqwest`/
//! rustls stack already in the lock via `mtui-datasources`. The protobuf
//! varint encoder below is hand-written; tests assert byte-exact output
//! rather than pulling `prost` even as a dev oracle.
//!
//! Body is the verbatim JSONL line the file sink writes; attributes are a
//! closed `mtui.*` vocabulary (`tool`, `outcome`, `event`, `seq`,
//! `transport`, `session.id`, `response_bytes` when sized) plus the W3C
//! trace correlation when the client supplied one. Resource carries only
//! `service.name` (the multi-deployment join key, default `mtui`).
//!
//! Configuration is env-only (container convention; headers must never be CLI
//! flags, so there are no new TOML keys — the sink matrix falls out of the
//! existing `[mcp] audit_log` plus the endpoint):
//!
//! * file-only: `audit_log` set, no endpoint (current behaviour).
//! * OTLP-only: endpoint set, `audit_log` unset. The JSONL line is still
//!   built in memory and used verbatim as the OTLP body.
//! * both: file first, then OTLP enqueue in `seq` order. A pre-flight checks
//!   both; an OTLP race after a file write keeps the file record and reports
//!   an `audit_gap` on recovery.
//! * neither: auditing off, dispatch byte-identical.
//!
//! Load-bearing semantics: a startup probe posts one real diagnostics record
//! (`~5x500ms`) before serving; its latch feeds the existing refuse path.
//! Lost batches merge into a pending `audit_gap` sent on recovery. Queue full
//! refuses foreground calls, never drops; terminal records (already answered)
//! warn instead. Diagnostics ride a separate best-effort queue (drop +
//! counter, never refuse).
//!
//! Batching: 2048/stream cap, 512/batch, 500ms interval, 10s request timeout,
//! 5s shutdown flush, redirects disabled. TLS reuses the workspace posture
//! (`[mtui] ssl_verify` via `SslVerify`); the endpoint value never reaches a
//! log, record, or error string.
//!
//! Schema discipline: the OTel envelope is versioned alongside the audit
//! schema it carries ([`OTEL_SCOPE_VERSION`], currently `"1"`). No serde
//! deserialisation of untrusted input exists on this path — config is env-only,
//! records are built internally — so there is no `deny_unknown_fields` site;
//! the closed `mtui.*` attribute vocabulary and the kwarg allowlist serve that
//! role instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use mtui_config::SslVerify;
use tokio::sync::mpsc;

// Batching and lifecycle bounds (documented above).
pub(crate) const OTEL_QUEUE_CAP: usize = 2048;
pub(crate) const OTEL_BATCH_MAX: usize = 512;
pub(crate) const OTEL_FLUSH_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const OTEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const OTEL_SHUTDOWN_FLUSH: Duration = Duration::from_secs(5);
pub(crate) const OTEL_PROBE_RETRIES: usize = 5;
pub(crate) const OTEL_PROBE_DELAY: Duration = Duration::from_millis(500);

// OTel envelope versioning: scope version tracks the audit schema it carries.
pub(crate) const OTEL_SCOPE_VERSION: &str = "1";
pub(crate) const OTEL_SCOPE_AUDIT: &str = "mtui-mcp/audit";
pub(crate) const OTEL_SCOPE_DIAG: &str = "mtui-mcp/diagnostics";

// Closed diagnostic vocabulary for exporter failures: never a URL, header,
// or endpoint value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportReason {
    Timeout,
    Connect,
    Status,
    Body,
    Encode,
}

impl ExportReason {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            ExportReason::Timeout => "timeout",
            ExportReason::Connect => "connect",
            ExportReason::Status => "status",
            ExportReason::Body => "body",
            ExportReason::Encode => "encode",
        }
    }
}

// Secret holder with no Debug impl by design: header values and the endpoint
// must never reach a log via `{:?}`. Containers implement a redacted Debug
// manually.
pub(crate) struct SecretBox(String);

impl SecretBox {
    fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

// Env-only OTLP configuration. `endpoint` is the full `/v1/logs` URL.
pub(crate) struct OtelConfig {
    endpoint: SecretBox,
    headers: Vec<(String, SecretBox)>,
    service_name: String,
}

impl std::fmt::Debug for OtelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelConfig")
            .field("endpoint", &"<redacted>")
            .field("headers", &"<redacted>")
            .field("service_name", &self.service_name)
            .finish()
    }
}

impl OtelConfig {
    fn non_empty(var: &str) -> Option<String> {
        std::env::var(var)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    }

    // Full `/v1/logs` URL: the logs-specific endpoint wins as-is, else the
    // generic base gains `/v1/logs`. Unset-or-empty disables.
    fn endpoint_url() -> Option<String> {
        if let Some(logs) = Self::non_empty("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT") {
            return Some(logs);
        }
        Self::non_empty("OTEL_EXPORTER_OTLP_ENDPOINT").map(|base| {
            let base = base.trim_end_matches('/');
            format!("{base}/v1/logs")
        })
    }

    fn endpoint_valid(url: &str) -> bool {
        let url = url.trim();
        let rest = if let Some(rest) = url.strip_prefix("https://") {
            rest
        } else if let Some(rest) = url.strip_prefix("http://") {
            rest
        } else {
            return false;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
        if host_port.is_empty() {
            return false;
        }
        if let Some(after) = host_port.strip_prefix('[') {
            let Some((host, tail)) = after.split_once(']') else {
                return false;
            };
            if host.is_empty() {
                return false;
            }
            return match tail.strip_prefix(':') {
                None => tail.is_empty(),
                Some(port) => {
                    !port.is_empty()
                        && port.bytes().all(|b| b.is_ascii_digit())
                        && port.parse::<u16>().is_ok()
                }
            };
        }
        match host_port.rsplit_once(':') {
            Some((host, port)) => {
                // A trailing colon with no port is invalid; a colon may also
                // be part of a path-less authority only as host:port.
                if port.is_empty() {
                    return false;
                }
                if port.bytes().all(|b| b.is_ascii_digit()) {
                    return !host.is_empty() && port.parse::<u16>().is_ok();
                }
                // Non-numeric colon (e.g. IPv6 without brackets) is rejected;
                // the authority itself must still be non-empty.
                !host_port.is_empty()
            }
            None => true,
        }
    }

    // Comma-separated `key=value` pairs; values are percent-decoded. Malformed
    // entries are skipped, never logged with values.
    fn headers_from_env() -> Vec<(String, SecretBox)> {
        let raw = Self::non_empty("OTEL_EXPORTER_OTLP_LOGS_HEADERS")
            .or_else(|| Self::non_empty("OTEL_EXPORTER_OTLP_HEADERS"))
            .unwrap_or_default();
        let mut out = Vec::new();
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            let key = key.trim().to_owned();
            if key.is_empty() {
                continue;
            }
            out.push((key, SecretBox::new(percent_decode(value.trim()))));
        }
        out
    }

    // `http/protobuf` only, validated only when an endpoint exists. Empty
    // means the default; anything else with an endpoint disables.
    fn protocol_valid(endpoint_exists: bool) -> bool {
        if !endpoint_exists {
            return true;
        }
        let raw = Self::non_empty("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL")
            .or_else(|| Self::non_empty("OTEL_EXPORTER_OTLP_PROTOCOL"))
            .unwrap_or_default();
        raw.is_empty() || raw == "http/protobuf"
    }

    pub(crate) fn from_env() -> Option<Self> {
        let endpoint = Self::endpoint_url()?;
        if !Self::endpoint_valid(&endpoint) {
            tracing::warn!("otlp disabled: invalid endpoint");
            return None;
        }
        if !Self::protocol_valid(true) {
            tracing::warn!("otlp disabled: unsupported protocol (want http/protobuf)");
            return None;
        }
        let service_name =
            Self::non_empty("OTEL_SERVICE_NAME").unwrap_or_else(|| "mtui".to_owned());
        Some(Self {
            endpoint: SecretBox::new(endpoint),
            headers: Self::headers_from_env(),
            service_name,
        })
    }

    // Test seam: build from explicit values without touching the environment.
    #[cfg(test)]
    pub(crate) fn for_tests(endpoint: &str, service_name: &str) -> Self {
        Self {
            endpoint: SecretBox::new(endpoint.to_owned()),
            headers: Vec::new(),
            service_name: service_name.to_owned(),
        }
    }
}

fn percent_decode(raw: &str) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Strict lowercase 55-byte W3C traceparent: `00-<32hex>-<16hex>-<2hex>`,
// neither id all-zeroes. Returns trace/span bytes plus flags.
pub(crate) fn parse_traceparent(value: &str) -> Option<([u8; 16], [u8; 8], u8)> {
    if value.len() != 55 {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return None;
    }
    if &value[0..2] != "00" {
        return None;
    }
    let trace_hex = &value[3..35];
    let span_hex = &value[36..52];
    let flags_hex = &value[53..55];
    if !trace_hex
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    if !span_hex
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    if !flags_hex
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    let trace = hex_to_bytes::<16>(trace_hex)?;
    let span = hex_to_bytes::<8>(span_hex)?;
    if trace.iter().all(|&b| b == 0) || span.iter().all(|&b| b == 0) {
        return None;
    }
    let flags = u8::from_str_radix(flags_hex, 16).ok()?;
    Some((trace, span, flags))
}

fn hex_to_bytes<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let (chunks, _) = hex.as_bytes().as_chunks::<2>();
    for (i, chunk) in chunks.iter().enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

// Hand-written protobuf varint encoder (no prost, even in dev-deps).
fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn tag(field: u32, wire: u8, out: &mut Vec<u8>) {
    varint(u64::from(field << 3 | u32::from(wire)), out);
}

fn varint_field(field: u32, value: u64, out: &mut Vec<u8>) {
    tag(field, 0, out);
    varint(value, out);
}

fn fixed64_field(field: u32, value: u64, out: &mut Vec<u8>) {
    tag(field, 1, out);
    out.extend_from_slice(&value.to_le_bytes());
}

fn delimited_field(field: u32, bytes: &[u8], out: &mut Vec<u8>) {
    tag(field, 2, out);
    varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn string_field(field: u32, value: &str, out: &mut Vec<u8>) {
    delimited_field(field, value.as_bytes(), out);
}

fn bytes_field(field: u32, value: &[u8], out: &mut Vec<u8>) {
    delimited_field(field, value, out);
}

fn anyvalue_string(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    string_field(1, value, &mut out);
    out
}

fn anyvalue_bool(value: bool) -> Vec<u8> {
    let mut out = Vec::new();
    varint_field(2, u64::from(value), &mut out);
    out
}

#[allow(clippy::cast_sign_loss)]
fn anyvalue_int(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    // int64 varint: negatives sign-extend to ten bytes, matching prost.
    varint_field(3, value as u64, &mut out);
    out
}

fn keyvalue(key: &str, value_msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    string_field(1, key, &mut out);
    delimited_field(2, value_msg, &mut out);
    out
}

fn str_attr(key: &str, value: &str) -> Vec<u8> {
    keyvalue(key, &anyvalue_string(value))
}

fn int_attr(key: &str, value: i64) -> Vec<u8> {
    keyvalue(key, &anyvalue_int(value))
}

fn bool_attr(key: &str, value: bool) -> Vec<u8> {
    keyvalue(key, &anyvalue_bool(value))
}

// One OTLP LogRecord. Severity follows the audit outcome: ok/info,
// error/error, unknown-tool/warn; diagnostics/info; gaps/warn.
struct LogRecord {
    time_nanos: u64,
    severity_number: u32,
    severity_text: &'static str,
    body: String,
    attributes: Vec<Vec<u8>>,
    trace: Option<([u8; 16], [u8; 8], u8)>,
}

impl LogRecord {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        fixed64_field(1, self.time_nanos, &mut out);
        varint_field(2, u64::from(self.severity_number), &mut out);
        string_field(3, self.severity_text, &mut out);
        delimited_field(5, &anyvalue_string(&self.body), &mut out);
        for attr in &self.attributes {
            delimited_field(6, attr, &mut out);
        }
        if let Some((trace_id, span_id, flags)) = &self.trace {
            varint_field(8, u64::from(*flags), &mut out);
            bytes_field(9, trace_id, &mut out);
            bytes_field(10, span_id, &mut out);
        }
        fixed64_field(11, self.time_nanos, &mut out);
        out
    }
}

fn scope_logs(scope: &str, records: &[Vec<u8>]) -> Vec<u8> {
    let mut scope_msg = Vec::new();
    string_field(1, scope, &mut scope_msg);
    string_field(2, OTEL_SCOPE_VERSION, &mut scope_msg);
    let mut out = Vec::new();
    delimited_field(1, &scope_msg, &mut out);
    for record in records {
        delimited_field(2, record, &mut out);
    }
    out
}

fn resource_logs(service_name: &str, scopes: &[Vec<u8>]) -> Vec<u8> {
    let service_attr = str_attr("service.name", service_name);
    let mut resource_msg = Vec::new();
    delimited_field(1, &service_attr, &mut resource_msg);
    let mut out = Vec::new();
    delimited_field(1, &resource_msg, &mut out);
    for scope in scopes {
        delimited_field(2, scope, &mut out);
    }
    out
}

pub(crate) fn export_request(service_name: &str, scopes: &[Vec<u8>]) -> Vec<u8> {
    let resource = resource_logs(service_name, scopes);
    let mut out = Vec::new();
    delimited_field(1, &resource, &mut out);
    out
}

// Queued audit record: the verbatim JSONL line plus its indexed attrs.
pub(crate) struct QueuedAudit {
    pub(crate) seq: u64,
    pub(crate) jsonl: String,
    pub(crate) tool: String,
    pub(crate) outcome: String,
    pub(crate) event: String,
    pub(crate) transport: String,
    pub(crate) session_id: u64,
    pub(crate) response_bytes: Option<usize>,
    pub(crate) trace: Option<([u8; 16], [u8; 8], u8)>,
    pub(crate) time_nanos: u64,
    pub(crate) extra_attrs: Vec<Vec<u8>>,
}

// Best-effort diagnostics event, already capped and escaped at capture.
pub(crate) struct QueuedDiag {
    pub(crate) message: String,
    pub(crate) level: String,
    pub(crate) target: String,
    pub(crate) time_nanos: u64,
}

fn audit_record(q: &QueuedAudit) -> Vec<u8> {
    let mut attrs = vec![
        str_attr("mtui.tool", &q.tool),
        str_attr("mtui.outcome", &q.outcome),
        str_attr("mtui.event", &q.event),
        str_attr("mtui.transport", &q.transport),
        int_attr("mtui.seq", q.seq as i64),
        int_attr("mtui.session.id", q.session_id as i64),
    ];
    attrs.extend(q.extra_attrs.iter().cloned());
    if let Some(bytes) = q.response_bytes {
        attrs.push(int_attr("mtui.response_bytes", bytes as i64));
    }
    let (severity_number, severity_text) = match q.outcome.as_str() {
        "ok" => (9u32, "INFO"),
        "unknown-tool" => (13u32, "WARN"),
        _ => (17u32, "ERROR"),
    };
    LogRecord {
        time_nanos: q.time_nanos,
        severity_number,
        severity_text,
        body: q.jsonl.clone(),
        attributes: attrs,
        trace: q.trace,
    }
    .encode()
}

fn diag_record(q: &QueuedDiag) -> Vec<u8> {
    let attrs = vec![
        str_attr("mtui.stream", "diagnostics"),
        str_attr("mtui.level", &q.level),
        str_attr("mtui.target", &q.target),
    ];
    LogRecord {
        time_nanos: q.time_nanos,
        severity_number: 9,
        severity_text: "INFO",
        body: q.message.clone(),
        attributes: attrs,
        trace: None,
    }
    .encode()
}

fn gap_record(seq_start: u64, seq_end: u64, count: usize, service_name: &str) -> Vec<u8> {
    let body = serde_json::json!({
        "event": "audit_gap",
        "lost_start": seq_start,
        "lost_end": seq_end,
        "count": count,
        "service": service_name,
    })
    .to_string();
    let attrs = vec![
        str_attr("mtui.event", "audit_gap"),
        int_attr("mtui.seq_start", seq_start as i64),
        int_attr("mtui.seq_end", seq_end as i64),
        int_attr("mtui.lost", count as i64),
    ];
    LogRecord {
        time_nanos: now_nanos(),
        severity_number: 13,
        severity_text: "WARN",
        body,
        attributes: attrs,
        trace: None,
    }
    .encode()
}

pub(crate) fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

// Closed-vocab classification of a reqwest failure: never the URL.
fn classify_reqwest(err: &reqwest::Error) -> ExportReason {
    if err.is_timeout() {
        ExportReason::Timeout
    } else if err.is_connect() {
        ExportReason::Connect
    } else if err.is_status() {
        ExportReason::Status
    } else if err.is_body() || err.is_decode() {
        ExportReason::Body
    } else {
        ExportReason::Connect
    }
}

// The background exporter. Clone shares the queues and the health latch.
pub(crate) struct OtelExporter {
    config_service: String,
    endpoint: SecretBox,
    headers: Vec<(String, SecretBox)>,
    client: reqwest::Client,
    audit_tx: mpsc::Sender<QueuedAudit>,
    diag_tx: mpsc::Sender<QueuedDiag>,
    healthy: Arc<AtomicBool>,
    diag_dropped: Arc<AtomicU64>,
    audit_lost: Arc<AtomicU64>,
    shutdown: tokio_util::sync::CancellationToken,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for OtelExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelExporter")
            .field("endpoint", &"<redacted>")
            .field("service", &self.config_service)
            .field("healthy", &self.is_healthy())
            .field("dropped", &self.diag_dropped())
            .field("lost", &self.audit_lost())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnqueueError {
    Unhealthy,
    Full,
}

impl OtelExporter {
    // The one blocking syscall on the OTLP path (CA bundle read) runs on the
    // blocking pool; client construction itself is CPU-only. The POSTs are
    // async `reqwest` and the probe/shutdown run outside dispatch (startup /
    // teardown in `runner`), so no collector stall ever parks a worker.
    async fn build_client(verify: &SslVerify) -> Result<reqwest::Client, &'static str> {
        let pem = match verify {
            SslVerify::CaBundle(path) => {
                let path = path.clone();
                let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path))
                    .await
                    .map_err(|_| "ca_bundle_unreadable")?
                    .map_err(|_| "ca_bundle_unreadable")?;
                Some(bytes)
            }
            _ => None,
        };
        let mut builder = reqwest::Client::builder()
            .timeout(OTEL_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        builder = match (verify, pem) {
            (SslVerify::Enabled, _) => builder,
            (SslVerify::Disabled, _) => {
                tracing::warn!(
                    "otlp TLS verification disabled by ssl_verify; connections are not authenticated"
                );
                builder.danger_accept_invalid_certs(true)
            }
            (SslVerify::CaBundle(_), Some(pem)) => {
                let certs =
                    reqwest::Certificate::from_pem_bundle(&pem).map_err(|_| "ca_bundle_invalid")?;
                builder.tls_certs_only(certs)
            }
            (SslVerify::CaBundle(_), None) => return Err("ca_bundle_unreadable"),
        };
        builder.build().map_err(|_| "client_build")
    }

    // Spawned exporter; the background task owns the receivers. Async so the
    // blocking CA bundle read rides `spawn_blocking`; `probe` still runs after
    // in `runner`.
    pub(crate) async fn new(config: OtelConfig, verify: &SslVerify) -> Option<Arc<Self>> {
        let client = match Self::build_client(verify).await {
            Ok(client) => client,
            Err(reason) => {
                tracing::warn!(reason, "otlp disabled: http client failed");
                return None;
            }
        };
        Self::spawn(config, client)
    }

    // Test seam: explicit client (wiremock http, no TLS posture).
    #[cfg(test)]
    pub(crate) fn with_client(config: OtelConfig, client: reqwest::Client) -> Arc<Self> {
        Self::spawn(config, client).expect("test exporter builds")
    }

    fn spawn(config: OtelConfig, client: reqwest::Client) -> Option<Arc<Self>> {
        let (audit_tx, audit_rx) = mpsc::channel(OTEL_QUEUE_CAP);
        let (diag_tx, diag_rx) = mpsc::channel(OTEL_QUEUE_CAP);
        let exporter = Arc::new(Self {
            config_service: config.service_name.clone(),
            endpoint: config.endpoint,
            headers: config.headers,
            client: client.clone(),
            audit_tx,
            diag_tx,
            healthy: Arc::new(AtomicBool::new(true)),
            diag_dropped: Arc::new(AtomicU64::new(0)),
            audit_lost: Arc::new(AtomicU64::new(0)),
            shutdown: tokio_util::sync::CancellationToken::new(),
            task: std::sync::Mutex::new(None),
        });
        let task = tokio::spawn(run_loop(
            audit_rx,
            diag_rx,
            client,
            // Move the endpoint/headers into the task without logging them.
            TaskConfig {
                endpoint: exporter.endpoint.expose().to_owned(),
                headers: exporter
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.expose().to_owned()))
                    .collect(),
                service: exporter.config_service.clone(),
            },
            Arc::clone(&exporter.healthy),
            Arc::clone(&exporter.audit_lost),
            exporter.shutdown.clone(),
        ));
        *exporter.task.lock().expect("exporter task slot") = Some(task);
        Some(exporter)
    }

    // Global singleton for the serving process. Initialised once before
    // serving; sessions clone it. Tests use explicit handles instead.
    pub(crate) fn global() -> Option<Arc<Self>> {
        global_exporter().get().cloned()
    }

    pub(crate) fn install_global(exporter: Arc<Self>) {
        let _ = global_exporter().set(exporter);
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub(crate) fn diag_dropped(&self) -> u64 {
        self.diag_dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn audit_lost(&self) -> u64 {
        self.audit_lost.load(Ordering::Relaxed)
    }

    // Foreground path: refuse on unhealthy or full, never drop, never block.
    pub(crate) fn enqueue_audit(&self, record: QueuedAudit) -> Result<(), EnqueueError> {
        if !self.is_healthy() {
            return Err(EnqueueError::Unhealthy);
        }
        self.audit_tx
            .try_send(record)
            .map_err(|_| EnqueueError::Full)
    }

    // Diagnostics path: best-effort, drop plus counter, never refuse.
    pub(crate) fn enqueue_diag(&self, record: QueuedDiag) {
        if self.diag_tx.try_send(record).is_err() {
            self.diag_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Startup probe: one real diagnostics record, bounded retries.
    pub(crate) async fn probe(&self) -> bool {
        let body = diag_record(&QueuedDiag {
            message: "mtui-mcp otel probe".to_owned(),
            level: "info".to_owned(),
            target: "mtui_mcp::otel::probe".to_owned(),
            time_nanos: now_nanos(),
        });
        let scope = scope_logs(OTEL_SCOPE_DIAG, &[body]);
        let payload = export_request(&self.config_service, &[scope]);
        for _ in 0..OTEL_PROBE_RETRIES {
            if post_logs(
                &self.client,
                self.endpoint.expose(),
                &self.headers_exposed(),
                payload.clone(),
            )
            .await
            .is_ok()
            {
                return true;
            }
            tokio::time::sleep(OTEL_PROBE_DELAY).await;
        }
        self.healthy.store(false, Ordering::Relaxed);
        tracing::warn!("otlp probe failed: export unhealthy");
        false
    }

    fn headers_exposed(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect()
    }

    // 5s shutdown flush: signal the loop, then bound the join.
    pub(crate) async fn shutdown(&self) {
        self.shutdown.cancel();
        let handle = self.task.lock().expect("exporter task slot").take();
        if let Some(handle) = handle {
            let _ =
                tokio::time::timeout(OTEL_SHUTDOWN_FLUSH + Duration::from_secs(1), handle).await;
        }
        tracing::info!(
            dropped = self.diag_dropped.load(Ordering::Relaxed),
            lost = self.audit_lost.load(Ordering::Relaxed),
            "otlp shutdown flush"
        );
    }
}

struct TaskConfig {
    endpoint: String,
    headers: Vec<(String, String)>,
    service: String,
}

fn global_exporter() -> &'static std::sync::OnceLock<Arc<OtelExporter>> {
    static GLOBAL: std::sync::OnceLock<Arc<OtelExporter>> = std::sync::OnceLock::new();
    &GLOBAL
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    mut audit_rx: mpsc::Receiver<QueuedAudit>,
    mut diag_rx: mpsc::Receiver<QueuedDiag>,
    client: reqwest::Client,
    config: TaskConfig,
    healthy: Arc<AtomicBool>,
    audit_lost: Arc<AtomicU64>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut pending_gap: Option<(u64, u64, usize)> = None;
    let mut tick = tokio::time::interval(OTEL_FLUSH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                flush_remaining(
                    &mut audit_rx,
                    &mut diag_rx,
                    &client,
                    &config,
                    &healthy,
                    &audit_lost,
                    &mut pending_gap,
                )
                .await;
                return;
            }
            _ = tick.tick() => {
                flush_one_batch(
                    &mut audit_rx,
                    &mut diag_rx,
                    &client,
                    &config,
                    &healthy,
                    &audit_lost,
                    &mut pending_gap,
                )
                .await;
            }
        }
    }
}

async fn flush_remaining(
    audit_rx: &mut mpsc::Receiver<QueuedAudit>,
    diag_rx: &mut mpsc::Receiver<QueuedDiag>,
    client: &reqwest::Client,
    config: &TaskConfig,
    healthy: &AtomicBool,
    audit_lost: &AtomicU64,
    pending_gap: &mut Option<(u64, u64, usize)>,
) {
    // Bounded by the shutdown budget: drain without waiting, then one final
    // export attempt inside the timeout.
    let drained = drain_batch(audit_rx, diag_rx, usize::MAX);
    if drained.0.is_empty() && drained.1.is_empty() && pending_gap.is_none() {
        return;
    }
    let _ = tokio::time::timeout(
        OTEL_SHUTDOWN_FLUSH,
        export_batch(
            client,
            config,
            healthy,
            audit_lost,
            pending_gap,
            drained.0,
            drained.1,
        ),
    )
    .await;
}

// Drain up to OTEL_BATCH_MAX records total, audit first (load-bearing) then
// diagnostics to fill the remainder.
fn drain_batch(
    audit_rx: &mut mpsc::Receiver<QueuedAudit>,
    diag_rx: &mut mpsc::Receiver<QueuedDiag>,
    cap: usize,
) -> (Vec<QueuedAudit>, Vec<QueuedDiag>) {
    let limit = cap.min(OTEL_BATCH_MAX);
    let mut audits = Vec::new();
    while audits.len() < limit {
        match audit_rx.try_recv() {
            Ok(record) => audits.push(record),
            Err(_) => break,
        }
    }
    let mut diags = Vec::new();
    while audits.len() + diags.len() < limit {
        match diag_rx.try_recv() {
            Ok(record) => diags.push(record),
            Err(_) => break,
        }
    }
    (audits, diags)
}

async fn flush_one_batch(
    audit_rx: &mut mpsc::Receiver<QueuedAudit>,
    diag_rx: &mut mpsc::Receiver<QueuedDiag>,
    client: &reqwest::Client,
    config: &TaskConfig,
    healthy: &AtomicBool,
    audit_lost: &AtomicU64,
    pending_gap: &mut Option<(u64, u64, usize)>,
) {
    let (audits, diags) = drain_batch(audit_rx, diag_rx, OTEL_BATCH_MAX);
    if audits.is_empty() && diags.is_empty() && pending_gap.is_none() {
        return;
    }
    export_batch(
        client,
        config,
        healthy,
        audit_lost,
        pending_gap,
        audits,
        diags,
    )
    .await;
}

async fn export_batch(
    client: &reqwest::Client,
    config: &TaskConfig,
    healthy: &AtomicBool,
    audit_lost: &AtomicU64,
    pending_gap: &mut Option<(u64, u64, usize)>,
    audits: Vec<QueuedAudit>,
    diags: Vec<QueuedDiag>,
) {
    let mut audit_records: Vec<Vec<u8>> = audits.iter().map(audit_record).collect();
    let diag_records: Vec<Vec<u8>> = diags.iter().map(diag_record).collect();
    // A recovered gap rides first so the collector sees the hole it fills.
    let mut scopes = Vec::new();
    let prior_gap = pending_gap.take();
    if let Some((start, end, count)) = prior_gap {
        audit_records.insert(0, gap_record(start, end, count, &config.service));
    }
    if !audit_records.is_empty() {
        scopes.push(scope_logs(OTEL_SCOPE_AUDIT, &audit_records));
    }
    if !diag_records.is_empty() {
        scopes.push(scope_logs(OTEL_SCOPE_DIAG, &diag_records));
    }
    if scopes.is_empty() {
        return;
    }
    let payload = export_request(&config.service, &scopes);
    match post_logs(client, &config.endpoint, &config.headers, payload).await {
        Ok(()) => {}
        Err(reason) => {
            // Sticky fail-closed latch for audit; diagnostics stay best-effort.
            healthy.store(false, Ordering::Relaxed);
            if audits.is_empty() {
                // Gap-only or diagnostics-only batch failed: restore the taken
                // gap (None stays None).
                *pending_gap = prior_gap;
            } else {
                let batch_start = audits.first().map_or(u64::MAX, |r| r.seq);
                let batch_end = audits.last().map_or(0, |r| r.seq);
                let merged = match prior_gap {
                    Some((prior_start, prior_end, prior_count)) => (
                        prior_start.min(batch_start),
                        prior_end.max(batch_end),
                        prior_count + audits.len(),
                    ),
                    None => (batch_start, batch_end, audits.len()),
                };
                *pending_gap = Some(merged);
                audit_lost.fetch_add(audits.len() as u64, Ordering::Relaxed);
            }
            // Closed vocabulary only: never the endpoint, headers, or body.
            tracing::warn!(reason = reason.as_str(), "otlp export failed");
        }
    }
}

// POST one OTLP/HTTP LOGS request. Redirects are disabled on the client;
// callers classify failures without retaining the URL.
async fn post_logs(
    client: &reqwest::Client,
    endpoint: &str,
    headers: &[(String, String)],
    payload: Vec<u8>,
) -> Result<(), ExportReason> {
    let mut request = client
        .post(endpoint)
        .header("content-type", "application/x-protobuf")
        .body(payload);
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|err| classify_reqwest(&err))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(ExportReason::Status)
    }
}

// Diagnostics capture: per-field caps plus control-char escaping. Exporter
// and HTTP-stack targets are never exported (feedback loop).
pub(crate) const DIAG_FIELD_CAP: usize = 1024;

const DIAG_EXCLUDED_PREFIXES: &[&str] = &[
    "mtui_mcp::otel",
    "reqwest",
    "hyper",
    "hyper_util",
    "rustls",
    "h2",
    "tower",
    "axum",
];

pub(crate) fn diag_target_excluded(target: &str) -> bool {
    DIAG_EXCLUDED_PREFIXES
        .iter()
        .any(|prefix| target == *prefix || target.starts_with(&format!("{prefix}::")))
}

pub(crate) fn escape_diag_field(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '"' => escaped.push_str("\\\""),
            ch if (ch as u32) < 0x20 || (ch as u32) == 0x7f => {
                escaped.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => escaped.push(ch),
        }
        if escaped.len() >= DIAG_FIELD_CAP * 4 {
            break;
        }
    }
    // Cap on chars of the escaped form so the wire field stays bounded.
    if escaped.chars().count() > DIAG_FIELD_CAP {
        escaped.chars().take(DIAG_FIELD_CAP).collect()
    } else {
        escaped
    }
}

// `tracing` layer forwarding events to the global exporter's diagnostics
// queue. Installed unconditionally; no-ops when OTLP is off. Never logs
// itself (that would recurse through this same layer).
pub(crate) struct OtelDiagLayer;

impl<S> tracing_subscriber::Layer<S> for OtelDiagLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(exporter) = OtelExporter::global() else {
            return;
        };
        let target = event.metadata().target();
        if diag_target_excluded(target) {
            return;
        }
        struct Visitor {
            message: String,
        }
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.message = value.to_owned();
                }
            }
        }
        let mut visitor = Visitor {
            message: String::new(),
        };
        event.record(&mut visitor);
        let level = event.metadata().level().to_string().to_lowercase();
        exporter.enqueue_diag(QueuedDiag {
            message: escape_diag_field(&visitor.message),
            level: escape_diag_field(&level),
            target: escape_diag_field(target),
            time_nanos: now_nanos(),
        });
    }
}

// Kwarg allowlist for indexed OTLP derivation: full kwargs stay in the body
// (the verbatim JSONL line); only these closed keys become queryable
// attributes elsewhere. `config_set` keeps its full redact and contributes
// no kwarg attrs.
pub(crate) const OTEL_KWARG_ALLOWLIST: &[&str] =
    &["template", "all_templates", "background", "job_id"];

// Allowlisted kwarg attrs for one call, with in-record-only caps. Strings
// cap at 1024 chars; bools pass through; other shapes are ignored (the body
// keeps them verbatim). `config_set` contributes nothing.
pub(crate) fn kwarg_otlp_attrs(
    tool: &str,
    kwargs: &serde_json::Map<String, serde_json::Value>,
) -> Vec<Vec<u8>> {
    if tool == "config_set" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for key in OTEL_KWARG_ALLOWLIST {
        let Some(value) = kwargs.get(*key) else {
            continue;
        };
        match (*key, value) {
            ("template", serde_json::Value::String(s))
            | ("job_id", serde_json::Value::String(s)) => {
                let capped: String = s.chars().take(1024).collect();
                out.push(str_attr(&format!("mtui.{key}"), &capped));
            }
            ("background", serde_json::Value::Bool(b))
            | ("all_templates", serde_json::Value::Bool(b)) => {
                out.push(bool_attr(&format!("mtui.{key}"), *b));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_GUARD.lock().expect("env guard")
    }

    // `set_var`/`remove_var` are `unsafe` in edition 2024; the `lock_env`
    // mutex plus `#[serial(env)]` on every caller makes the mutation exclusive.
    #[allow(unsafe_code)]
    fn set_env(key: &str, value: &str) {
        // SAFETY: serialised via `lock_env` + `#[serial(env)]`.
        unsafe {
            std::env::set_var(key, value);
        }
    }

    #[allow(unsafe_code)]
    fn remove_env(key: &str) {
        // SAFETY: serialised via `lock_env` + `#[serial(env)]`.
        unsafe {
            std::env::remove_var(key);
        }
    }

    fn clear_otel_env() {
        for key in [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
            "OTEL_EXPORTER_OTLP_PROTOCOL",
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
            "OTEL_SERVICE_NAME",
        ] {
            remove_env(key);
        }
    }

    #[test]
    #[serial(env)]
    fn unset_or_empty_endpoint_disables() {
        let _guard = lock_env();
        clear_otel_env();
        assert!(OtelConfig::from_env().is_none());
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "");
        assert!(OtelConfig::from_env().is_none());
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "   ");
        assert!(OtelConfig::from_env().is_none());
    }

    #[test]
    #[serial(env)]
    fn generic_endpoint_gains_v1_logs_suffix() {
        let _guard = lock_env();
        clear_otel_env();
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        let config = OtelConfig::from_env().expect("enabled");
        assert_eq!(config.endpoint.expose(), "http://collector:4318/v1/logs");
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/");
        let config = OtelConfig::from_env().expect("trailing slash trimmed");
        assert_eq!(config.endpoint.expose(), "http://collector:4318/v1/logs");
    }

    #[test]
    #[serial(env)]
    fn logs_endpoint_overrides_generic_verbatim() {
        let _guard = lock_env();
        clear_otel_env();
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://generic:4318");
        set_env(
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            "https://logs-only:4318/custom/logs",
        );
        let config = OtelConfig::from_env().expect("enabled");
        assert_eq!(
            config.endpoint.expose(),
            "https://logs-only:4318/custom/logs"
        );
    }

    #[test]
    #[serial(env)]
    fn invalid_endpoint_disables_without_leaking_value() {
        let _guard = lock_env();
        clear_otel_env();
        set_env(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "not-a-url-with-secret-abc123",
        );
        assert!(OtelConfig::from_env().is_none());
        // The redacted Debug must never contain the raw value.
        let config = OtelConfig::for_tests("http://collector:4318/v1/logs", "mtui");
        let debug = format!("{config:?}");
        assert!(!debug.contains("collector"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    #[serial(env)]
    fn headers_parse_comma_pairs_with_percent_decoding() {
        let _guard = lock_env();
        clear_otel_env();
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        set_env(
            "OTEL_EXPORTER_OTLP_HEADERS",
            "authorization=Bearer%20abc%3D, x-tenant = t1 ,, bad-entry, =novalue",
        );
        let config = OtelConfig::from_env().expect("enabled");
        let exposed: Vec<(String, String)> = config
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect();
        assert_eq!(
            exposed,
            vec![
                ("authorization".to_owned(), "Bearer abc=".to_owned()),
                ("x-tenant".to_owned(), "t1".to_owned()),
            ]
        );
    }

    #[test]
    #[serial(env)]
    fn logs_headers_override_generic() {
        let _guard = lock_env();
        clear_otel_env();
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        set_env("OTEL_EXPORTER_OTLP_HEADERS", "a=1");
        set_env("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "b=2");
        let config = OtelConfig::from_env().expect("enabled");
        assert_eq!(config.headers.len(), 1);
        assert_eq!(config.headers[0].0, "b");
    }

    #[test]
    #[serial(env)]
    fn protocol_validated_only_with_endpoint() {
        let _guard = lock_env();
        clear_otel_env();
        // No endpoint: an exotic protocol is ignored, still disabled for the
        // boring reason (no endpoint), not a protocol error.
        set_env("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc");
        assert!(OtelConfig::from_env().is_none());
        // With an endpoint the same value disables with a protocol warning.
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        assert!(OtelConfig::from_env().is_none());
        set_env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf");
        assert!(OtelConfig::from_env().is_some());
        remove_env("OTEL_EXPORTER_OTLP_PROTOCOL");
        assert!(OtelConfig::from_env().is_some());
    }

    #[test]
    #[serial(env)]
    fn service_name_defaults_to_mtui() {
        let _guard = lock_env();
        clear_otel_env();
        set_env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318");
        assert_eq!(OtelConfig::from_env().expect("on").service_name, "mtui");
        set_env("OTEL_SERVICE_NAME", "qam-mtui");
        assert_eq!(OtelConfig::from_env().expect("on").service_name, "qam-mtui");
    }

    #[test]
    fn traceparent_strict_lowercase_55_bytes() {
        let valid = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";
        let (trace, span, flags) = parse_traceparent(valid).expect("valid");
        assert_eq!(hex_of(&trace), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(hex_of(&span), "00f067aa0ba902b7");
        assert_eq!(flags, 1);
        // Uppercase rejected (strict lowercase).
        assert!(parse_traceparent(&valid.to_uppercase()).is_none());
        // Wrong length, bad version, all-zero ids rejected.
        assert!(parse_traceparent("00-short").is_none());
        assert!(
            parse_traceparent("01-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01").is_none()
        );
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
        assert!(
            parse_traceparent("00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01").is_none()
        );
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn varint_encodes_boundaries() {
        let mut out = Vec::new();
        varint(0, &mut out);
        assert_eq!(out, vec![0x00]);
        out.clear();
        varint(127, &mut out);
        assert_eq!(out, vec![0x7f]);
        out.clear();
        varint(128, &mut out);
        assert_eq!(out, vec![0x80, 0x01]);
        out.clear();
        varint(300, &mut out);
        assert_eq!(out, vec![0xac, 0x02]);
    }

    #[test]
    fn anyvalue_string_keyvalue_bytes_are_exact() {
        // `string_value="a"` is field 1, tag 0x0a, len 1.
        assert_eq!(anyvalue_string("a"), vec![0x0a, 0x01, b'a']);
        // KeyValue{key:"k", value:AnyValue{string_value:"v"}}:
        // key field 1 (0x0a len 1 'k'), value field 2 (0x12 len 3).
        let kv = keyvalue("k", &anyvalue_string("v"));
        assert_eq!(kv, vec![0x0a, 0x01, b'k', 0x12, 0x03, 0x0a, 0x01, b'v',]);
    }

    #[test]
    fn export_request_carries_verbatim_body_and_mtui_attrs() {
        let audit = QueuedAudit {
            seq: 7,
            jsonl: r#"{"v":1,"tool":"whoami"}"#.to_owned(),
            tool: "whoami".to_owned(),
            outcome: "ok".to_owned(),
            event: "call".to_owned(),
            transport: "stdio".to_owned(),
            session_id: 3,
            response_bytes: Some(12),
            trace: None,
            time_nanos: 1_700_000_000_000_000_000,
            extra_attrs: Vec::new(),
        };
        let record = audit_record(&audit);
        // Body string_value carries the verbatim JSONL line.
        let body_marker = r#"{"v":1,"tool":"whoami"}"#.as_bytes();
        assert!(
            record.windows(body_marker.len()).any(|w| w == body_marker),
            "body must embed the verbatim JSONL line"
        );
        let scopes = [scope_logs(OTEL_SCOPE_AUDIT, &[record])];
        let payload = export_request("mtui", &scopes);
        // Resource service.name present; envelope scope version present.
        assert!(
            payload
                .windows(b"service.name".len())
                .any(|w| w == b"service.name"),
            "resource must carry service.name"
        );
        assert!(
            payload.windows(b"mtui".len()).any(|w| w == b"mtui"),
            "service value mtui present"
        );
        for attr in [
            "mtui.tool",
            "mtui.outcome",
            "mtui.seq",
            "mtui.transport",
            "mtui.session.id",
            "mtui.response_bytes",
        ] {
            assert!(
                payload.windows(attr.len()).any(|w| w == attr.as_bytes()),
                "attr {attr} present"
            );
        }
    }

    #[test]
    fn export_request_emits_trace_ids_when_present() {
        let (trace, span, flags) =
            parse_traceparent("00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01")
                .expect("valid");
        let audit = QueuedAudit {
            seq: 1,
            jsonl: "{}".to_owned(),
            tool: "run".to_owned(),
            outcome: "ok".to_owned(),
            event: "call".to_owned(),
            transport: "http".to_owned(),
            session_id: 1,
            response_bytes: None,
            trace: Some((trace, span, flags)),
            time_nanos: 1,
            extra_attrs: Vec::new(),
        };
        let payload = export_request(
            "mtui",
            &[scope_logs(OTEL_SCOPE_AUDIT, &[audit_record(&audit)])],
        );
        assert!(
            payload.windows(16).any(|w| w == trace),
            "trace_id bytes present"
        );
        assert!(
            payload.windows(8).any(|w| w == span),
            "span_id bytes present"
        );
    }

    #[test]
    fn kwarg_allowlist_indexes_only_closed_keys() {
        use serde_json::json;
        let kwargs: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
            "template": "SUSE:Maintenance:1:1",
            "background": true,
            "command": ["rm", "-rf", "/"],
            "all_templates": false,
        }))
        .expect("object");
        let attrs = kwarg_otlp_attrs("run", &kwargs);
        let blob: Vec<u8> = attrs.concat();
        for key in ["mtui.template", "mtui.background", "mtui.all_templates"] {
            assert!(
                blob.windows(key.len()).any(|w| w == key.as_bytes()),
                "{key} indexed"
            );
        }
        // Full kwargs stay in the body only: the destructive command never
        // becomes an indexed attribute name.
        assert!(
            !blob.windows(b"command".len()).any(|w| w == b"command"),
            "non-allowlisted kwargs never indexed"
        );
        // `config_set` contributes no kwarg attrs (full redact).
        let secret_kwargs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(json!({"template": "x", "attribute": "gitea_token"}))
                .expect("object");
        assert!(
            kwarg_otlp_attrs("config_set", &secret_kwargs).is_empty(),
            "config_set contributes no kwarg attrs"
        );
    }

    #[test]
    fn kwarg_allowlist_excludes_file_body_payloads() {
        // File-body payloads ride the JSONL body only as `{bytes, sha256}`;
        // they must never become indexed attributes either.
        use serde_json::json;
        for (tool, kwargs) in [
            (
                "put",
                json!({"filename": "id_rsa", "content": "SECRET", "template": "t"}),
            ),
            (
                "testreport_write",
                json!({"content": "SECRET", "relpath": "log"}),
            ),
            (
                "testreport_patch",
                json!({"replacement": "SECRET", "start_line": 1}),
            ),
        ] {
            let kwargs: serde_json::Map<String, serde_json::Value> =
                serde_json::from_value(kwargs).expect("object");
            let blob: Vec<u8> = kwarg_otlp_attrs(tool, &kwargs).concat();
            assert!(
                !blob.windows(b"SECRET".len()).any(|w| w == b"SECRET"),
                "{tool}: payload never indexed"
            );
            assert!(
                !blob.windows(b"content".len()).any(|w| w == b"content"),
                "{tool}: payload key never indexed"
            );
        }
    }

    #[test]
    fn diag_targets_exclude_exporter_and_http_stack() {
        for excluded in [
            "mtui_mcp::otel",
            "mtui_mcp::otel::probe",
            "reqwest",
            "reqwest::connect",
            "hyper",
            "hyper_util::client::legacy::pool",
            "rustls",
            "h2",
        ] {
            assert!(diag_target_excluded(excluded), "{excluded} excluded");
        }
        assert!(!diag_target_excluded("mtui_mcp::server"));
        assert!(!diag_target_excluded("mtui_core::commands"));
    }

    #[test]
    fn diag_fields_escape_controls_and_cap() {
        assert_eq!(
            escape_diag_field("a\nb\rc\td\\e\"f"),
            "a\\nb\\rc\\td\\\\e\\\"f"
        );
        assert_eq!(escape_diag_field("a\x01b\x7fz"), "a\\u0001b\\u007fz");
        let long = "x".repeat(2000);
        assert_eq!(escape_diag_field(&long).chars().count(), DIAG_FIELD_CAP);
    }

    #[test]
    fn secret_holders_have_no_debug_leak() {
        // `SecretBox` has no `Debug` by design: any `{:?}` use fails to build,
        // so the compiler — not a runtime assertion — enforces the property.
        // What this test pins is the other half: every container's manual
        // `Debug` redacts the values it holds.
        let secret_endpoint = "https://secret-collector/x";
        let config = OtelConfig {
            endpoint: SecretBox::new(secret_endpoint.to_owned()),
            headers: vec![(
                "authorization".to_owned(),
                SecretBox::new("Bearer secret-header-value".to_owned()),
            )],
            service_name: "mtui".to_owned(),
        };
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("secret-collector"),
            "endpoint redacted: {debug}"
        );
        assert!(
            !debug.contains("secret-header-value"),
            "header value redacted: {debug}"
        );
        assert!(
            debug.contains("<redacted>"),
            "redaction marker present: {debug}"
        );
        assert!(debug.contains("mtui"), "non-secret still visible: {debug}");
    }

    #[tokio::test]
    async fn exporter_debug_redacts_endpoint() {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test client");
        let exporter = OtelExporter::with_client(
            OtelConfig::for_tests("https://secret-collector/y", "mtui"),
            client,
        );
        let debug = format!("{exporter:?}");
        assert!(
            !debug.contains("secret-collector"),
            "endpoint redacted: {debug}"
        );
        assert!(
            debug.contains("<redacted>"),
            "redaction marker present: {debug}"
        );
        exporter.shutdown().await;
    }

    #[test]
    fn classify_never_carries_url() {
        // Closed vocabulary pin: every reason renders without a host.
        for reason in [
            ExportReason::Timeout,
            ExportReason::Connect,
            ExportReason::Status,
            ExportReason::Body,
            ExportReason::Encode,
        ] {
            assert!(!reason.as_str().contains("http"));
            assert!(!reason.as_str().contains(":"));
        }
    }

    #[tokio::test]
    async fn wiremock_export_posts_protobuf_with_verbatim_body() {
        let server = wiremock::MockServer::start().await;
        let body_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let seen = Arc::clone(&body_seen);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/logs"))
            .respond_with(move |req: &wiremock::Request| {
                seen.lock()
                    .expect("body slot")
                    .extend_from_slice(req.body.as_slice());
                wiremock::ResponseTemplate::new(200)
            })
            .mount(&server)
            .await;
        let endpoint = format!("{}/v1/logs", server.uri());
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("test client");
        let exporter = OtelExporter::with_client(OtelConfig::for_tests(&endpoint, "mtui"), client);
        exporter
            .enqueue_audit(QueuedAudit {
                seq: 1,
                jsonl: r#"{"v":1,"tool":"whoami","seq":1}"#.to_owned(),
                tool: "whoami".to_owned(),
                outcome: "ok".to_owned(),
                event: "call".to_owned(),
                transport: "stdio".to_owned(),
                session_id: 9,
                response_bytes: None,
                trace: None,
                time_nanos: now_nanos(),
                extra_attrs: Vec::new(),
            })
            .expect("enqueue");
        // One flush interval plus margin; the background task posts async.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let body = body_seen.lock().expect("body").clone();
        assert!(!body.is_empty(), "exporter must POST one batch");
        assert!(
            body.windows(b"whoami".len()).any(|w| w == b"whoami"),
            "protobuf must carry the verbatim JSONL tool"
        );
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn lost_batch_merges_into_gap_sent_on_recovery() {
        // Direct `export_batch`: a failed batch merges into the pending gap,
        // and the next success sends the gap first.
        let failing = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&failing)
            .await;
        let recovering = wiremock::MockServer::start().await;
        let gap_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let seen = Arc::clone(&gap_seen);
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/logs"))
            .respond_with(move |req: &wiremock::Request| {
                seen.lock()
                    .expect("gap slot")
                    .extend_from_slice(req.body.as_slice());
                wiremock::ResponseTemplate::new(200)
            })
            .mount(&recovering)
            .await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("test client");
        let healthy = Arc::new(AtomicBool::new(true));
        let lost = Arc::new(AtomicU64::new(0));
        let mut gap: Option<(u64, u64, usize)> = None;
        let failing_config = TaskConfig {
            endpoint: format!("{}/v1/logs", failing.uri()),
            headers: Vec::new(),
            service: "mtui".to_owned(),
        };
        let audit = QueuedAudit {
            seq: 21,
            jsonl: "{}".to_owned(),
            tool: "run".to_owned(),
            outcome: "ok".to_owned(),
            event: "call".to_owned(),
            transport: "stdio".to_owned(),
            session_id: 1,
            response_bytes: None,
            trace: None,
            time_nanos: now_nanos(),
            extra_attrs: Vec::new(),
        };
        export_batch(
            &client,
            &failing_config,
            &healthy,
            &lost,
            &mut gap,
            vec![audit],
            Vec::new(),
        )
        .await;
        assert!(!healthy.load(Ordering::Relaxed), "failure latches");
        assert_eq!(gap, Some((21, 21, 1)));
        assert_eq!(lost.load(Ordering::Relaxed), 1);
        // Recovery on a good endpoint sends the gap first and clears it.
        let good_config = TaskConfig {
            endpoint: format!("{}/v1/logs", recovering.uri()),
            headers: Vec::new(),
            service: "mtui".to_owned(),
        };
        export_batch(
            &client,
            &good_config,
            &healthy,
            &lost,
            &mut gap,
            Vec::new(),
            Vec::new(),
        )
        .await;
        assert_eq!(gap, None, "gap clears after it is sent");
        let body = gap_seen.lock().expect("gap body").clone();
        assert!(
            body.windows(b"audit_gap".len()).any(|w| w == b"audit_gap"),
            "recovery batch carries the gap record"
        );
    }

    #[tokio::test]
    async fn queue_full_refuses_never_drops() {
        // Fill the bounded audit queue synchronously (no await, so the
        // background flush cannot interleave) and require the next enqueue
        // to refuse rather than drop.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("test client");
        // Unreachable endpoint: no flush can succeed during the fill, but the
        // fill itself finishes before the first 500ms tick either way.
        let exporter = OtelExporter::with_client(
            OtelConfig::for_tests("http://127.0.0.1:9/v1/logs", "mtui"),
            client,
        );
        for seq in 0..OTEL_QUEUE_CAP {
            exporter
                .enqueue_audit(QueuedAudit {
                    seq: seq as u64,
                    jsonl: "{}".to_owned(),
                    tool: "run".to_owned(),
                    outcome: "ok".to_owned(),
                    event: "call".to_owned(),
                    transport: "stdio".to_owned(),
                    session_id: 1,
                    response_bytes: None,
                    trace: None,
                    time_nanos: 0,
                    extra_attrs: Vec::new(),
                })
                .expect("queue accepts to capacity");
        }
        assert_eq!(
            exporter.enqueue_audit(QueuedAudit {
                seq: OTEL_QUEUE_CAP as u64,
                jsonl: "{}".to_owned(),
                tool: "run".to_owned(),
                outcome: "ok".to_owned(),
                event: "call".to_owned(),
                transport: "stdio".to_owned(),
                session_id: 1,
                response_bytes: None,
                trace: None,
                time_nanos: 0,
                extra_attrs: Vec::new(),
            }),
            Err(EnqueueError::Full),
            "queue full refuses, never drops"
        );
        // Diagnostics never refuse: a full diagnostics queue drops with a
        // counter instead.
        for _ in 0..OTEL_QUEUE_CAP + 10 {
            exporter.enqueue_diag(QueuedDiag {
                message: "m".to_owned(),
                level: "info".to_owned(),
                target: "t".to_owned(),
                time_nanos: 0,
            });
        }
        assert!(
            exporter.diag_dropped() > 0,
            "diagnostics drop with a counter"
        );
        exporter.shutdown().await;
    }

    #[tokio::test]
    async fn failed_batch_latches_unhealthy_and_reports_gap_on_recovery() {
        // First endpoint 500s everything; exporter latches unhealthy.
        let failing = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&failing)
            .await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("test client");
        let exporter = OtelExporter::with_client(
            OtelConfig::for_tests(&format!("{}/v1/logs", failing.uri()), "mtui"),
            client,
        );
        exporter
            .enqueue_audit(QueuedAudit {
                seq: 11,
                jsonl: "{}".to_owned(),
                tool: "run".to_owned(),
                outcome: "ok".to_owned(),
                event: "call".to_owned(),
                transport: "stdio".to_owned(),
                session_id: 1,
                response_bytes: None,
                trace: None,
                time_nanos: now_nanos(),
                extra_attrs: Vec::new(),
            })
            .expect("first enqueue while healthy");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!exporter.is_healthy(), "failed batch must latch unhealthy");
        assert_eq!(exporter.audit_lost(), 1);
        // Foreground calls now refuse instead of queuing into a dead sink.
        assert_eq!(
            exporter.enqueue_audit(QueuedAudit {
                seq: 12,
                jsonl: "{}".to_owned(),
                tool: "run".to_owned(),
                outcome: "ok".to_owned(),
                event: "call".to_owned(),
                transport: "stdio".to_owned(),
                session_id: 1,
                response_bytes: None,
                trace: None,
                time_nanos: now_nanos(),
                extra_attrs: Vec::new(),
            }),
            Err(EnqueueError::Unhealthy),
            "unhealthy refuses"
        );
        exporter.shutdown().await;
    }
}
