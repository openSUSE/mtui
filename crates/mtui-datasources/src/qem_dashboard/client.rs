//! Low-level read-only HTTP client for the QEM Dashboard API, ported from
//! `mtui/data_sources/qem_dashboard/client.py`.
//!
//! Every endpoint is a thin GET-to-JSON wrapper over the shared
//! [`HttpClient`](crate::http::HttpClient). Mirroring upstream
//! `QEMDashboardClient._get`, any transport, non-2xx, or JSON-parse failure is
//! logged at `debug` and folded into a `None` (for [`incident`](Self::incident))
//! or an empty `Vec` (for the list endpoints), so a fetch failure never escapes
//! the client — the caller sees the same "no data" shape whether the dashboard
//! was unreachable or genuinely empty.

use std::time::Duration;

use serde_json::Value;

use crate::error::QemDashboardError;
use crate::http::{HttpClient, MAX_API_BODY, VerifyPolicy};

/// openQA job result statuses reported individually in the exported log.
///
/// Every other status (`passed`, `softfailed`, …) is collapsed into a per-group
/// summary count to keep the report short and reviewable. Ported verbatim from
/// upstream `FAILED_RESULTS`.
pub const FAILED_RESULTS: [&str; 3] = ["failed", "incomplete", "timeout_exceeded"];

/// Wall-clock cap per future in the parallel fan-out.
///
/// Defence-in-depth on top of the shared per-request HTTP timeout: a stuck
/// worker will not block the whole batch. Ported from upstream
/// `_FUTURE_TIMEOUT` (60s); consumed by
/// [`DashboardAutoOpenQA`](super::DashboardAutoOpenQA).
pub const FUTURE_TIMEOUT: Duration = Duration::from_secs(60);

/// A small read-only client for the QEM Dashboard API.
///
/// Mirrors upstream `QEMDashboardClient`: it pins the resolved TLS-verify policy
/// on a shared [`HttpClient`] and exposes one method per dashboard endpoint.
#[derive(Debug, Clone)]
pub struct QemDashboardClient {
    apiurl: String,
    http: HttpClient,
}

impl QemDashboardClient {
    /// Build a client for `apiurl` with the given TLS-verify `policy`.
    ///
    /// The trailing slash on `apiurl` is stripped, matching upstream
    /// `apiurl.rstrip("/")`, so callers may pass either form.
    ///
    /// # Errors
    ///
    /// Returns [`QemDashboardError::Http`] if the shared HTTP client cannot be
    /// built (e.g. a configured CA bundle cannot be read or parsed).
    pub fn new(apiurl: impl Into<String>, policy: VerifyPolicy) -> Result<Self, QemDashboardError> {
        let http = HttpClient::new(policy)?;
        Ok(Self::with_client(http, apiurl))
    }

    /// Build a client from an already-constructed [`HttpClient`].
    ///
    /// The test/composition seam (mirrors `teregen`/`oqa_search`): callers that
    /// already hold a shared client — or a test that wants to point the client
    /// at a mock server — inject it here without a second TLS build.
    #[must_use]
    pub fn with_client(http: HttpClient, apiurl: impl Into<String>) -> Self {
        Self {
            apiurl: apiurl.into().trim_end_matches('/').to_string(),
            http,
        }
    }

    /// The resolved base API URL (trailing slash stripped).
    #[must_use]
    pub fn apiurl(&self) -> &str {
        &self.apiurl
    }

    /// GET `path` and parse the body as JSON, folding any failure into `None`.
    ///
    /// Mirrors upstream `_get`: a transport error, a non-2xx status, or a
    /// malformed JSON body is logged at `debug` and returns `None`.
    async fn get(&self, path: &str) -> Option<Value> {
        let url = format!("{}/{}", self.apiurl, path.trim_start_matches('/'));
        match self.http.get_bytes_capped(&url, MAX_API_BODY).await {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => Some(value),
                Err(e) => {
                    tracing::debug!("QEM Dashboard returned invalid JSON: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::debug!("QEM Dashboard request failed: {e}");
                None
            }
        }
    }

    /// GET a list endpoint, folding any failure (or a non-array body) into `[]`.
    ///
    /// Mirrors the upstream `_get(...) or []` idiom on the settings/jobs
    /// endpoints, extended to also treat a non-array JSON body as empty (upstream
    /// implicitly relied on the endpoint always returning a list).
    async fn get_list(&self, path: &str) -> Vec<Value> {
        match self.get(path).await {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        }
    }

    /// GET `path` and parse the body as JSON, surfacing failures as `Err`.
    ///
    /// The fallible sibling of [`get`](Self::get): a transport error, a non-2xx
    /// status, or a malformed JSON body returns [`QemDashboardError::Fetch`]
    /// (with a URL-free description) instead of being folded to `None`, so a
    /// caller can distinguish "unreachable" from "empty". A valid-but-`null`
    /// body is `Ok(None)`.
    async fn try_get(&self, path: &str) -> Result<Option<Value>, QemDashboardError> {
        let url = format!("{}/{}", self.apiurl, path.trim_start_matches('/'));
        let bytes = self
            .http
            .get_bytes_capped(&url, MAX_API_BODY)
            .await
            .map_err(|e| QemDashboardError::Fetch(e.to_string()))?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Null) => Ok(None),
            Ok(value) => Ok(Some(value)),
            Err(e) => Err(QemDashboardError::Fetch(format!("invalid JSON: {e}"))),
        }
    }

    /// GET a list endpoint, surfacing failures as `Err`.
    ///
    /// The fallible sibling of [`get_list`](Self::get_list): a fetch failure
    /// returns [`QemDashboardError::Fetch`]. A successful non-array body is
    /// treated as an empty list (matching `get_list`), so only a real fetch
    /// failure — not a shape surprise — is an error.
    async fn try_get_list(&self, path: &str) -> Result<Vec<Value>, QemDashboardError> {
        match self.try_get(path).await? {
            Some(Value::Array(items)) => Ok(items),
            _ => Ok(Vec::new()),
        }
    }

    /// Fetch the incident record for `incident_number`.
    ///
    /// Returns `None` on any failure, mirroring upstream `incident`.
    pub async fn incident(&self, incident_number: &str) -> Option<Value> {
        self.get(&format!("incidents/{incident_number}")).await
    }

    /// Fetch the incident settings list for `incident_number` (empty on failure).
    pub async fn incident_settings(&self, incident_number: &str) -> Vec<Value> {
        self.get_list(&format!("incident_settings/{incident_number}"))
            .await
    }

    /// Fetch the update (aggregate) settings list for `incident_number`
    /// (empty on failure).
    pub async fn update_settings(&self, incident_number: &str) -> Vec<Value> {
        self.get_list(&format!("update_settings/{incident_number}"))
            .await
    }

    /// Fetch the openQA jobs for an incident settings id (empty on failure).
    pub async fn incident_jobs(&self, incident_settings_id: i64) -> Vec<Value> {
        self.get_list(&format!("jobs/incident/{incident_settings_id}"))
            .await
    }

    /// Fetch the openQA jobs for an update settings id (empty on failure).
    pub async fn update_jobs(&self, update_settings_id: i64) -> Vec<Value> {
        self.get_list(&format!("jobs/update/{update_settings_id}"))
            .await
    }

    /// Fallible sibling of [`incident_settings`](Self::incident_settings): a
    /// fetch failure returns [`QemDashboardError::Fetch`] instead of `[]`.
    ///
    /// # Errors
    ///
    /// [`QemDashboardError::Fetch`] on a transport error, non-2xx status, or
    /// malformed JSON body.
    pub async fn try_incident_settings(
        &self,
        incident_number: &str,
    ) -> Result<Vec<Value>, QemDashboardError> {
        self.try_get_list(&format!("incident_settings/{incident_number}"))
            .await
    }

    /// Fallible sibling of [`update_settings`](Self::update_settings): a fetch
    /// failure returns [`QemDashboardError::Fetch`] instead of `[]`.
    ///
    /// # Errors
    ///
    /// [`QemDashboardError::Fetch`] on a transport error, non-2xx status, or
    /// malformed JSON body.
    pub async fn try_update_settings(
        &self,
        incident_number: &str,
    ) -> Result<Vec<Value>, QemDashboardError> {
        self.try_get_list(&format!("update_settings/{incident_number}"))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> QemDashboardClient {
        let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        QemDashboardClient::with_client(http, format!("{}/api", server.uri()))
    }

    #[test]
    fn new_strips_trailing_slash() {
        let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        let client = QemDashboardClient::with_client(http, "https://d/api/");
        assert_eq!(client.apiurl(), "https://d/api");
    }

    #[tokio::test]
    async fn incident_returns_parsed_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["bash"],
            })))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let data = client.incident("12358").await.unwrap();
        assert_eq!(data["number"], 12358);
    }

    #[tokio::test]
    async fn incident_folds_error_to_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/999"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = client_for(&server);
        assert!(client.incident("999").await.is_none());
    }

    #[tokio::test]
    async fn incident_folds_invalid_json_to_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/5"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        assert!(client.incident("5").await.is_none());
    }

    #[tokio::test]
    async fn list_endpoints_fold_error_to_empty() {
        let server = MockServer::start().await;
        // No mounts: every request 404s, so each list endpoint returns [].
        let client = client_for(&server);
        assert!(client.incident_settings("1").await.is_empty());
        assert!(client.update_settings("1").await.is_empty());
        assert!(client.incident_jobs(7).await.is_empty());
        assert!(client.update_jobs(23).await.is_empty());
    }

    #[tokio::test]
    async fn list_endpoint_returns_items() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/12358"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 7}, {"id": 8}])),
            )
            .mount(&server)
            .await;

        let client = client_for(&server);
        let settings = client.incident_settings("12358").await;
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0]["id"], 7);
    }

    #[tokio::test]
    async fn non_array_list_body_is_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"x": 1})))
            .mount(&server)
            .await;

        let client = client_for(&server);
        assert!(client.incident_settings("1").await.is_empty());
    }

    #[tokio::test]
    async fn try_settings_error_status_is_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client.try_incident_settings("1").await.unwrap_err();
        assert!(matches!(err, QemDashboardError::Fetch(_)));
    }

    #[tokio::test]
    async fn try_settings_invalid_json_is_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/update_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client.try_update_settings("1").await.unwrap_err();
        assert!(matches!(err, QemDashboardError::Fetch(_)));
    }

    #[tokio::test]
    async fn try_settings_empty_success_is_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = client_for(&server);
        assert!(client.try_incident_settings("1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn try_settings_non_array_success_is_ok_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"x": 1})))
            .mount(&server)
            .await;

        let client = client_for(&server);
        assert!(client.try_incident_settings("1").await.unwrap().is_empty());
    }
}
