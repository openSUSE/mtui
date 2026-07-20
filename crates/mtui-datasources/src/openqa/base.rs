//! The shared base for openQA connectors, ported from
//! `mtui/data_sources/openqa/base.py`.
//!
//! Upstream's `OpenQA` ABC builds the job-query parameters from the incident's
//! [`RequestReviewID`] and incident name, fetches jobs from the openQA instance,
//! and folds every transport/HTTP failure into a `None` result so a command
//! never aborts on a flaky openQA. This module provides that shared machinery;
//! the concrete `auto` and `kernel` workflows live in
//! [`standard`](crate::openqa::standard) and [`kernel`](crate::openqa::kernel).

use mtui_types::{RequestKind, RequestReviewID};
use serde::Deserialize;

use super::client::OpenQAClient;
use crate::error::OpenQAError;
use crate::http::{MAX_API_BODY, read_body_capped, sanitize_url};

/// Redact any URL userinfo from a displayable error before it reaches a log or
/// an [`OpenQAError::Fetch`] value.
///
/// reqwest's `Error` `Display` appends `" for url ({url})"`. reqwest strips
/// userinfo from the URL it stores, but this is the defensive backstop that
/// honours [`OpenQAError::Fetch`]'s "never the raw URL" contract regardless of
/// the error's source: [`sanitize_url`] scans the whole string and rewrites any
/// `scheme://user:pass@host` substring to `scheme://***@host`, and is a no-op
/// when no userinfo is present.
fn redact(e: &impl std::fmt::Display) -> String {
    sanitize_url(&e.to_string())
}

/// The openQA `distri` query parameter.
///
/// Upstream sources this from `[openqa] openqa_install_distri`. That option is
/// effectively obsolete (unchanged in practice), so it is pinned here rather
/// than adding an `[openqa]` config surface.
pub const OPENQA_INSTALL_DISTRI: &str = "sle";

/// Provides the incident name used to build the openQA job-query `build`
/// parameter.
///
/// Upstream passes an `incident` metadata object and calls
/// `incident.get_incident_name()`. The concrete metadata type lands with the
/// testreport work (Phase 4); this trait is the seam so the connectors can be
/// built and tested now against a mock, and the real metadata can implement it
/// later without a connector refactor.
pub trait IncidentName {
    /// The incident's short name (e.g. the package name `bash`).
    fn get_incident_name(&self) -> String;
}

/// One openQA job as returned by `GET /api/v1/jobs`.
///
/// Only the fields the connectors consume are modelled; unknown fields are
/// ignored. `clone_id` is `None` when the job has not been cloned.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Job {
    /// The job id.
    pub id: i64,
    /// The test/job name (e.g. `qam-incidentinstall`).
    #[serde(default)]
    pub test: String,
    /// The overall job result (e.g. `passed`, `failed`).
    #[serde(default)]
    pub result: String,
    /// The id of the job this job was cloned as, if any.
    #[serde(default)]
    pub clone_id: Option<i64>,
    /// The job settings (FLAVOR, ARCH, VERSION, HDD_1, ...).
    #[serde(default)]
    pub settings: std::collections::BTreeMap<String, String>,
    /// The per-module results.
    #[serde(default)]
    pub modules: Vec<JobModule>,
}

/// One module within an openQA job.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct JobModule {
    /// The module name.
    #[serde(default)]
    pub name: String,
    /// The module category.
    #[serde(default)]
    pub category: String,
    /// The module result (e.g. `passed`, `failed`).
    #[serde(default)]
    pub result: String,
}

impl Job {
    /// A settings value, or `""` if absent (mirrors upstream dict access on
    /// keys the connectors always expect to be present).
    #[must_use]
    pub fn setting(&self, key: &str) -> &str {
        self.settings.get(key).map_or("", String::as_str)
    }
}

/// The response envelope of `GET /api/v1/jobs`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JobsResponse {
    #[serde(default)]
    pub jobs: Vec<Job>,
}

/// The shared connector state: the API client plus the resolved query params.
///
/// Ported from the `OpenQA.__init__` body: it computes the `distri`/`scope`/
/// `latest`/`build` parameters once, from the [`RequestReviewID`] and incident
/// name, and holds the [`OpenQAClient`] used to fetch jobs.
#[derive(Debug, Clone)]
pub struct OpenQABase {
    client: OpenQAClient,
    params: Vec<(String, String)>,
    host: String,
    /// [`sanitize_url`]-redacted `host`, for logs/errors/display so a
    /// credentialed base URL (`scheme://user:pass@host`) never leaks userinfo.
    safe_host: String,
}

impl OpenQABase {
    /// Build the shared connector state.
    ///
    /// Mirrors `OpenQA.__init__`: the `build` parameter is
    /// `:{git|smelt}:{maintenance_id}:{incident_name}`, keyed on whether the
    /// request is [`RequestKind::Slfo`] (`git`) or otherwise (`smelt`).
    pub fn new(client: OpenQAClient, rrid: &RequestReviewID, incident: &impl IncidentName) -> Self {
        let prefix = if rrid.kind == RequestKind::Slfo {
            "git"
        } else {
            "smelt"
        };
        let build = format!(
            ":{prefix}:{}:{}",
            rrid.maintenance_id,
            incident.get_incident_name()
        );
        let params = vec![
            ("distri".to_string(), OPENQA_INSTALL_DISTRI.to_string()),
            ("scope".to_string(), "relevant".to_string()),
            ("latest".to_string(), "1".to_string()),
            ("build".to_string(), build),
        ];
        let host = client.base_url().to_string();
        let safe_host = sanitize_url(&host);
        Self {
            client,
            params,
            host,
            safe_host,
        }
    }

    /// The openQA instance host (base URL), used in pretty-printed output.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The resolved job-query parameters.
    #[must_use]
    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }

    /// Fetch jobs from the openQA instance (best-effort).
    ///
    /// Returns `None` on *any* failure — request-build, transport, non-2xx
    /// status, or a malformed body — after logging at `error`/`debug`, matching
    /// upstream's "no URL/transport failure shape may escape as a traceback"
    /// contract. `Some(vec![])` is possible for a valid-but-empty response.
    ///
    /// Prefer [`try_get_jobs`](Self::try_get_jobs) when the caller needs to tell
    /// a fetch failure apart from a genuinely-empty result.
    pub async fn get_jobs(&self) -> Option<Vec<Job>> {
        self.try_get_jobs().await.ok()
    }

    /// Fetch jobs from the openQA instance, surfacing failures as `Err`.
    ///
    /// The fallible sibling of [`get_jobs`](Self::get_jobs): a request-build,
    /// transport, non-2xx, or malformed-body failure returns
    /// [`OpenQAError::Fetch`] (with a URL-free description) instead of being
    /// folded to `None`, so a caller can distinguish "unreachable" from
    /// "empty". `Ok(vec![])` is a valid-but-empty response.
    ///
    /// # Errors
    ///
    /// [`OpenQAError::Fetch`] on any fetch failure.
    pub async fn try_get_jobs(&self) -> Result<Vec<Job>, OpenQAError> {
        tracing::debug!("Get data from openQA - {}", self.safe_host);

        let param_refs: Vec<(&str, String)> = self
            .params
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();

        let builder = self
            .client
            .build_get("jobs", &param_refs)
            .inspect_err(|e| {
                tracing::error!("openQA request to {} failed: {}", self.safe_host, redact(e));
            })?;

        let response = builder.send().await.map_err(|e| {
            tracing::error!(
                "openQA request to {} failed: {}",
                self.safe_host,
                redact(&e)
            );
            OpenQAError::Fetch(redact(&e))
        })?;

        let response = response.error_for_status().map_err(|e| {
            tracing::debug!("openQA returned an error status: {}", redact(&e));
            OpenQAError::Fetch(redact(&e))
        })?;

        let bytes = read_body_capped(response, MAX_API_BODY)
            .await
            .map_err(|e| {
                tracing::error!(
                    "openQA request to {} failed: {}",
                    self.safe_host,
                    redact(&e)
                );
                OpenQAError::Fetch(redact(&e))
            })?;
        serde_json::from_slice::<JobsResponse>(&bytes)
            .map(|body| body.jobs)
            .map_err(|e| {
                tracing::error!(
                    "openQA request to {} failed: {}",
                    self.safe_host,
                    redact(&e)
                );
                OpenQAError::Fetch(redact(&e))
            })
    }

    /// Borrow the API client (used by connectors that need it directly).
    #[must_use]
    pub fn client(&self) -> &OpenQAClient {
        &self.client
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A mock incident provider, mirroring the `mock_incident` pytest fixture
    /// whose `get_incident_name` returns `"bash"`.
    pub(crate) struct MockIncident {
        pub name: String,
    }

    impl MockIncident {
        pub fn new(name: &str) -> Self {
            Self { name: name.into() }
        }
    }

    impl IncidentName for MockIncident {
        fn get_incident_name(&self) -> String {
            self.name.clone()
        }
    }

    fn rrid(kind: &str) -> RequestReviewID {
        RequestReviewID::parse(&format!("SUSE:{kind}:1:1")).unwrap()
    }

    #[test]
    fn build_param_uses_smelt_prefix_for_maintenance() {
        let base = OpenQABase::new(
            dummy_client(),
            &rrid("Maintenance"),
            &MockIncident::new("bash"),
        );
        let build = base
            .params()
            .iter()
            .find(|(k, _)| k == "build")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(build, ":smelt:1:bash");
    }

    #[test]
    fn build_param_uses_git_prefix_for_slfo() {
        // SLFO maintenance ids are dotted; use one that parses.
        let rrid = RequestReviewID::parse("SUSE:SLFO:1.1:1").unwrap();
        let base = OpenQABase::new(dummy_client(), &rrid, &MockIncident::new("bash"));
        let build = base
            .params()
            .iter()
            .find(|(k, _)| k == "build")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(build, ":git:1.1:bash");
    }

    #[test]
    fn safe_host_redacts_url_credentials() {
        use crate::http::{HttpClient, VerifyPolicy};
        let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        let client = OpenQAClient::new(
            http,
            "https://alice:s3cret@openqa.example.com",
            crate::openqa::client::ApiCredentials::default(),
        );
        let base = OpenQABase::new(client, &rrid("Maintenance"), &MockIncident::new("bash"));
        // The log-facing host never carries userinfo...
        assert!(!base.safe_host.contains("s3cret"));
        assert!(base.safe_host.contains("***"));
        // ...but the functional host() keeps the raw URL for building requests.
        assert!(base.host().contains("s3cret"));
    }

    #[test]
    fn redact_strips_userinfo_from_error_display() {
        // Mirrors reqwest's `... for url (scheme://user:pass@host)` shape.
        let msg =
            "error sending request for url (https://alice:s3cret@openqa.example.com/api/v1/jobs)";
        let out = redact(&msg);
        assert!(!out.contains("s3cret"), "leaked credential: {out}");
        assert!(out.contains("***"), "missing redaction marker: {out}");
    }

    #[test]
    fn default_params_match_upstream() {
        let base = OpenQABase::new(
            dummy_client(),
            &rrid("Maintenance"),
            &MockIncident::new("bash"),
        );
        let get = |k: &str| {
            base.params()
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("distri"), Some("sle".to_string()));
        assert_eq!(get("scope"), Some("relevant".to_string()));
        assert_eq!(get("latest"), Some("1".to_string()));
    }

    /// A client pointed at an unroutable base URL, for unit tests that only
    /// exercise param building (never the network).
    pub(crate) fn dummy_client() -> OpenQAClient {
        use crate::http::{HttpClient, VerifyPolicy};
        let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        OpenQAClient::new(
            http,
            "https://openqa.example.com",
            crate::openqa::client::ApiCredentials::default(),
        )
    }
}
