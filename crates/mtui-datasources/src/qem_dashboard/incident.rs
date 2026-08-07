//! QEM Dashboard incident metadata.
//!
//! [`QemIncident`] resolves the dashboard *incident number* from an
//! [`RequestReviewID`] and fetches the incident record via
//! [`QemDashboardClient`]. It is the metadata handle the auto-workflow provider
//! ([`DashboardAutoOpenQA`](super::DashboardAutoOpenQA)) builds on.
//!
//! [`RequestReviewID`]: mtui_types::RequestReviewID

use serde_json::Value;

use mtui_types::{RequestKind, RequestReviewID, UpdateSource};

use crate::error::QemDashboardError;
use crate::http::VerifyPolicy;
use crate::openqa::base::IncidentName;

use super::client::QemDashboardClient;

/// Package-name suffixes qem-bot demotes when choosing an incident's short
/// name (issue #433, B2; `qem-bot/openqabot/types/submission.py:29-72`,
/// `sort_packages`).
const DEMOTED_ARCH_SUFFIXES: &[&str] = &[
    "-aarch64", "-armv7l", "-armv6l", "-x86_64", "-i586", "-i686",
];

/// Whether qem-bot's `sort_packages` demotes `name` below every
/// non-demoted candidate: an arch-suffixed name, or one containing
/// `-livepatch-`.
fn is_demoted_package_name(name: &str) -> bool {
    name.contains("-livepatch-") || DEMOTED_ARCH_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Incident metadata from the QEM Dashboard.
///
/// On construction it resolves the incident
/// number (SLFO 1.2 requests key on the review id; everything else keys on the
/// maintenance id) and fetches the incident record. A missing/failed fetch
/// leaves [`data`](Self::data) as `None` — the [`is_present`](Self::is_present)
/// predicate reflects that.
#[derive(Debug, Clone)]
pub struct QemIncident {
    /// The request/review id of the incident.
    pub rrid: RequestReviewID,
    /// The resolved dashboard incident number (a maintenance or review id).
    pub incident_number: String,
    /// The report's own [`UpdateSource`], as resolved by the caller at load
    /// time from the template's `gitea_commit_hash` — the fallback the
    /// [`update_source`](Self::update_source) accessor uses when no dashboard
    /// record is available (see
    /// [`source_from_record`](Self::source_from_record)).
    pub source: UpdateSource,
    /// The shared dashboard client (reused by the auto-workflow provider).
    pub client: QemDashboardClient,
    /// The fetched incident record, or `None` when unavailable.
    pub data: Option<Value>,
}

impl QemIncident {
    /// Build the incident metadata: resolve the number, then fetch the record.
    ///
    /// `source` is the report's own [`UpdateSource`] (resolved at load from
    /// the template's `gitea_commit_hash`), used as the fallback when no
    /// dashboard record is available — see
    /// [`update_source`](Self::update_source).
    ///
    /// # Errors
    ///
    /// Returns [`QemDashboardError::Http`] if the shared HTTP client cannot be
    /// built (a fetch failure is *not* an error — it folds into `data = None`).
    pub async fn new(
        rrid: RequestReviewID,
        apiurl: impl Into<String>,
        policy: VerifyPolicy,
        source: UpdateSource,
    ) -> Result<Self, QemDashboardError> {
        let client = QemDashboardClient::new(apiurl, policy)?;
        Ok(Self::with_client(rrid, client, source).await)
    }

    /// Build the incident metadata from an existing client (test/composition
    /// seam), fetching the incident record eagerly on construction.
    ///
    /// Skips the fetch entirely for a Product Increment: PI is connected to
    /// neither qem-dashboard nor openQA (issue #433, F5 — its RRID's
    /// `maintenance_id` slot holds a product-family string, not an id, so
    /// every id-shaped use of it here would be meaningless rather than merely
    /// wrong). `data` is left `None`, identical to today's failed fetch
    /// (`is_present()` false, `get_incident_name()` empty) — the only
    /// observable difference is the removed request and its log line.
    #[must_use = "the fetched incident metadata should be used"]
    pub async fn with_client(
        rrid: RequestReviewID,
        client: QemDashboardClient,
        source: UpdateSource,
    ) -> Self {
        let incident_number = Self::incident_number(&rrid);
        let data = if rrid.kind == RequestKind::Pi {
            None
        } else {
            client.incident(&incident_number).await
        };
        Self {
            rrid,
            incident_number,
            source,
            client,
            data,
        }
    }

    /// Resolve the dashboard incident number from an [`RequestReviewID`].
    ///
    /// Mirrors qem-bot's own dashboard writes (issue #433, F3): a git
    /// submission's dashboard `number` is the Gitea PR number
    /// (`gitea.py::698`), a classic one is the maintenance incident id
    /// (`smeltsync.py::111`) — but for **every** SLFO update, both git- and
    /// OBS-served, that number is the RRID's `review_id` (a Gitea PR number
    /// for git, an OBS review-request number for OBS; the dashboard does not
    /// distinguish them). Every other kind keys on the maintenance id.
    ///
    /// This is a `kind`-only predicate, not an [`UpdateSource`] one: both
    /// SLFO sources key on `review_id`, so the fact the RRID's shape cannot
    /// resolve (issue #433) never enters here. Do not add an `UpdateSource`
    /// parameter — it would be unused and imply a distinction that does not
    /// exist (see the plan's F4).
    ///
    /// [`RequestReviewID`]: mtui_types::RequestReviewID
    /// [`UpdateSource`]: mtui_types::UpdateSource
    #[must_use]
    fn incident_number(rrid: &RequestReviewID) -> String {
        if rrid.kind == RequestKind::Slfo {
            rrid.review_id.to_string()
        } else {
            rrid.maintenance_id.clone()
        }
    }

    /// Return the incident's short name, for build-query compatibility.
    ///
    /// `None` when there is no incident record or no packages, else the name
    /// chosen by qem-bot's own `sort_packages` ordering (issue #433, B2):
    /// arch-suffixed and `-livepatch-` names are demoted *first*, then the
    /// remaining candidates are sorted by length, then alphabetically; the
    /// first survivor wins. Plain shortest-by-length disagrees with this on a
    /// livepatch or arch-split submission, and the two diverging is exactly
    /// how B2 went unnoticed: the `build` query then matches no openQA job.
    #[must_use]
    pub fn get_incident_name(&self) -> Option<String> {
        let packages = self.data.as_ref()?.get("packages")?.as_array()?;
        let mut names: Vec<&str> = packages.iter().filter_map(Value::as_str).collect();
        names.sort_by(|a, b| {
            is_demoted_package_name(a)
                .cmp(&is_demoted_package_name(b))
                .then_with(|| a.len().cmp(&b.len()))
                .then_with(|| a.cmp(b))
        });
        names.first().map(|s| (*s).to_owned())
    }

    /// The dashboard record's own `type` field, resolved to an
    /// [`UpdateSource`], when a record is available.
    ///
    /// `None` only when no incident record was fetched at all (dashboard
    /// down, or PI, which never fetches one — see [`with_client`](Self::with_client));
    /// [`update_source`](IncidentName::update_source) falls back to
    /// [`source`](Self::source) in that case. When a record *is* present, a
    /// missing or blank `type` field resolves to `Obs` (not a fallback),
    /// matching qem-bot's own `default_submission_type = "smelt"`.
    #[must_use]
    pub fn source_from_record(&self) -> Option<UpdateSource> {
        let record = self.data.as_ref()?;
        let raw = record.get("type").and_then(Value::as_str).unwrap_or("");
        Some(UpdateSource::from_qem_type(raw))
    }

    /// Whether the incident record was successfully fetched.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.data.is_some()
    }
}

impl IncidentName for QemIncident {
    /// The incident's short name for openQA build queries.
    ///
    /// Delegates to the inherent [`get_incident_name`](Self::get_incident_name),
    /// falling back to an empty string when no incident record / package is
    /// available; an empty name yields the same `:prefix:mid:` shape
    /// the connectors already tolerate.
    fn get_incident_name(&self) -> String {
        Self::get_incident_name(self).unwrap_or_default()
    }

    /// The resolved dashboard incident number (the openQA `build` middle
    /// component — B3: qem-bot writes the dashboard `number`, not the RRID's
    /// maintenance id).
    fn incident_number(&self) -> String {
        self.incident_number.clone()
    }

    /// The dashboard's own answer when a record is available
    /// ([`source_from_record`](Self::source_from_record)); otherwise the
    /// template fact ([`source`](Self::source)) — see [`UpdateSource`] for
    /// the precedence rule this implements.
    fn update_source(&self) -> UpdateSource {
        self.source_from_record().unwrap_or(self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> QemDashboardClient {
        let http = HttpClient::new(VerifyPolicy::Default(true)).unwrap();
        QemDashboardClient::with_client(http, format!("{}/api", server.uri()))
    }

    #[test]
    fn incident_number_keys_on_maintenance_id_by_default() {
        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&rrid), "12358");
    }

    #[test]
    fn incident_number_uses_review_id_for_slfo_1_2() {
        let rrid: RequestReviewID = "SUSE:SLFO:1.2:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&rrid), "199773");
    }

    /// B1: an OBS-served `SLFO:1.1` request also keys on the review id, not
    /// the maintenance id — the predicate is `kind == Slfo` alone (issue
    /// #433, F4: both git and OBS SLFO key on `review_id`). Before this fix
    /// `incident_number("SUSE:SLFO:1.1:418286")` returned `"1.1"`, which made
    /// mtui GET `/api/incidents/1.1` (not an incident number) for every
    /// OBS-served `1.1` update. Must be observed red against the unfixed
    /// `maintenance_id == "1.2"` guard.
    #[test]
    fn incident_number_uses_review_id_for_slfo_1_1() {
        let rrid: RequestReviewID = "SUSE:SLFO:1.1:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&rrid), "199773");
    }

    /// A hypothetical future SLFO maintenance id — there is no SLFO 2.0
    /// product, and none is expected soon — still keys on the review id: the
    /// predicate is open on `kind`, not closed on a maintenance-id literal.
    #[test]
    fn incident_number_uses_review_id_for_any_slfo_maintenance_id() {
        let rrid: RequestReviewID = "SUSE:SLFO:2.0:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&rrid), "199773");
    }

    /// A PI or Maintenance request that happens to carry a SLFO-shaped
    /// maintenance id (`1.2`) still keys on the maintenance id: the guard is
    /// `kind == Slfo`, not a maintenance-id literal match. Pins the `kind`
    /// half of the predicate, which a "simplification" down to
    /// `maintenance_id == "1.2"` would drop while every other test stayed
    /// green.
    #[test]
    fn incident_number_ignores_maintenance_id_1_2_on_non_slfo_kinds() {
        let pi: RequestReviewID = "SUSE:PI:1.2:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&pi), "1.2");
        let maint: RequestReviewID = "SUSE:Maintenance:1.2:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&maint), "1.2");
    }

    #[tokio::test]
    async fn metadata_and_shortest_package_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["kernel-default", "kernel-ec2"],
                "channels": ["SUSE:SLE-12-SP2:Update"],
            })))
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;

        assert!(incident.is_present());
        assert_eq!(incident.get_incident_name().as_deref(), Some("kernel-ec2"));
    }

    #[tokio::test]
    async fn missing_incident_is_not_present() {
        let server = MockServer::start().await;
        // No mount -> 404 -> data folds to None.
        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;

        assert!(!incident.is_present());
        assert_eq!(incident.get_incident_name(), None);
    }

    #[tokio::test]
    async fn no_packages_yields_no_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"number": 12358, "packages": []})),
            )
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;

        assert!(incident.is_present());
        assert_eq!(incident.get_incident_name(), None);
    }

    /// B2: a `-livepatch-` name is demoted below a plain name **even when it
    /// is shorter** — plain shortest-by-length would pick the (wrong)
    /// livepatch name here (`a-livepatch-b` is 13 chars, `kernel-default` is
    /// 14), so this case is the one that actually discriminates the two
    /// orderings, unlike a case where the correct answer also happens to be
    /// globally shortest.
    #[tokio::test]
    async fn get_incident_name_demotes_livepatch_packages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["a-livepatch-b", "kernel-default"],
            })))
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;
        assert_eq!(
            incident.get_incident_name().as_deref(),
            Some("kernel-default")
        );
    }

    /// B2: an arch-suffixed name is demoted below a plain name, again picking
    /// a case where the demoted candidate (`ab-x86_64`, 9 chars) is shorter
    /// than the correct answer (`kernel-default`, 14 chars).
    #[tokio::test]
    async fn get_incident_name_demotes_arch_suffixed_packages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["ab-x86_64", "kernel-default"],
            })))
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;
        assert_eq!(
            incident.get_incident_name().as_deref(),
            Some("kernel-default")
        );
    }

    /// B2: among non-demoted candidates of equal length, the alphabetically
    /// first wins.
    #[tokio::test]
    async fn get_incident_name_breaks_length_ties_alphabetically() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["cat", "bat"],
            })))
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;
        assert_eq!(incident.get_incident_name().as_deref(), Some("bat"));
    }

    /// Site 5: the dashboard record's own `type` wins when a record is
    /// present.
    #[tokio::test]
    async fn update_source_prefers_the_dashboard_records_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["bash"],
                "type": "git",
            })))
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        // The template fact disagrees (Obs); the dashboard record still wins.
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Obs).await;
        assert_eq!(incident.update_source(), UpdateSource::Git);
    }

    /// A record with a missing/blank `type` resolves `Obs` directly — not a
    /// fallback to the template fact, even though one is available and
    /// disagrees.
    #[tokio::test]
    async fn update_source_blank_type_on_a_present_record_is_obs_not_a_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["bash"],
            })))
            .mount(&server)
            .await;

        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Git).await;
        assert_eq!(incident.update_source(), UpdateSource::Obs);
    }

    /// When no record was fetched at all (dashboard down, or PI), the
    /// template fact is the fallback.
    #[tokio::test]
    async fn update_source_falls_back_to_template_fact_when_record_absent() {
        let server = MockServer::start().await;
        // No mount -> 404 -> data folds to None.
        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server), UpdateSource::Git).await;
        assert!(!incident.is_present());
        assert_eq!(incident.update_source(), UpdateSource::Git);
    }

    /// F5: a PI request is on neither service, so `with_client` skips the
    /// dashboard fetch entirely — zero requests, even though a mock is
    /// mounted and would happily answer. A `SUSE:Maintenance` RRID against
    /// the *same* mock still fetches, so this test can tell "PI skipped"
    /// apart from "mock never mounted".
    #[tokio::test]
    async fn pi_skips_the_dashboard_fetch_entirely() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incidents/16.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 16,
                "packages": ["patch"],
            })))
            .mount(&server)
            .await;

        let pi: RequestReviewID = "SUSE:PI:16.0:199773".parse().unwrap();
        let incident = QemIncident::with_client(pi, client_for(&server), UpdateSource::Obs).await;

        assert!(!incident.is_present());
        assert_eq!(incident.get_incident_name(), None);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "PI must issue zero dashboard requests"
        );

        // The same mock, a Maintenance RRID: proves the mount works and the
        // PI skip above was a real skip, not an unmounted endpoint.
        Mock::given(method("GET"))
            .and(path("/api/incidents/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 12358,
                "packages": ["kernel-default"],
            })))
            .mount(&server)
            .await;
        let maint: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident =
            QemIncident::with_client(maint, client_for(&server), UpdateSource::Obs).await;
        assert!(incident.is_present());
        assert!(!server.received_requests().await.unwrap().is_empty());
    }
}
