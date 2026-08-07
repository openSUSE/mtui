//! The shared base for openQA connectors.
//!
//! Builds the job-query parameters from the incident's [`IncidentName`]
//! facts, fetches jobs from the openQA instance, and folds every
//! transport/HTTP failure into a `None` result so a command never aborts on a
//! flaky openQA. This module provides that shared machinery; the concrete
//! `auto` and `kernel` workflows live in `standard` and
//! [`kernel`](crate::openqa::kernel).

use mtui_types::UpdateSource;
use serde::Deserialize;

use crate::error::OpenQAError;
use crate::http::sanitize_url;
use crate::openqa::client::describe;

/// The openQA `distri` query parameter.
///
/// Upstream sources this from `[openqa] openqa_install_distri`. That option is
/// effectively obsolete (unchanged in practice), so it is pinned here rather
/// than adding an `[openqa]` config surface.
pub(crate) const OPENQA_INSTALL_DISTRI: &str = "sle";

/// Provides the incident facts used to build the openQA job-query `build`
/// parameter (`:{type}:{number}:{package}`, mirroring qem-bot's own
/// `types/submissions.py`).
///
/// This trait is the seam: the connectors are built and tested against a
/// mock, and concrete metadata
/// ([`QemIncident`](crate::qem_dashboard::incident::QemIncident)) implements
/// it without a connector refactor.
///
/// No default bodies: a default would let an implementor silently skip a
/// component, which is exactly how the `build` string's middle component
/// (issue #433, B3) went unnoticed — it should have come from the incident,
/// not the RRID, and nothing forced every implementor to say so.
pub trait IncidentName {
    /// The incident's short name (e.g. the package name `bash`), chosen by
    /// qem-bot's `sort_packages` ordering.
    fn get_incident_name(&self) -> String;

    /// The dashboard incident number (the `build` string's middle
    /// component) — qem-bot's own `sub.id`, not the RRID's maintenance id.
    fn incident_number(&self) -> String;

    /// Which workflow this incident's update belongs to (the `build`
    /// string's prefix): the dashboard record's own `type` when available,
    /// otherwise the report's own [`UpdateSource`] as resolved at load.
    fn update_source(&self) -> UpdateSource;
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
    /// The overall job result.
    #[serde(default = "default_job_result")]
    pub(crate) result: ruoqa::consts::JobResult,
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

/// The default for [`Job::result`] when the field is missing from the
/// response: `#[serde(default)]` needs a fallible-free `Default`, which the
/// foreign, `#[non_exhaustive]` [`ruoqa::consts::JobResult`] doesn't derive
/// (and this crate can't add one under orphan rules), so this names the
/// fallback explicitly instead.
///
/// Deliberately [`Unknown`](ruoqa::consts::JobResult::Unknown), not
/// [`None`](ruoqa::consts::JobResult::None): openQA itself sends the string
/// `"none"` for a genuinely pending job (that value deserializes straight
/// into `JobResult::None`, no default involved), so this fallback is reserved
/// for the field being *absent* from a malformed/truncated response — a
/// distinct "we don't know" case this crate has no evidence to call `None`.
fn default_job_result() -> ruoqa::consts::JobResult {
    ruoqa::consts::JobResult::Unknown
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
/// incident's [`IncidentName`] facts, and holds the [`ruoqa::Client`] used to
/// fetch jobs.
#[derive(Debug, Clone)]
pub struct OpenQABase {
    client: ruoqa::Client,
    params: Vec<(String, String)>,
}

impl OpenQABase {
    /// Build the shared connector state.
    ///
    /// The `build` parameter mirrors qem-bot's own
    /// `:{sub.type}:{sub.id}:{sub.packages[0]}`
    /// (`qem-bot/openqabot/types/submissions.py:236`, issue #433):
    /// [`IncidentName::update_source`] for the prefix (`git`/`smelt`),
    /// [`IncidentName::incident_number`] for the middle component (the
    /// dashboard's own number — **not** the RRID's maintenance id, which
    /// coincides for `SUSE:Maintenance` but never matched an SLFO job; B3),
    /// and [`IncidentName::get_incident_name`] for the package name. Every
    /// component now comes from `incident`, so there is no `rrid` parameter
    /// to leave unused.
    pub fn new(client: ruoqa::Client, incident: &impl IncidentName) -> Self {
        let prefix = incident.update_source().as_qem_type();
        let build = format!(
            ":{prefix}:{}:{}",
            incident.incident_number(),
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
                tracing::error!("openQA request to {safe_host} failed: {}", describe(&e));
                OpenQAError::Fetch(describe(&e))
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

    /// A mock incident provider exposing all three [`IncidentName`] facts
    /// directly, mirroring the `mock_incident` pytest fixture. Defaults to
    /// incident number `"1"` and [`UpdateSource::Obs`]; `with_number`/
    /// `with_source` override either for a specific test.
    pub(crate) struct MockIncident {
        name: String,
        number: String,
        source: UpdateSource,
    }

    impl MockIncident {
        pub(crate) fn new(name: &str) -> Self {
            Self {
                name: name.into(),
                number: "1".to_owned(),
                source: UpdateSource::Obs,
            }
        }

        pub(crate) fn with_number(mut self, number: &str) -> Self {
            self.number = number.to_owned();
            self
        }

        pub(crate) fn with_source(mut self, source: UpdateSource) -> Self {
            self.source = source;
            self
        }
    }

    impl IncidentName for MockIncident {
        fn get_incident_name(&self) -> String {
            self.name.clone()
        }

        fn incident_number(&self) -> String {
            self.number.clone()
        }

        fn update_source(&self) -> UpdateSource {
            self.source
        }
    }

    fn build_param(base: &OpenQABase) -> &str {
        base.params
            .iter()
            .find(|(k, _)| k == "build")
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[test]
    fn build_param_uses_smelt_prefix_for_obs_source() {
        let base = OpenQABase::new(dummy_client(), &MockIncident::new("bash"));
        assert_eq!(build_param(&base), ":smelt:1:bash");
    }

    #[test]
    fn build_param_uses_git_prefix_for_git_source() {
        let incident = MockIncident::new("bash").with_source(UpdateSource::Git);
        let base = OpenQABase::new(dummy_client(), &incident);
        assert_eq!(build_param(&base), ":git:1:bash");
    }

    /// B3: the middle component is the incident's own dashboard number, not
    /// a maintenance id. For SLFO those differ (a dashboard `4413` vs a
    /// maintenance id `1.2`) — this is the field `OpenQABase::new` must use,
    /// unconditionally, regardless of any RRID.
    #[test]
    fn build_param_uses_incident_number_not_a_maintenance_id() {
        let incident = MockIncident::new("bash").with_number("4413");
        let base = OpenQABase::new(dummy_client(), &incident);
        assert_eq!(build_param(&base), ":smelt:4413:bash");
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
        let base = OpenQABase::new(client, &MockIncident::new("bash"));
        assert!(!base.host().contains("s3cret"));
        assert!(!base.host().contains("alice"));
        assert_eq!(base.host(), "https://openqa.example.com/");
    }

    #[test]
    fn default_params_are_stable() {
        let base = OpenQABase::new(dummy_client(), &MockIncident::new("bash"));
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
