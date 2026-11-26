//! A read-only-plus-one-write client for the TeReGen Report API
//! (`qam.suse.de/api/v1`).
//!
//! The v1 API serves the live queue and per-report status: the update queue,
//! SMELT's checker runs, the template's Minion job state and the regenerate
//! write. The report itself is the v2 document ([`document`]), not anything
//! served here.
//!
//! Most reads are best-effort: any failure returns `None` so a TeReGen hiccup
//! never breaks the surrounding command. Two exceptions:
//! [`updates`](TeReGen::updates) and [`checkers`](TeReGen::checkers) return a
//! `Result`, so their caller can tell a genuinely-empty answer apart from an
//! unreachable or failing TeReGen, and name the status and reason TeReGen
//! gave; and the write
//! [`regenerate`](TeReGen::regenerate) returns `None` only when TeReGen is
//! *unreachable*, and tells a refusal (409) from a failed request.
//!
//! The base URL comes from the `[teregen] api` option (default
//! `https://qam.suse.de/api/v1`), but [`TeReGen::new`] takes it explicitly
//! rather than reading `config.teregen_api` itself, as the
//! [`Gitea`](crate::gitea) constructor does.
//!
//! `wait_for_template`'s inter-poll wait polls `should_stop` in small
//! [`tokio::time::sleep`] steps, so cancellation takes effect promptly rather
//! than after a full interval.

use std::time::Duration;

use mtui_config::Config;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{HttpError, TeReGenError};
use crate::http::{
    HTTP_TIMEOUT, HttpClient, MAX_API_BODY, PATIENT_READ_TIMEOUT, join_url, query_string,
    read_body_capped, sanitize_url,
};

pub mod auth;
pub mod document;
pub mod tokens;

use document::cap_detail;

pub use auth::{AuthHint, AuthStage, DEFAULT_NAMESPACE, TeregenAuth, TeregenAuthError};
pub use document::{
    ArtifactEntry, ArtifactOrigin, ArtifactStored, ArtifactUploadError, DocumentFetch,
    Precondition, TeregenV2, TeregenV2Error, TeregenV2WriteError, UploadOutcome,
    is_valid_artifact_name,
};
pub use tokens::{CachedToken, TokenStore, describe_age};

/// What TeReGen answered to `POST /reports/{id}/regenerate`.
///
/// Anything but [`Enqueued`](Self::Enqueued) means no job was started. Only a
/// 409 is a *refusal* of this request; any other error status is a failed
/// request, which no flag can lift.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegenRequest {
    /// `2xx`: the job was enqueued. `job` is the id TeReGen returned, if any.
    Enqueued {
        /// The enqueued job id.
        job: Option<Value>,
    },
    /// `409`: TeReGen refuses to regenerate, with its own reason (its `error`
    /// text, else `HTTP 409`).
    Refused(String),
    /// Any other non-2xx status: the request failed. `error` is TeReGen's
    /// `error` text, else the messages of a validator's `errors` array; either
    /// is external text, capped but not sanitized.
    Failed {
        /// The HTTP status code.
        status: u16,
        /// TeReGen's explanation, when it gave one.
        error: Option<String>,
    },
}

/// The result of a regenerate-and-wait attempt (see
/// [`TeReGen::regenerate_and_wait`]).
///
/// Exactly one of the flags is meaningful at a time; `ok` is the only success.
/// Carries just enough for a caller to message the user and decide whether to
/// reload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegenOutcome {
    /// The job finished and the template is built.
    pub ok: bool,
    /// TeReGen could not be asked at all.
    pub unreachable: bool,
    /// The server refused the request (message from its `{"error": …}` body).
    pub error: Option<String>,
    /// The request failed with an error status other than a refusal: TeReGen's
    /// status and reason, rendered as one line. No flag can lift it.
    pub failed: Option<String>,
    /// The final Minion state when the job ran but did not finish.
    pub state: Option<String>,
    /// The Minion error message, when present.
    pub minion_error: Option<String>,
    /// The enqueued job id, for logging.
    pub job: Option<Value>,
}

/// One checker run for a report: the checker that produced it, its per-status
/// counts, and the per-check verdicts in [`results`](Self::results).
///
/// **Only what the renderer reads is modelled.** Every other served key
/// (`run_id`, `started`, `finished`, …) is left out on purpose: modelling a key
/// is a liability, since one served as `null` or with a drifted type fails the
/// whole element, and `null` is exactly what an in-progress run's `finished`
/// looks like (#522).
///
/// Tolerance of an *added* key comes from serde ignoring unknown fields, i.e.
/// from not setting `deny_unknown_fields`. The container-level
/// `#[serde(default)]` covers the other direction: a **missing** key degrades to
/// an empty value rather than the `?` this shape replaced. A *type* change on a
/// modelled key still fails the element on purpose, so [`TeReGen::checkers`] can
/// warn about the drift instead of silently substituting zeroes.
///
/// The run carries no status of its own — the status vocabulary is what the
/// `*_count` fields enumerate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CheckerRun {
    /// The checker that produced this run (the run's display name).
    pub checker_type: String,
    /// Number of `results` that passed.
    pub pass_count: u64,
    /// Number of `results` that failed.
    pub fail_count: u64,
    /// Number of `results` that warned.
    pub warn_count: u64,
    /// Number of `results` that were skipped.
    pub skip_count: u64,
    /// Number of `results` that errored.
    pub error_count: u64,
    /// Number of `results` that hit a recoverable error.
    pub recerror_count: u64,
    /// Number of `results` still running.
    pub running_count: u64,
    /// Number of `results` waiting to run.
    pub wait_count: u64,
    /// Number of `results` in an unknown state.
    pub unknown_count: u64,
    /// The per-check verdicts, parsed element-wise (see `lenient_results`).
    /// Absent in the payload means "no rows", not `?`.
    #[serde(deserialize_with = "lenient_results")]
    pub results: Vec<CheckerResult>,
}

/// One check's verdict inside a [`CheckerRun`]. Modelled and defaulted on the
/// same terms as its parent: only the three keys the renderer reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CheckerResult {
    /// The check that produced this verdict.
    pub check_type: String,
    /// The verdict, e.g. `PASS`/`WARN`. Compared case-insensitively and never
    /// treated as a closed set — an unrecognised value is rendered verbatim.
    pub status: String,
    /// The check's output; the finding itself for a non-passing verdict.
    pub output: String,
}

/// Deserialize a run's `results` element-wise, so one drifted verdict costs its
/// own row instead of the whole run — header and good siblings included (#522).
///
/// A `results` served as something other than an array degrades to no rows for
/// the same reason.
fn lenient_results<'de, D>(de: D) -> Result<Vec<CheckerResult>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(de)?;
    let Some(elements) = value.as_array() else {
        tracing::warn!("TeReGen checkers: `results` is not an array; rendering the run with none");
        return Ok(Vec::new());
    };
    Ok(lenient_elements("", "result", elements))
}

/// Deserialize `elements` one at a time, dropping any that does not match `T`.
///
/// The warning names the offending element's **key set only**: these payloads
/// carry customer-visible check output, so neither the body nor serde's error
/// (which quotes the offending value) may reach the log (#431, #439).
/// `context` is appended to `TeReGen checkers` where the caller has an RRID.
fn lenient_elements<T>(context: &str, what: &str, elements: &[Value]) -> Vec<T>
where
    T: for<'de> Deserialize<'de>,
{
    elements
        .iter()
        .filter_map(|element| match T::deserialize(element) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                tracing::warn!(
                    "TeReGen checkers{context}: skipping a {what} that does not match the \
                     expected schema; its keys were [{}]",
                    json_keys(element)
                );
                None
            }
        })
        .collect()
}

/// Best-effort read-only (plus one write) TeReGen Report API client.
#[derive(Clone)]
pub struct TeReGen {
    base: String,
    http: HttpClient,
}

impl std::fmt::Debug for TeReGen {
    /// `base` may embed `user:pass@`; a derive would echo it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeReGen")
            .field("base", &sanitize_url(&self.base))
            .field("http", &self.http)
            .finish()
    }
}

impl TeReGen {
    /// Build a client targeting `apiurl`, deriving the TLS posture from
    /// `config.ssl_verify`.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] if the shared HTTP client cannot be built (e.g. a
    /// configured CA bundle cannot be read).
    pub fn new(config: &Config, apiurl: &str) -> crate::Result<Self> {
        let http = HttpClient::from_config(config)?;
        Ok(Self::with_client(http, apiurl))
    }

    /// Build a client from an already-constructed [`HttpClient`], bypassing
    /// [`Config`].
    ///
    /// The composition-root / test seam, letting a caller inject a client whose
    /// TLS posture (or base host, under `wiremock`) is already fixed.
    #[must_use]
    pub fn with_client(http: HttpClient, apiurl: &str) -> Self {
        Self {
            base: apiurl.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// GET `path` and return the decoded JSON body, or `None` on any transport
    /// failure, non-2xx status, or invalid JSON — callers never see an error.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Option<Value> {
        self.fetch(self.http.inner(), HTTP_TIMEOUT.1, path, query)
            .await
            .ok()
    }

    /// GET `path`, surfacing failures as `Err`.
    ///
    /// The fallible sibling of [`get`](Self::get): failures become
    /// [`TeReGenError`] instead of `None`, so a caller can distinguish a failed
    /// lookup from a genuinely-empty successful response. The descriptions are
    /// URL-free because every `reqwest::Error` is converted to [`HttpError`]
    /// — which strips the URL — before it is stringified (#431), and the log line
    /// for an error status names the status alone, never the body. Waits up to
    /// [`SMELT_BACKED_TIMEOUT`], on a client whose read timeout allows it.
    async fn try_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, TeReGenError> {
        let client = self
            .http
            .patient()
            .map_err(|e| TeReGenError::Fetch(e.to_string()))?;
        self.fetch(client, SMELT_BACKED_TIMEOUT, path, query).await
    }

    async fn fetch(
        &self,
        client: &reqwest::Client,
        timeout: Duration,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, TeReGenError> {
        let url = join_url(&self.base, path, &query_string(query));
        let request = client.get(&url).timeout(timeout);
        let response = request.send().await.map_err(|e| {
            let e = HttpError::from(e);
            tracing::debug!("TeReGen GET {path} failed: {e}");
            TeReGenError::Fetch(e.to_string())
        })?;
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            tracing::debug!("TeReGen GET {path} failed: HTTP {status}");
            let error = match read_body_capped(response, MAX_API_BODY).await {
                Ok(bytes) => error_text(&bytes),
                Err(_) => None,
            };
            return Err(TeReGenError::Status {
                status: status.as_u16(),
                error,
            });
        }
        let bytes = read_body_capped(response, MAX_API_BODY)
            .await
            .map_err(|e| {
                tracing::debug!("TeReGen GET {path} failed: {e}");
                TeReGenError::Fetch(e.to_string())
            })?;
        serde_json::from_slice::<Value>(&bytes).map_err(|e| {
            tracing::debug!("TeReGen GET {path} returned invalid JSON: {e}");
            TeReGenError::Fetch(format!("invalid JSON: {e}"))
        })
    }

    /// The main report endpoint (`GET /reports/{id}`): id, file list, and the
    /// live `priority`/`deadline` (refreshed from SMELT for SLFO). Returns
    /// `None` unless the body is a JSON object.
    pub async fn info(&self, rrid: &str) -> Option<Value> {
        let d = self.get(&format!("reports/{rrid}"), &[]).await?;
        d.is_object().then_some(d)
    }

    /// Template existence + Minion job state for a report, or `None`.
    async fn status(&self, rrid: &str) -> Option<Value> {
        let d = self.get(&format!("reports/{rrid}/status"), &[]).await?;
        d.is_object().then_some(d)
    }

    /// Live checker (build-check) result runs for a report.
    ///
    /// An element that does not deserialise as a [`CheckerRun`] is skipped and
    /// warned about by its **key set only** — never the body nor serde's error
    /// (which quotes the offending value), both of which carry customer-visible
    /// check output (#431, #439). A drifted verdict inside a run is skipped the
    /// same way, so it costs one row rather than the whole run.
    ///
    /// # Errors
    ///
    /// Returns [`TeReGenError::Status`] for a non-2xx answer, carrying
    /// TeReGen's own `error` text, and [`TeReGenError::Fetch`] for a transport
    /// failure, a body that is not JSON, or one without a `checkers` array, so
    /// the caller can tell a failed lookup apart from none. A 404 is no
    /// exception: TeReGen answers it, with its reason, when it has no checker
    /// results to give (every non-SLFO id, or an SLFO id SMELT does not know);
    /// a 502 or 504 means SMELT failed or timed out, so the results are
    /// unknown.
    pub async fn checkers(&self, rrid: &str) -> Result<Vec<CheckerRun>, TeReGenError> {
        let body = self
            .try_get(&format!("reports/{rrid}/checkers"), &[])
            .await?;
        let Some(elements) = body.get("checkers").and_then(Value::as_array) else {
            tracing::debug!(
                "TeReGen checkers for {rrid}: response carries no `checkers` array; its keys were \
                 [{}]",
                json_keys(&body)
            );
            return Err(TeReGenError::Fetch(
                "the response carries no `checkers` array".to_owned(),
            ));
        };
        Ok(lenient_elements(&format!(" for {rrid}"), "run", elements))
    }

    /// The unreleased update queue (live from SMELT). Each [`UpdatesQuery`]
    /// field maps to a server-side query param of the same name.
    ///
    /// # Errors
    ///
    /// Returns [`TeReGenError::Status`] for a non-2xx answer (502/504 when SMELT
    /// failed or timed out, with TeReGen's own `error` text) and
    /// [`TeReGenError::Fetch`] for a transport or JSON failure, so the caller
    /// can tell a failing TeReGen apart from an empty queue. `Ok(None)` is a
    /// successful response whose body carried no `updates` key.
    pub async fn updates(&self, opts: &UpdatesQuery<'_>) -> Result<Option<Value>, TeReGenError> {
        let mut params: Vec<(&str, String)> = Vec::new();
        for group in opts.review_groups.iter().filter(|g| !g.is_empty()) {
            params.push(("review_group", (*group).to_string()));
        }
        for (name, value) in [("status", opts.status), ("assignee", opts.assignee)] {
            if let Some(v) = value
                && !v.is_empty()
            {
                params.push((name, v.to_string()));
            }
        }
        for (flag, name) in [
            (opts.unassigned, "unassigned"),
            (opts.with_assignment, "with_assignment"),
            (opts.no_cache, "no_cache"),
        ] {
            if flag {
                params.push((name, "1".to_string()));
            }
        }
        let d = self.try_get("updates", &params).await?;
        Ok(d.get("updates").cloned())
    }

    /// Enqueue a template regeneration job
    /// (`POST /reports/{id}/regenerate`).
    ///
    /// `force_overwrite` overwrites an existing but *unedited* template;
    /// `ignore_inconsistent` regenerates despite inconsistent metadata (e.g. an
    /// arch list that disagrees with the build).
    ///
    /// Returns [`RegenRequest::Enqueued`] on success (HTTP 202),
    /// [`RegenRequest::Refused`] when the server refuses (HTTP 409: the template
    /// already exists or was hand-edited), [`RegenRequest::Failed`] for any other
    /// error status, and `None` only when TeReGen is unreachable — so callers can
    /// tell "refused" and "failed" apart from "couldn't ask".
    pub async fn regenerate(
        &self,
        rrid: &str,
        force_overwrite: bool,
        ignore_inconsistent: bool,
    ) -> Option<RegenRequest> {
        let url = format!("{}/reports/{rrid}/regenerate", self.base);
        let payload = json!({
            "force_overwrite": force_overwrite,
            "ignore_inconsistent": ignore_inconsistent,
        });
        let response = match self
            .http
            .inner()
            .post(&url)
            .timeout(HTTP_TIMEOUT.1)
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Convert before logging: reqwest's Display appends the URL (#431).
                tracing::debug!(
                    "TeReGen POST regenerate {rrid} failed: {}",
                    HttpError::from(e)
                );
                return None;
            }
        };
        let status = response.status();
        let body = read_body_capped(response, MAX_API_BODY)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if status.is_success() {
            let job = body.as_ref().and_then(|b| b.get("job")).cloned();
            return Some(RegenRequest::Enqueued { job });
        }
        let error = body.as_ref().and_then(failure_text);
        if status == reqwest::StatusCode::CONFLICT {
            return Some(RegenRequest::Refused(
                error.unwrap_or_else(|| "HTTP 409".to_owned()),
            ));
        }
        Some(RegenRequest::Failed {
            status: status.as_u16(),
            error,
        })
    }

    /// Poll [`status`](Self::status) until the latest generate job finishes or
    /// fails, or `timeout` elapses / `should_stop` returns `true`.
    ///
    /// Returns the final status dict once `minion_state` is `finished` or
    /// `failed`, else the last-seen status (or `None`) on timeout; the caller
    /// inspects `minion_state` / `minion_error` to decide success.
    /// `should_stop` is polled before each sleep *and* inside it, so a caller
    /// can abandon the wait promptly and still get the last seen status.
    async fn wait_for_template<F>(
        &self,
        rrid: &str,
        interval: Duration,
        timeout: Duration,
        mut should_stop: F,
    ) -> Option<Value>
    where
        F: FnMut() -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let last = self.status(rrid).await;
            if let Some(v) = &last
                && matches!(
                    v.get("minion_state").and_then(Value::as_str),
                    Some("finished" | "failed")
                )
            {
                return last;
            }
            if should_stop() || tokio::time::Instant::now() >= deadline {
                return last;
            }
            // Small steps so a stop request is honoured without waiting out
            // the whole interval.
            let step = Duration::from_millis(100);
            let mut waited = Duration::ZERO;
            while waited < interval && !should_stop() {
                let this_step = step.min(interval - waited);
                tokio::time::sleep(this_step).await;
                waited += step;
            }
        }
    }

    /// Enqueue a regeneration and wait for the job to finish.
    ///
    /// Bundles [`regenerate`](Self::regenerate) + `wait_for_template` into the
    /// single protocol both the `regenerate` command and the stale-template
    /// loader share, returning a [`RegenOutcome`] the caller maps to its own
    /// messaging and reload strategy.
    pub async fn regenerate_and_wait<F>(
        &self,
        rrid: &str,
        force_overwrite: bool,
        ignore_inconsistent: bool,
        should_stop: F,
    ) -> RegenOutcome
    where
        F: FnMut() -> bool,
    {
        let request = self
            .regenerate(rrid, force_overwrite, ignore_inconsistent)
            .await;
        let job = match request {
            None => {
                return RegenOutcome {
                    ok: false,
                    unreachable: true,
                    ..Default::default()
                };
            }
            Some(RegenRequest::Refused(error)) => {
                return RegenOutcome {
                    ok: false,
                    error: Some(error),
                    ..Default::default()
                };
            }
            Some(RegenRequest::Failed { status, error }) => {
                return RegenOutcome {
                    ok: false,
                    failed: Some(TeReGenError::Status { status, error }.to_string()),
                    ..Default::default()
                };
            }
            Some(RegenRequest::Enqueued { job }) => job,
        };
        let status = self
            .wait_for_template(rrid, DEFAULT_INTERVAL, DEFAULT_TIMEOUT, should_stop)
            .await;
        let state = status
            .as_ref()
            .and_then(|s| s.get("minion_state"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if state.as_deref() != Some("finished") {
            let minion_error = status
                .as_ref()
                .and_then(|s| s.get("minion_error"))
                .and_then(Value::as_str)
                .map(str::to_string);
            return RegenOutcome {
                ok: false,
                state,
                minion_error,
                job,
                ..Default::default()
            };
        }
        RegenOutcome {
            ok: true,
            job,
            ..Default::default()
        }
    }
}

/// The string `error` of a JSON-object body, cut to [`MAX_DETAIL_LEN`](document::MAX_DETAIL_LEN) on a
/// char boundary. Anything else (HTML, empty, no string `error`) is `None`.
fn error_text(bytes: &[u8]) -> Option<String> {
    error_field(&serde_json::from_slice::<Value>(bytes).ok()?)
}

fn error_field(body: &Value) -> Option<String> {
    let text = body.get("error")?.as_str().filter(|t| !t.is_empty())?;
    Some(cap_detail(text))
}

/// TeReGen's explanation of a failed request, from a JSON body: its string
/// `error`, else the `message`s of a validator's `errors` array joined with
/// `; `. Cut to [`MAX_DETAIL_LEN`](document::MAX_DETAIL_LEN) on a char boundary.
fn failure_text(body: &Value) -> Option<String> {
    if body.get("error").is_some_and(Value::is_string) {
        return error_field(body);
    }
    let text = body
        .get("errors")?
        .as_array()?
        .iter()
        .filter_map(|e| e.get("message").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("; ");
    (!text.is_empty()).then(|| cap_detail(&text))
}

/// The comma-joined top-level key names of a JSON object, for a schema-drift
/// warning. Names only — never values (#431, #439).
fn json_keys(v: &Value) -> String {
    v.as_object().map_or_else(
        || "<not an object>".to_string(),
        |o| o.keys().map(String::as_str).collect::<Vec<_>>().join(", "),
    )
}

/// How long the SMELT-backed reads ([`TeReGen::checkers`], [`TeReGen::updates`])
/// wait for an answer. TeReGen bounds its own SMELT calls at 10 s to connect plus
/// 40 s of inactivity, so anything shorter than that cuts its 504 off.
const SMELT_BACKED_TIMEOUT: Duration = PATIENT_READ_TIMEOUT;

/// Default poll interval for [`TeReGen::wait_for_template`].
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// Default overall wait for [`TeReGen::wait_for_template`].
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Optional filters for [`TeReGen::updates`].
///
/// All fields default to unset; empty string filters and `false` flags are
/// omitted from the request query.
#[derive(Debug, Clone, Default)]
pub struct UpdatesQuery<'a> {
    /// Narrow the queue to these review groups (server-side), sent as a repeated
    /// `review_group`. TeReGen serves the union of the groups, and accepts the
    /// bare name (`qam-sle`) and the review login (`qam-sle-review`) alike. Empty
    /// means no narrowing.
    pub review_groups: &'a [&'a str],
    /// Narrow the queue by status (server-side).
    pub status: Option<&'a str>,
    /// Keep only updates assigned to that user; implies `status=testing`.
    pub assignee: Option<&'a str>,
    /// Keep only updates with no assignee; implies `status=testing`.
    pub unassigned: bool,
    /// Include assignment on every row without filtering; implies
    /// `status=testing`.
    pub with_assignment: bool,
    /// Bypass the server's short assignment cache.
    pub no_cache: bool,
}

#[cfg(test)]
mod tests {
    use super::document::MAX_DETAIL_LEN;
    use super::*;
    use crate::http::VerifyPolicy;
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const RRID: &str = "SUSE:SLFO:1.2:5702";

    /// A base URL where nothing listens, so a request fails inside `send`
    /// rather than on a status — the arm wiremock cannot reach.
    const CLOSED_PORT: &str = "http://127.0.0.1:1";

    #[test]
    fn debug_redacts_base_userinfo() {
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        let c = TeReGen::with_client(http, "https://u:p@qam.example.invalid/api/v1");
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("u:p"), "{rendered}");
        assert!(rendered.contains("qam.example.invalid"), "{rendered}");
    }

    fn client(server: &MockServer) -> TeReGen {
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        TeReGen::with_client(http, &server.uri())
    }

    /// A client pointed at [`CLOSED_PORT`].
    fn unreachable_client() -> TeReGen {
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        TeReGen::with_client(http, CLOSED_PORT)
    }

    thread_local! {
        /// The active capture buffer for this thread, if any. Events on a
        /// thread with no active capture are dropped.
        static SINK: std::cell::RefCell<Option<Vec<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Install — once per test binary — a permissive **global** subscriber that
    /// forwards every event into the calling thread's [`SINK`].
    ///
    /// Global rather than `tracing::subscriber::set_default`'s thread-local
    /// because `tracing` caches callsite *interest* process-wide: a callsite
    /// first reached from a thread with no subscriber is cached
    /// `Interest::never()` and stays silent for every later capture, which made
    /// `regenerate_none_when_unreachable` flake ~30% of runs. Always installing
    /// pins interest at `always`; scoping moves to the thread-local sink.
    /// Canonical copy of the pattern: `tests/log_capture.rs`.
    fn install_capture_subscriber() {
        use std::fmt::Write as _;
        use std::sync::OnceLock;
        use tracing::field::{Field, Visit};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
        use tracing_subscriber::registry::Registry;

        struct CaptureLayer;
        struct MessageVisitor(String);
        impl Visit for MessageVisitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    let _ = write!(self.0, "{value:?}");
                }
            }
        }
        impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                SINK.with(|s| {
                    if let Some(buf) = s.borrow_mut().as_mut() {
                        let mut v = MessageVisitor(String::new());
                        event.record(&mut v);
                        buf.push(v.0);
                    }
                });
            }
        }

        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
        });
    }

    /// Capture the `message` field of every tracing event emitted by `f` on
    /// this thread. See [`install_capture_subscriber`] for why the subscriber
    /// is global and only the sink is scoped.
    async fn capture_logs<F, Fut>(f: F) -> String
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        install_capture_subscriber();
        SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
        f().await;
        SINK.with(|s| s.borrow_mut().take())
            .unwrap_or_default()
            .join("\n")
    }

    /// The `TeReGen GET … failed` line matching `needle`, or a panic naming the
    /// whole capture — so an assertion can never pass because the line the test
    /// is about was never emitted.
    fn failure_line<'a>(logs: &'a str, needle: &str) -> &'a str {
        logs.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} line in capture: {logs}"))
    }

    /// The budget must outlast TeReGen's own SMELT limits (10 s + 40 s), or its
    /// 504 can never arrive.
    #[test]
    fn smelt_backed_timeout_outlasts_teregens_smelt_budget() {
        assert!(SMELT_BACKED_TIMEOUT >= Duration::from_secs(55));
    }

    /// The SMELT-backed reads outwait the shared client's read timeout. A
    /// per-request `timeout` cannot do it: reqwest's client-level read timeout
    /// still fires first.
    #[tokio::test]
    async fn smelt_backed_reads_outwait_the_shared_read_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(checkers_path()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"checkers": []}))
                    .set_delay(Duration::from_millis(600)),
            )
            .mount(&server)
            .await;
        let http = HttpClient::with_read_timeouts(
            VerifyPolicy::Default(false),
            Duration::from_millis(100),
            Duration::from_secs(5),
        )
        .unwrap();
        let c = TeReGen::with_client(http, &server.uri());
        // Premise: the shared client alone gives up on this server.
        let slow = format!("{}{}", server.uri(), checkers_path());
        assert!(c.http.inner().get(slow).send().await.is_err());
        assert!(c.checkers(RRID).await.unwrap().is_empty());
    }

    // --- reads: best-effort JSON decode ---

    #[tokio::test]
    async fn get_returns_decoded_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": RRID})))
            .mount(&server)
            .await;
        assert_eq!(client(&server).info(RRID).await, Some(json!({"id": RRID})));
    }

    #[tokio::test]
    async fn get_swallows_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}")))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert_eq!(client(&server).info(RRID).await, None);
    }

    /// A wiremock server can only produce a *status*, so `get`'s `send` arm is
    /// otherwise never executed. Drives it, and pins that the log line it emits
    /// carries no reqwest URL (#431).
    #[tokio::test]
    async fn get_swallows_transport_failure_without_logging_a_url() {
        let mut out = Some(json!({}));
        let logs = capture_logs(|| async {
            out = unreachable_client().info(RRID).await;
        })
        .await;
        assert_eq!(out, None);

        let line = failure_line(&logs, "TeReGen GET");
        assert!(!line.contains(" for url ("), "log rendered the URL: {line}");
        assert!(
            line.contains("error sending request"),
            "log lost the failure kind: {line}"
        );
    }

    #[tokio::test]
    async fn get_swallows_invalid_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}")))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        assert_eq!(client(&server).info(RRID).await, None);
    }

    #[tokio::test]
    async fn info_rejects_non_dict_payload() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(["x"])))
            .mount(&server)
            .await;
        assert_eq!(client(&server).info(RRID).await, None);
    }

    #[tokio::test]
    async fn status_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"template": true, "minion_state": "finished"})),
            )
            .mount(&server)
            .await;
        let c = client(&server);
        assert_eq!(
            c.status(RRID).await,
            Some(json!({"template": true, "minion_state": "finished"}))
        );
    }

    /// Mounts a `checkers` response carrying `runs`.
    async fn checkers_server(runs: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/checkers")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"checkers": runs})))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn checkers_parses_nested_runs() {
        let server = checkers_server(json!([{
            "checker_type": "checker-alpha",
            "run_id": "run-0001",
            "pass_count": 1,
            "warn_count": 1,
            "results": [
                {"check_type": "check-01", "status": "PASS"},
                {"check_type": "check-02", "status": "WARN", "output": "synthetic finding"},
            ],
        }]))
        .await;
        let runs = client(&server).checkers(RRID).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].checker_type, "checker-alpha");
        assert_eq!(runs[0].pass_count, 1);
        assert_eq!(runs[0].warn_count, 1);
        assert_eq!(runs[0].results.len(), 2);
        assert_eq!(runs[0].results[1].status, "WARN");
        assert_eq!(runs[0].results[1].output, "synthetic finding");
        // Missing keys default; the unmodelled `run_id` is ignored. Drop either
        // tolerance and the element is dropped instead, i.e. the two lengths
        // above go to 0 and 1.
        assert_eq!(runs[0].fail_count, 0);
        assert_eq!(runs[0].results[0].output, "");
    }

    /// A run whose *types* drift is dropped with a warning naming its keys —
    /// and only its keys: the payload carries customer-visible check output
    /// (#431, #439).
    #[tokio::test]
    async fn checkers_warns_with_key_set_on_schema_drift() {
        let server = checkers_server(json!([
            {"checker_type": "checker-alpha", "pass_count": "twelve",
             "results": [{"check_type": "check-01", "status": "PASS",
                          "output": "customer visible finding"}]},
            {"checker_type": "checker-beta"},
        ]))
        .await;
        let mut runs = Vec::new();
        let logs = capture_logs(|| async {
            runs = client(&server).checkers(RRID).await.unwrap();
        })
        .await;

        // The good sibling survives the bad element.
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].checker_type, "checker-beta");
        let line = failure_line(&logs, "skipping a run");
        assert!(line.contains("checker_type, pass_count, results"), "{line}");
        assert!(!line.contains("twelve"), "warning leaked a value: {line}");
        assert!(
            !line.contains("customer visible finding"),
            "warning leaked check output: {line}"
        );
    }

    /// Regression (#522): `null` and unknown keys are what an in-progress run
    /// and a server-side key addition look like on the wire, and a drifted
    /// verdict must cost its own row — not the run, its header and its
    /// siblings.
    #[tokio::test]
    async fn checkers_keeps_run_despite_unknown_keys_and_drifted_result() {
        let server = checkers_server(json!([{
            "checker_type": "checker-alpha",
            "run_id": 1234,
            "finished": null,
            "pass_count": 1,
            "results": [
                {"check_type": "check-01", "status": "PASS", "output": "",
                 "created": null},
                {"check_type": "check-02", "status": 7,
                 "output": "customer visible finding"},
            ],
        }]))
        .await;
        let mut runs = Vec::new();
        let logs = capture_logs(|| async {
            runs = client(&server).checkers(RRID).await.unwrap();
        })
        .await;

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].checker_type, "checker-alpha");
        assert_eq!(runs[0].pass_count, 1);
        assert_eq!(runs[0].results.len(), 1, "{:?}", runs[0].results);
        assert_eq!(runs[0].results[0].check_type, "check-01");
        let line = failure_line(&logs, "skipping a result");
        assert!(line.contains("check_type, output, status"), "{line}");
        assert!(
            !line.contains("customer visible finding"),
            "warning leaked check output: {line}"
        );
    }

    /// A `results` served as something other than an array costs the rows, not
    /// the run and its counts.
    #[tokio::test]
    async fn checkers_keeps_run_when_results_is_not_an_array() {
        let server = checkers_server(json!([
            {"checker_type": "checker-alpha", "pass_count": 3, "results": null},
        ]))
        .await;
        let runs = client(&server).checkers(RRID).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].pass_count, 3);
        assert!(runs[0].results.is_empty());
    }

    /// The container shape is the likeliest drift point of an inferred schema
    /// and the only one no per-element warning can see: a renamed key or a
    /// non-array `checkers` fails the lookup rather than reading as none, and
    /// the log names the keys that were served.
    #[tokio::test]
    async fn checkers_errs_when_container_is_absent_or_not_an_array() {
        for (body, expected_keys) in [
            (json!({"other": 1}), "other"),
            (json!({"checkers": {"run-1": {}}}), "checkers"),
            (json!({"checkers": null}), "checkers"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/reports/{RRID}/checkers")))
                .respond_with(ResponseTemplate::new(200).set_body_json(&body))
                .mount(&server)
                .await;
            let mut result = None;
            let logs = capture_logs(|| async {
                result = Some(client(&server).checkers(RRID).await);
            })
            .await;
            let Some(Err(TeReGenError::Fetch(msg))) = &result else {
                panic!("{body}: expected a Fetch error, got {result:?}");
            };
            assert!(msg.contains("no `checkers` array"), "{body}: {msg}");
            let line = failure_line(&logs, "no `checkers` array");
            assert!(line.contains(expected_keys), "{body}: {line}");
        }
    }

    /// A 404 included: for an SLFO id TeReGen answers it when it could not
    /// fetch the runs from SMELT. The mock must be hit: wiremock answers an
    /// unmatched path with a 404 of its own.
    #[tokio::test]
    async fn checkers_errs_on_error_status() {
        for (status, expected) in [(404, "404 Not Found"), (503, "503 Service Unavailable")] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(format!("/reports/{RRID}/checkers")))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
            let err = client(&server).checkers(RRID).await.unwrap_err();
            assert!(matches!(err, TeReGenError::Status { error: None, .. }));
            assert!(err.to_string().contains(expected), "{status}: {err}");
        }
    }

    /// Mounts one `GET` answer for `p`, expected to be hit once: wiremock answers
    /// an unmatched path with a 404 of its own.
    async fn serve(p: &str, response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    fn checkers_path() -> String {
        format!("/reports/{RRID}/checkers")
    }

    /// The statuses TeReGen answers for a failed SMELT lookup, with the body
    /// shapes it sends: the `error` text must reach the caller.
    #[tokio::test]
    async fn checkers_status_carries_teregens_reason() {
        for (status, text) in [
            (404, "SMELT does not know update SUSE:SLFO:1.2:5702"),
            (502, "SMELT checker lookup failed: SMELT is unreachable"),
            (504, "SMELT checker lookup timed out"),
        ] {
            let server = serve(
                &checkers_path(),
                ResponseTemplate::new(status).set_body_json(json!({"error": text})),
            )
            .await;
            let err = client(&server).checkers(RRID).await.unwrap_err();
            let TeReGenError::Status { status: got, error } = &err else {
                panic!("{status}: expected Status, got {err:?}");
            };
            assert_eq!(*got, status);
            assert_eq!(error.as_deref(), Some(text));
        }
    }

    /// Only a JSON object with a string `error` is trusted as TeReGen's reason;
    /// a proxy's HTML, an empty body and a validator's `{"errors": […]}` are
    /// not, and the raw body is never rendered.
    #[tokio::test]
    async fn checkers_status_without_a_reason_never_echoes_the_body() {
        for (status, response) in [
            (
                502,
                ResponseTemplate::new(502).set_body_string("<html>proxy secret-marker</html>"),
            ),
            (503, ResponseTemplate::new(503)),
            (
                400,
                ResponseTemplate::new(400).set_body_json(
                    json!({"errors": [{"message": "secret-marker"}], "status": 400}),
                ),
            ),
            (
                500,
                ResponseTemplate::new(500).set_body_json(json!({"error": 7})),
            ),
        ] {
            let server = serve(&checkers_path(), response).await;
            let err = client(&server).checkers(RRID).await.unwrap_err();
            assert!(
                matches!(&err, TeReGenError::Status { status: s, error: None } if *s == status),
                "{status}: {err:?}"
            );
            let shown = err.to_string();
            assert!(!shown.contains("secret-marker"), "{status}: {shown}");
            assert!(!shown.contains(':'), "a reason was invented: {shown}");
        }
    }

    /// The reason is bounded, and cut on a char boundary.
    #[tokio::test]
    async fn status_reason_is_capped_on_a_char_boundary() {
        // One ASCII byte shifts every 3-byte `€`, so the cap lands mid-char.
        let text = format!("x{}", "€".repeat(5000));
        let server = serve(
            &checkers_path(),
            ResponseTemplate::new(502).set_body_json(json!({"error": text})),
        )
        .await;
        let err = client(&server).checkers(RRID).await.unwrap_err();
        let TeReGenError::Status {
            error: Some(error), ..
        } = &err
        else {
            panic!("expected a reason, got {err:?}");
        };
        assert!(error.len() <= MAX_DETAIL_LEN, "{} bytes", error.len());
        assert_eq!(error.len(), MAX_DETAIL_LEN - 1);
        assert!(text.starts_with(error.as_str()));
    }

    #[tokio::test]
    async fn updates_status_carries_teregens_reason() {
        let server = serve(
            "/updates",
            ResponseTemplate::new(502)
                .set_body_json(json!({"error": "SMELT update queue lookup failed"})),
        )
        .await;
        let err = client(&server)
            .updates(&UpdatesQuery::default())
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                TeReGenError::Status { status: 502, error: Some(e) }
                    if e == "SMELT update queue lookup failed"
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "TeReGen answered 502 Bad Gateway: SMELT update queue lookup failed"
        );
    }

    /// The DEBUG line for an error status names the status, never the body or
    /// the URL.
    #[tokio::test]
    async fn status_log_line_names_the_status_only() {
        let server = serve(
            &checkers_path(),
            ResponseTemplate::new(502).set_body_json(json!({"error": "secret-marker"})),
        )
        .await;
        let logs = capture_logs(|| async {
            let _ = client(&server).checkers(RRID).await;
        })
        .await;
        let line = failure_line(&logs, "TeReGen GET");
        assert!(line.contains("502"), "{line}");
        assert!(!line.contains("secret-marker"), "{line}");
        assert!(!line.contains(" for url ("), "{line}");
    }

    // --- updates: query-param rules ---

    #[tokio::test]
    async fn updates_without_filters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": [1, 2]})))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server)
                .updates(&UpdatesQuery::default())
                .await
                .unwrap(),
            Some(json!([1, 2]))
        );
    }

    #[tokio::test]
    async fn updates_passes_query_filters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "qam-sle"))
            .and(query_param("status", "testing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server)
                .updates(&UpdatesQuery {
                    review_groups: &["qam-sle"],
                    status: Some("testing"),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Some(json!([]))
        );
    }

    /// Every group goes out as its own `review_group`, in order, and an empty
    /// name is dropped.
    #[tokio::test]
    async fn updates_repeats_review_group_per_group() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        client(&server)
            .updates(&UpdatesQuery {
                review_groups: &["qam-sle", "", "qam-openqa-review"],
                status: Some("testing"),
                ..Default::default()
            })
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0].url.query(),
            Some("review_group=qam-sle&review_group=qam-openqa-review&status=testing")
        );
    }

    #[tokio::test]
    async fn updates_passes_assignee() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("assignee", "mpluskal"))
            .and(query_param("status", "testing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server)
                .updates(&UpdatesQuery {
                    assignee: Some("mpluskal"),
                    status: Some("testing"),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Some(json!([]))
        );
    }

    #[tokio::test]
    async fn updates_passes_unassigned_flag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("unassigned", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        let _ = client(&server)
            .updates(&UpdatesQuery {
                unassigned: true,
                ..Default::default()
            })
            .await;
    }

    #[tokio::test]
    async fn updates_passes_with_assignment_flag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("with_assignment", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        let _ = client(&server)
            .updates(&UpdatesQuery {
                with_assignment: true,
                ..Default::default()
            })
            .await;
    }

    #[tokio::test]
    async fn updates_passes_no_cache_flag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("no_cache", "1"))
            .and(query_param("assignee", "mpluskal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        let _ = client(&server)
            .updates(&UpdatesQuery {
                assignee: Some("mpluskal"),
                no_cache: true,
                ..Default::default()
            })
            .await;
    }

    #[tokio::test]
    async fn updates_omits_unset_assignment_flags() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        let _ = client(&server).updates(&UpdatesQuery::default()).await;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.query().is_none());
    }

    #[tokio::test]
    async fn updates_encodes_filter_params() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .and(query_param("review_group", "qam sle&x"))
            .and(query_param("status", "testing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        // query_param compares *decoded* values, so a match proves the raw
        // '&'/space were percent-encoded on the wire.
        let _ = client(&server)
            .updates(&UpdatesQuery {
                review_groups: &["qam sle&x"],
                status: Some("testing"),
                ..Default::default()
            })
            .await;
    }

    #[tokio::test]
    async fn updates_no_params_omits_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        let _ = client(&server).updates(&UpdatesQuery::default()).await;
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].url.query().is_none());
    }

    #[tokio::test]
    async fn updates_errs_on_error_status() {
        // `try_get`'s `error_for_status` arm; the transport arm is separate
        // below, since a mock server cannot refuse a connection.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = client(&server)
            .updates(&UpdatesQuery::default())
            .await
            .unwrap_err();
        assert!(matches!(err, TeReGenError::Status { status: 500, .. }));
    }

    /// `try_get`'s `send` arm: both the log line and the `Fetch` string it
    /// bakes must be free of reqwest's URL (#431).
    #[tokio::test]
    async fn updates_errs_on_transport_failure_without_a_url() {
        let mut err = None;
        let logs = capture_logs(|| async {
            err = unreachable_client()
                .updates(&UpdatesQuery::default())
                .await
                .err();
        })
        .await;

        let Some(TeReGenError::Fetch(msg)) = &err else {
            panic!("expected TeReGenError::Fetch, got {err:?}");
        };
        assert!(!msg.contains(" for url ("), "error rendered the URL: {msg}");
        assert!(
            msg.contains("error sending request"),
            "error lost the failure kind: {msg}"
        );
        let line = failure_line(&logs, "TeReGen GET");
        assert!(!line.contains(" for url ("), "log rendered the URL: {line}");
    }

    #[tokio::test]
    async fn updates_ok_none_when_key_absent() {
        // Ok(None), not an error: the caller prints "no updates".
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"other": 1})))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server)
                .updates(&UpdatesQuery::default())
                .await
                .unwrap(),
            None
        );
    }

    // --- regenerate: unreachable vs refused ---

    async fn regenerate_answer(response: ResponseTemplate) -> Option<RegenRequest> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        client(&server).regenerate(RRID, false, false).await
    }

    #[tokio::test]
    async fn regenerate_sends_its_flags_and_reads_the_job() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .and(body_json(
                json!({"force_overwrite": true, "ignore_inconsistent": false}),
            ))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": RRID, "job": 42})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).regenerate(RRID, true, false).await,
            Some(RegenRequest::Enqueued {
                job: Some(json!(42))
            })
        );
    }

    #[tokio::test]
    async fn regenerate_accepts_empty_202() {
        assert_eq!(
            regenerate_answer(ResponseTemplate::new(202).set_body_string("")).await,
            Some(RegenRequest::Enqueued { job: None })
        );
    }

    /// Only a 409 is a refusal, which a flag may lift.
    #[tokio::test]
    async fn regenerate_409_is_a_refusal_with_teregens_reason() {
        assert_eq!(
            regenerate_answer(
                ResponseTemplate::new(409)
                    .set_body_json(json!({"error": "template was hand-edited"}))
            )
            .await,
            Some(RegenRequest::Refused("template was hand-edited".to_owned()))
        );
        assert_eq!(
            regenerate_answer(ResponseTemplate::new(409).set_body_string("<html>")).await,
            Some(RegenRequest::Refused("HTTP 409".to_owned()))
        );
    }

    /// Any other error status is a failed request, never a refusal: no flag
    /// lifts a 500.
    #[tokio::test]
    async fn regenerate_error_statuses_are_failures() {
        assert_eq!(
            regenerate_answer(
                ResponseTemplate::new(500).set_body_json(json!({"error": "database is down"}))
            )
            .await,
            Some(RegenRequest::Failed {
                status: 500,
                error: Some("database is down".to_owned())
            })
        );
        // A validator's body has no `error`; its messages are the reason.
        assert_eq!(
            regenerate_answer(ResponseTemplate::new(400).set_body_json(json!({
                "errors": [{"message": "bad flag", "path": "/x"}, {"message": "worse"}],
                "status": 400
            })))
            .await,
            Some(RegenRequest::Failed {
                status: 400,
                error: Some("bad flag; worse".to_owned())
            })
        );
        assert_eq!(
            regenerate_answer(ResponseTemplate::new(502).set_body_string("oops")).await,
            Some(RegenRequest::Failed {
                status: 502,
                error: None
            })
        );
    }

    #[tokio::test]
    async fn regenerate_and_wait_names_a_failed_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
            .mount(&server)
            .await;
        let outcome = client(&server)
            .regenerate_and_wait(RRID, false, false, || false)
            .await;
        assert_eq!(
            outcome,
            RegenOutcome {
                failed: Some("TeReGen answered 500 Internal Server Error: boom".to_owned()),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn regenerate_none_when_unreachable() {
        // A closed port is the only way to reach the POST `send` arm; every
        // other `regenerate` test drives a status, not a transport failure.
        let mut out = Some(RegenRequest::Refused(String::new()));
        let logs = capture_logs(|| async {
            out = unreachable_client().regenerate(RRID, false, false).await;
        })
        .await;
        assert_eq!(out, None);

        // #431: the line must not carry reqwest's ` for url (…)`.
        let line = failure_line(&logs, "POST regenerate");
        assert!(!line.contains(" for url ("), "log rendered the URL: {line}");
        assert!(
            line.contains("error sending request"),
            "log lost the failure kind: {line}"
        );
    }

    // --- wait_for_template ---

    #[tokio::test]
    async fn wait_for_template_returns_on_finished() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "finished"})),
            )
            .mount(&server)
            .await;
        let status = client(&server)
            .wait_for_template(
                RRID,
                Duration::from_millis(1),
                Duration::from_secs(1),
                || false,
            )
            .await;
        assert_eq!(status, Some(json!({"minion_state": "finished"})));
    }

    #[tokio::test]
    async fn wait_for_template_polls_until_done() {
        let server = MockServer::start().await;
        // up_to_n_times bounds the first stub so the second takes over.
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "running"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "failed"})),
            )
            .mount(&server)
            .await;
        let status = client(&server)
            .wait_for_template(
                RRID,
                Duration::from_millis(1),
                Duration::from_secs(5),
                || false,
            )
            .await;
        assert_eq!(status, Some(json!({"minion_state": "failed"})));
    }

    #[tokio::test]
    async fn wait_for_template_returns_last_on_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "running"})),
            )
            .mount(&server)
            .await;
        // timeout=0: the deadline is already past after the first poll.
        let status = client(&server)
            .wait_for_template(RRID, Duration::from_millis(1), Duration::ZERO, || false)
            .await;
        assert_eq!(status, Some(json!({"minion_state": "running"})));
    }

    #[tokio::test]
    async fn wait_for_template_stops_when_should_stop_true() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "running"})),
            )
            .mount(&server)
            .await;
        let status = client(&server)
            .wait_for_template(
                RRID,
                Duration::from_secs(999),
                Duration::from_secs(999),
                || true,
            )
            .await;
        assert_eq!(status, Some(json!({"minion_state": "running"})));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn wait_for_template_should_stop_sleep_is_interruptible() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "running"})),
            )
            .mount(&server)
            .await;
        // Stay False for the post-poll check, then flip True so the small-step
        // sleep loop exits on its first iteration instead of waiting 10s.
        let mut n = 0;
        let should_stop = move || {
            n += 1;
            n > 1
        };
        let status = client(&server)
            .wait_for_template(
                RRID,
                Duration::from_secs(10),
                Duration::from_secs(999),
                should_stop,
            )
            .await;
        assert_eq!(status, Some(json!({"minion_state": "running"})));
    }

    // --- regenerate_and_wait ---

    #[tokio::test]
    async fn regenerate_and_wait_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": RRID, "job": 5})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"minion_state": "finished"})),
            )
            .mount(&server)
            .await;
        let outcome = client(&server)
            .regenerate_and_wait(RRID, false, false, || false)
            .await;
        assert_eq!(
            outcome,
            RegenOutcome {
                ok: true,
                job: Some(json!(5)),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn regenerate_and_wait_unreachable() {
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        let c = TeReGen::with_client(http, "http://127.0.0.1:1");
        let outcome = c.regenerate_and_wait(RRID, false, false, || false).await;
        assert!(outcome.unreachable);
        assert!(!outcome.ok);
    }

    #[tokio::test]
    async fn regenerate_and_wait_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({"error": "edited"})))
            .mount(&server)
            .await;
        let outcome = client(&server)
            .regenerate_and_wait(RRID, false, false, || false)
            .await;
        assert_eq!(
            outcome,
            RegenOutcome {
                ok: false,
                error: Some("edited".to_string()),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn regenerate_and_wait_unfinished() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": RRID, "job": 8})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/status")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"minion_state": "failed", "minion_error": "kaboom"})),
            )
            .mount(&server)
            .await;
        let outcome = client(&server)
            .regenerate_and_wait(RRID, false, false, || false)
            .await;
        assert_eq!(
            outcome,
            RegenOutcome {
                ok: false,
                state: Some("failed".to_string()),
                minion_error: Some("kaboom".to_string()),
                job: Some(json!(8)),
                ..Default::default()
            }
        );
    }
}
