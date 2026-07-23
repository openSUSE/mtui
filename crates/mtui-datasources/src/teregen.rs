//! A read-only-plus-one-write client for the TeReGen Report API
//! (`qam.suse.de/api/v1`), ported from `mtui/data_sources/teregen.py`.
//!
//! TeReGen serves the generated test-report data over HTTP: the decoded
//! `metadata.json` plus template status. mtui prefers it as the **source of
//! truth** for report metadata (priority, deadline, review groups,
//! product-composer routing, …), falling back to the locally checked-out
//! `metadata.json` when the API is unreachable or doesn't carry a field.
//!
//! Most reads are best-effort: any failure returns `None` so a TeReGen hiccup
//! never breaks the surrounding command. The exception is
//! [`updates`](TeReGen::updates), which returns a `Result` so its caller can tell
//! a genuinely-empty queue apart from an unreachable TeReGen. The base URL comes
//! from the
//! `[teregen] api` option (upstream default `https://qam.suse.de/api/v1`);
//! wiring that config field is deferred to a later phase, so [`TeReGen::new`]
//! takes the base URL explicitly for now (mirroring the [`Gitea`](crate::gitea)
//! client's constructor).
//!
//! The one documented exception is [`regenerate`](TeReGen::regenerate) (a
//! write): it returns `None` only when TeReGen is *unreachable*, and
//! `{"error": …}` when the server *refuses*, so callers can tell the two apart.
//!
//! ## Deviation from upstream
//!
//! Upstream's [`wait_for_template`](TeReGen::wait_for_template) uses a blocking
//! `threading.Event`-based interruptible sleep. Here it is `async` and the
//! inter-poll wait uses [`tokio::time::sleep`], polling `should_stop` in small
//! steps so cancellation takes effect promptly rather than after a full
//! interval — behaviorally equivalent to upstream.

use std::time::Duration;

use mtui_config::Config;
use serde_json::{Value, json};

use crate::error::TeReGenError;
use crate::http::{
    HTTP_TIMEOUT, HttpClient, MAX_API_BODY, VerifyPolicy, read_body_capped, resolve_verify,
};

/// The result of a regenerate-and-wait attempt (see
/// [`TeReGen::regenerate_and_wait`]).
///
/// Exactly one of the flags is meaningful at a time; `ok` is the only success.
/// The fields carry just enough for a caller to message the user and decide
/// whether to reload:
///
/// - `ok`: the job finished and the template is built.
/// - `unreachable`: TeReGen could not be asked at all.
/// - `error`: the server refused the request (e.g. the template was edited).
/// - `state` / `minion_error`: set when the job ran but did not finish (timed
///   out, was cancelled, or failed).
/// - `job`: the enqueued job id, for logging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegenOutcome {
    /// The job finished and the template is built.
    pub ok: bool,
    /// TeReGen could not be asked at all.
    pub unreachable: bool,
    /// The server refused the request (message from its `{"error": …}` body).
    pub error: Option<String>,
    /// The final Minion state when the job ran but did not finish.
    pub state: Option<String>,
    /// The Minion error message, when present.
    pub minion_error: Option<String>,
    /// The enqueued job id, for logging.
    pub job: Option<Value>,
}

/// Best-effort read-only (plus one write) TeReGen Report API client.
#[derive(Debug, Clone)]
pub struct TeReGen {
    base: String,
    http: HttpClient,
}

impl TeReGen {
    /// Build a client targeting `apiurl`, deriving the TLS posture from
    /// `config.ssl_verify`.
    ///
    /// Mirrors upstream `TeReGen.__init__`, except the base URL is passed
    /// explicitly (the `[teregen] api` config field is deferred to a later
    /// phase) rather than read from `config.teregen_api`.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`](crate::error::HttpError) if the shared HTTP client
    /// cannot be built (e.g. a configured CA bundle cannot be read).
    pub fn new(config: &Config, apiurl: &str) -> crate::Result<Self> {
        let verify: VerifyPolicy = resolve_verify(
            VerifyPolicy::Default(true),
            Some(VerifyPolicy::from_config(&config.ssl_verify)),
        );
        let http = HttpClient::new(verify)?;
        Ok(Self::with_client(http, apiurl))
    }

    /// Build a client from an already-constructed [`HttpClient`], bypassing
    /// [`Config`].
    ///
    /// The composition-root / test seam: it lets a caller inject a client whose
    /// TLS posture (or base host, under `wiremock`) is already fixed.
    #[must_use]
    pub fn with_client(http: HttpClient, apiurl: &str) -> Self {
        Self {
            base: apiurl.trim_end_matches('/').to_string(),
            http,
        }
    }

    /// GET `path` and return the decoded JSON body, or `None` on any transport
    /// failure, non-2xx status, or invalid JSON.
    ///
    /// Mirrors upstream `_get`: best-effort, so callers never see an error.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Option<Value> {
        let mut url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        // Encode the query ourselves: reqwest's `.query()` needs the `query`
        // feature, which pulls in default features this workspace disables (see
        // `openqa::client`).
        let qs = build_query_string(query);
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs);
        }
        let request = self.http.inner().get(&url).timeout(HTTP_TIMEOUT.1);
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("TeReGen GET {path} failed: {e}");
                return None;
            }
        };
        let response = match response.error_for_status() {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("TeReGen GET {path} failed: {e}");
                return None;
            }
        };
        let bytes = match read_body_capped(response, MAX_API_BODY).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!("TeReGen GET {path} failed: {e}");
                return None;
            }
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("TeReGen GET {path} returned invalid JSON: {e}");
                None
            }
        }
    }

    /// GET `path`, surfacing failures as `Err`.
    ///
    /// The fallible sibling of [`get`](Self::get): a transport failure, a
    /// non-2xx status, or invalid JSON returns [`TeReGenError::Fetch`] (with a
    /// URL-free description) instead of being folded to `None`, so a caller can
    /// distinguish "unreachable" from a genuinely-empty successful response.
    async fn try_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, TeReGenError> {
        let mut url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let qs = build_query_string(query);
        if !qs.is_empty() {
            url.push('?');
            url.push_str(&qs);
        }
        let request = self.http.inner().get(&url).timeout(HTTP_TIMEOUT.1);
        let response = request.send().await.map_err(|e| {
            tracing::debug!("TeReGen GET {path} failed: {e}");
            TeReGenError::Fetch(e.to_string())
        })?;
        let response = response.error_for_status().map_err(|e| {
            tracing::debug!("TeReGen GET {path} failed: {e}");
            TeReGenError::Fetch(e.to_string())
        })?;
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

    /// Live checker (build-check) result runs for a report, or `None`.
    ///
    /// Unwraps the `checkers` key of the response object.
    pub async fn checkers(&self, rrid: &str) -> Option<Value> {
        let d = self.get(&format!("reports/{rrid}/checkers"), &[]).await?;
        d.get("checkers").cloned()
    }

    /// The unreleased update queue (live from SMELT).
    ///
    /// Optional `review_group` / `status` narrow the queue server-side.
    ///
    /// Assignment exposure (each maps to a query param of the same name):
    ///
    /// - `assignee`: keep only updates assigned to that user (any qam group);
    ///   implies server-side `status=testing`.
    /// - `unassigned`: keep only updates with no assignee; implies
    ///   `status=testing`.
    /// - `with_assignment`: include assignment on every row without filtering;
    ///   implies `status=testing`.
    /// - `no_cache`: bypass the server's short assignment cache (use for the
    ///   pickup moment).
    ///
    /// Empty string filters and unset flags are omitted from the query.
    ///
    /// # Errors
    ///
    /// Returns [`TeReGenError::Fetch`] on any transport/status/JSON failure, so
    /// the caller can tell an unreachable TeReGen apart from an empty queue.
    /// `Ok(None)` is a successful response whose body carried no `updates` key;
    /// `Ok(Some(v))` is the (possibly-empty) queue value.
    pub async fn updates(&self, opts: &UpdatesQuery<'_>) -> Result<Option<Value>, TeReGenError> {
        let mut params: Vec<(&str, String)> = Vec::new();
        for (name, value) in [
            ("review_group", opts.review_group),
            ("status", opts.status),
            ("assignee", opts.assignee),
        ] {
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
    /// Returns the decoded JSON body: `{"id", "job"}` on success (HTTP 202) or
    /// `{"error": …}` when the server refuses (HTTP 409, e.g. the template
    /// already exists or was hand-edited). Returns `None` only when TeReGen is
    /// unreachable — so callers can tell "refused" apart from "couldn't ask".
    pub async fn regenerate(
        &self,
        rrid: &str,
        force_overwrite: bool,
        ignore_inconsistent: bool,
    ) -> Option<Value> {
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
                tracing::debug!("TeReGen POST regenerate {rrid} failed: {e}");
                return None;
            }
        };
        let status = response.status();
        let body = read_body_capped(response, MAX_API_BODY)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if let Some(Value::Object(_)) = &body {
            return body;
        }
        // A 202 with no/!JSON body still means "enqueued"; anything else is an
        // error the caller should surface.
        if status.as_u16() == 202 {
            Some(json!({}))
        } else {
            Some(json!({ "error": format!("HTTP {}", status.as_u16()) }))
        }
    }

    /// Poll [`status`](Self::status) until the latest generate job finishes or
    /// fails, or `timeout` elapses / `should_stop` returns `true`.
    ///
    /// Polls every `interval`, returning the final status dict once
    /// `minion_state` is `finished` or `failed`, or the last-seen status (or
    /// `None`) on timeout. The caller inspects `minion_state` / `minion_error`
    /// to decide success.
    ///
    /// `should_stop` makes the wait interruptible: it is polled before each
    /// sleep and the inter-poll sleep itself is cancellable in small steps, so a
    /// caller can abandon the wait promptly and get back the last seen status.
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
            // Interruptible sleep: poll `should_stop` in small steps so we
            // re-check and exit promptly instead of waiting out the full
            // interval.
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
    /// Bundles [`regenerate`](Self::regenerate) +
    /// [`wait_for_template`](Self::wait_for_template) into the single protocol
    /// both the `regenerate` command and the stale-template loader share,
    /// returning a [`RegenOutcome`] the caller maps to its own messaging and
    /// reload strategy. `should_stop` is forwarded so the wait stays
    /// interruptible.
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
        let result = self
            .regenerate(rrid, force_overwrite, ignore_inconsistent)
            .await;
        let Some(result) = result else {
            return RegenOutcome {
                ok: false,
                unreachable: true,
                ..Default::default()
            };
        };
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            return RegenOutcome {
                ok: false,
                error: Some(error.to_string()),
                ..Default::default()
            };
        }

        let job = result.get("job").cloned();
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

/// Build the percent-encoded `key=value&...` query string.
///
/// Uses `application/x-www-form-urlencoded` encoding (space → `+`), matching
/// reqwest's `.query()` wire form, which this workspace's minimal reqwest build
/// does not expose.
fn build_query_string(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                urlencoding::encode(k),
                urlencoding::encode(v).replace("%20", "+")
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Default poll interval for [`TeReGen::wait_for_template`], mirroring upstream
/// `interval=5.0`.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// Default overall wait for [`TeReGen::wait_for_template`], mirroring upstream
/// `timeout=600.0`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Optional filters for [`TeReGen::updates`].
///
/// Mirrors the keyword arguments of upstream `TeReGen.updates`. All fields
/// default to unset; empty string filters and `false` flags are omitted from
/// the request query.
#[derive(Debug, Clone, Default)]
pub struct UpdatesQuery<'a> {
    /// Narrow the queue to a single review group (server-side).
    pub review_group: Option<&'a str>,
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
    use super::*;
    use crate::http::VerifyPolicy;
    use wiremock::matchers::{body_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const RRID: &str = "SUSE:SLFO:1.2:5702";

    fn client(server: &MockServer) -> TeReGen {
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        TeReGen::with_client(http, &server.uri())
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

    #[tokio::test]
    async fn checkers_unwraps_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/checkers")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"checkers": [{"name": "rpmlint"}]})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).checkers(RRID).await,
            Some(json!([{"name": "rpmlint"}]))
        );
    }

    #[tokio::test]
    async fn checkers_none_on_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/reports/{RRID}/checkers")))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        assert_eq!(client(&server).checkers(RRID).await, None);
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
                    review_group: Some("qam-sle"),
                    status: Some("testing"),
                    ..Default::default()
                })
                .await
                .unwrap(),
            Some(json!([]))
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
        // No query params expected at all: match a request with an empty query.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/updates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"updates": []})))
            .mount(&server)
            .await;
        let _ = client(&server).updates(&UpdatesQuery::default()).await;
        // Assert the recorded request carried no query string.
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
        // wiremock's query_param matcher compares decoded values, so a match
        // proves the raw '&'/space were percent-encoded on the wire.
        let _ = client(&server)
            .updates(&UpdatesQuery {
                review_group: Some("qam sle&x"),
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
    async fn updates_errs_on_transport_failure() {
        // A non-2xx status surfaces as Err, distinct from an empty queue.
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
        assert!(matches!(err, TeReGenError::Fetch(_)));
    }

    #[tokio::test]
    async fn updates_ok_none_when_key_absent() {
        // A successful response missing the `updates` key is Ok(None), not an
        // error — so the caller can print "no updates" rather than a failure.
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

    #[tokio::test]
    async fn regenerate_returns_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .and(body_json(
                json!({"force_overwrite": true, "ignore_inconsistent": false}),
            ))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": RRID, "job": 42})))
            .mount(&server)
            .await;
        let result = client(&server).regenerate(RRID, true, false).await;
        assert_eq!(result, Some(json!({"id": RRID, "job": 42})));
    }

    #[tokio::test]
    async fn regenerate_surfaces_refusal_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(json!({"error": "template was hand-edited"})),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).regenerate(RRID, false, false).await,
            Some(json!({"error": "template was hand-edited"}))
        );
    }

    #[tokio::test]
    async fn regenerate_accepts_empty_202() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(ResponseTemplate::new(202).set_body_string(""))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).regenerate(RRID, false, false).await,
            Some(json!({}))
        );
    }

    #[tokio::test]
    async fn regenerate_non_json_error_maps_to_http_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/reports/{RRID}/regenerate")))
            .respond_with(ResponseTemplate::new(400).set_body_string("oops"))
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).regenerate(RRID, false, false).await,
            Some(json!({"error": "HTTP 400"}))
        );
    }

    #[tokio::test]
    async fn regenerate_none_when_unreachable() {
        // No server: point the client at a closed port so the POST fails to
        // connect, mirroring upstream's ConnectionError -> None.
        let http = HttpClient::new(VerifyPolicy::Default(false)).unwrap();
        let c = TeReGen::with_client(http, "http://127.0.0.1:1");
        assert_eq!(c.regenerate(RRID, false, false).await, None);
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
        // First poll -> running, second -> failed. up_to_n_times bounds the
        // first stub so the second takes over.
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
        // timeout=0 -> the deadline is already reached after the first poll, so
        // the last seen ("running") status is returned without sleeping.
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
