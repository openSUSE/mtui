//! The shared base for openQA connectors.
//!
//! Builds the job-query parameters from the incident's
//! [`RequestReviewID`] and incident name, fetches jobs from the openQA instance,
//! and folds every transport/HTTP failure into a `None` result so a command
//! never aborts on a flaky openQA. This module provides that shared machinery;
//! the concrete `auto` and `kernel` workflows live in
//! `standard` and [`kernel`](crate::openqa::kernel).

use mtui_types::{RequestKind, RequestReviewID};
use serde::Deserialize;

use crate::error::OpenQAError;
use crate::http::sanitize_url;
use crate::openqa::client::redact;

/// The openQA `distri` query parameter.
///
/// Upstream sources this from `[openqa] openqa_install_distri`. That option is
/// effectively obsolete (unchanged in practice), so it is pinned here rather
/// than adding an `[openqa]` config surface.
pub(crate) const OPENQA_INSTALL_DISTRI: &str = "sle";

/// Provides the incident name used to build the openQA job-query `build`
/// parameter.
///
/// Upstream passes an `incident` metadata object and calls
/// `incident.get_incident_name()`. This trait is the seam: the connectors are
/// built and tested against a mock, and concrete metadata
/// ([`QemIncident`](crate::qem_dashboard::incident::QemIncident)) implements
/// it without a connector refactor.
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
    pub(crate) result: String,
    /// The id of the job this job was cloned as, if any.
    #[serde(default)]
    pub(crate) clone_id: Option<i64>,
    /// The job settings (FLAVOR, ARCH, VERSION, HDD_1, ...).
    #[serde(default)]
    pub(crate) settings: std::collections::BTreeMap<String, String>,
    /// The per-module results.
    #[serde(default)]
    pub(crate) modules: Vec<JobModule>,
}

/// One module within an openQA job.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct JobModule {
    /// The module name.
    #[serde(default)]
    pub(crate) name: String,
    /// The module category.
    #[serde(default)]
    pub(crate) category: String,
    /// The module result (e.g. `passed`, `failed`).
    #[serde(default)]
    pub(crate) result: String,
}

impl Job {
    /// A settings value, or `""` if absent (the connectors always expect these
    /// keys to be present).
    #[must_use]
    pub(crate) fn setting(&self, key: &str) -> &str {
        self.settings.get(key).map_or("", String::as_str)
    }
}

/// The response envelope of `GET /api/v1/jobs`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JobsResponse {
    #[serde(default)]
    pub jobs: Vec<Job>,
}

/// The shared connector state: the ruoqa client plus the resolved query params.
///
/// Computes the `distri`/`scope`/`latest`/`build` parameters once, from the
/// [`RequestReviewID`] and incident name, and holds the [`ruoqa::Client`] used
/// to fetch jobs.
#[derive(Debug, Clone)]
pub struct OpenQABase {
    client: ruoqa::Client,
    params: Vec<(String, String)>,
}

impl OpenQABase {
    /// Build the shared connector state.
    ///
    /// The `build` parameter is
    /// `:{git|smelt}:{maintenance_id}:{incident_name}`, keyed on whether the
    /// request is [`RequestKind::Slfo`] (`git`) or otherwise (`smelt`).
    pub fn new(
        client: ruoqa::Client,
        rrid: &RequestReviewID,
        incident: &impl IncidentName,
    ) -> Self {
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
        Self { client, params }
    }

    /// The openQA instance host (base URL), used in pretty-printed output.
    ///
    /// Sourced from `ruoqa`'s own resolved `base_url`, which already carries
    /// no userinfo: `ruoqa::config::resolve` reduces a URL to `host[:port]`
    /// before parsing it, dropping any `user:pass@` outright rather than
    /// merely redacting it for display.
    #[must_use]
    pub(crate) fn host(&self) -> &str {
        self.client.base_url().as_str()
    }

    /// Fetch jobs from the openQA instance (best-effort).
    ///
    /// Returns `None` on *any* failure — request-build, transport, non-2xx
    /// status, or a malformed body — after logging at `error`/`debug`, so no
    /// URL/transport failure shape ever escapes as a panic.
    /// `Some(vec![])` is possible for a valid-but-empty response.
    ///
    /// Prefer [`try_get_jobs`](Self::try_get_jobs) when the caller needs to tell
    /// a fetch failure apart from a genuinely-empty result.
    pub async fn get_jobs(&self) -> Option<Vec<Job>> {
        self.try_get_jobs().await.ok()
    }

    /// Fetch jobs from the openQA instance, surfacing failures as `Err`.
    ///
    /// The fallible sibling of [`get_jobs`](Self::get_jobs): a transport,
    /// non-2xx, or malformed-body failure returns [`OpenQAError::Fetch`]
    /// (with a URL-free description) instead of being folded to `None`, so a
    /// caller can distinguish "unreachable" from "empty". `Ok(vec![])` is a
    /// valid-but-empty response.
    ///
    /// # Errors
    ///
    /// [`OpenQAError::Fetch`] on any fetch failure.
    pub async fn try_get_jobs(&self) -> Result<Vec<Job>, OpenQAError> {
        let safe_host = sanitize_url(self.host());
        tracing::debug!("Get data from openQA - {safe_host}");

        let path = format!("/api/v1/jobs{}", build_query(&self.params));
        let body: JobsResponse = self
            .client
            .request_as(reqwest::Method::GET, &path, None)
            .await
            .map_err(|e| {
                tracing::error!("openQA request to {safe_host} failed: {}", redact(&e));
                OpenQAError::Fetch(redact(&e))
            })?;
        Ok(body.jobs)
    }
}

/// Renders `params` as a `?key=value&...` query string via `url`'s encoder
/// (through the `reqwest::Url` re-export, so no direct `url` dependency is
/// needed), or an empty string when `params` is empty.
fn build_query(params: &[(String, String)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut url = reqwest::Url::parse("http://x").expect("fixed base URL always parses");
    url.query_pairs_mut()
        .extend_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    format!("?{}", url.query().unwrap_or_default())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A mock incident provider, mirroring the `mock_incident` pytest fixture
    /// whose `get_incident_name` returns `"bash"`.
    pub(crate) struct MockIncident {
        name: String,
    }

    impl MockIncident {
        pub(crate) fn new(name: &str) -> Self {
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
            .params
            .iter()
            .find(|(k, _)| k == "build")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(build, ":smelt:1:bash");
    }

    /// PI takes the `smelt` prefix. This is the single case that separates the
    /// bare `kind == Slfo` test here from `qam`'s precondition guard, which
    /// groups PI *with* SLFO — merging the two would silently retag every PI
    /// job's `build` parameter as `git` and break openQA job lookup.
    #[test]
    fn build_param_uses_smelt_prefix_for_pi() {
        let base = OpenQABase::new(dummy_client(), &rrid("PI"), &MockIncident::new("bash"));
        let build = base
            .params
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
            .params
            .iter()
            .find(|(k, _)| k == "build")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(build, ":git:1.1:bash");
    }

    #[test]
    fn host_drops_url_credentials_entirely() {
        // Unlike the hand-rolled client this replaces, `ruoqa`'s
        // `config::resolve` reduces a URL to `host[:port]` before parsing, so
        // userinfo never survives into `base_url()` at all — not merely
        // redacted for display, genuinely absent.
        use crate::http::VerifyPolicy;
        use crate::openqa::client::build_openqa_client;
        let client = build_openqa_client(
            VerifyPolicy::Default(true),
            "https://alice:s3cret@openqa.example.com",
        )
        .unwrap();
        let base = OpenQABase::new(client, &rrid("Maintenance"), &MockIncident::new("bash"));
        assert!(!base.host().contains("s3cret"));
        assert!(!base.host().contains("alice"));
        assert_eq!(base.host(), "https://openqa.example.com/");
    }

    #[test]
    fn default_params_are_stable() {
        let base = OpenQABase::new(
            dummy_client(),
            &rrid("Maintenance"),
            &MockIncident::new("bash"),
        );
        let get = |k: &str| {
            base.params
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
    pub(crate) fn dummy_client() -> ruoqa::Client {
        use crate::http::VerifyPolicy;
        use crate::openqa::client::build_openqa_client;
        build_openqa_client(VerifyPolicy::Default(true), "https://openqa.example.com").unwrap()
    }
}
