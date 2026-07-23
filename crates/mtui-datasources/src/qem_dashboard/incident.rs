//! QEM Dashboard incident metadata, ported from
//! `mtui/data_sources/qem_dashboard/incident.py`.
//!
//! [`QemIncident`] resolves the dashboard *incident number* from an
//! [`RequestReviewID`] and fetches the incident record via
//! [`QemDashboardClient`]. It is the metadata handle the auto-workflow provider
//! ([`DashboardAutoOpenQA`](super::DashboardAutoOpenQA)) builds on.
//!
//! [`RequestReviewID`]: mtui_types::RequestReviewID

use serde_json::Value;

use mtui_types::{RequestKind, RequestReviewID};

use crate::error::QemDashboardError;
use crate::http::VerifyPolicy;
use crate::openqa::base::IncidentName;

use super::client::QemDashboardClient;

/// Incident metadata from the QEM Dashboard.
///
/// Mirrors upstream `QEMIncident`: on construction it resolves the incident
/// number (SLFO 1.2 requests key on the review id; everything else keys on the
/// maintenance id) and fetches the incident record. A missing/failed fetch
/// leaves [`data`](Self::data) as `None` — the [`is_present`](Self::is_present)
/// predicate mirrors upstream `__bool__`.
#[derive(Debug, Clone)]
pub struct QemIncident {
    /// The request/review id of the incident.
    pub rrid: RequestReviewID,
    /// The resolved dashboard incident number (a maintenance or review id).
    pub incident_number: String,
    /// The shared dashboard client (reused by the auto-workflow provider).
    pub client: QemDashboardClient,
    /// The fetched incident record, or `None` when unavailable.
    pub data: Option<Value>,
}

impl QemIncident {
    /// Build the incident metadata: resolve the number, then fetch the record.
    ///
    /// # Errors
    ///
    /// Returns [`QemDashboardError::Http`] if the shared HTTP client cannot be
    /// built (a fetch failure is *not* an error — it folds into `data = None`,
    /// matching upstream).
    pub async fn new(
        rrid: RequestReviewID,
        apiurl: impl Into<String>,
        policy: VerifyPolicy,
    ) -> Result<Self, QemDashboardError> {
        let client = QemDashboardClient::new(apiurl, policy)?;
        Ok(Self::with_client(rrid, client).await)
    }

    /// Build the incident metadata from an existing client (test/composition
    /// seam), fetching the incident record eagerly as upstream `__init__` does.
    #[must_use = "the fetched incident metadata should be used"]
    pub async fn with_client(rrid: RequestReviewID, client: QemDashboardClient) -> Self {
        let incident_number = Self::incident_number(&rrid);
        let data = client.incident(&incident_number).await;
        Self {
            rrid,
            incident_number,
            client,
            data,
        }
    }

    /// Resolve the dashboard incident number from an [`RequestReviewID`].
    ///
    /// Mirrors upstream `_incident_number`: an SLFO request with a `1.2`
    /// maintenance id keys on the review id; every other request keys on the
    /// maintenance id.
    ///
    /// [`RequestReviewID`]: mtui_types::RequestReviewID
    #[must_use]
    fn incident_number(rrid: &RequestReviewID) -> String {
        if rrid.kind == RequestKind::Slfo && rrid.maintenance_id == "1.2" {
            rrid.review_id.to_string()
        } else {
            rrid.maintenance_id.clone()
        }
    }

    /// Return the shortest package name, for build-query compatibility.
    ///
    /// Mirrors upstream `get_incident_name`: `None` when there is no incident
    /// record or no packages, else the shortest package name (ties broken by the
    /// stable sort, matching Python's `sorted(..., key=len)`).
    #[must_use]
    pub fn get_incident_name(&self) -> Option<String> {
        let packages = self.data.as_ref()?.get("packages")?.as_array()?;
        packages
            .iter()
            .filter_map(Value::as_str)
            .min_by_key(|name| name.len())
            .map(str::to_owned)
    }

    /// Whether the incident record was successfully fetched.
    ///
    /// Mirrors upstream `__bool__`.
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
    /// available (upstream passes the raw `get_incident_name()` value straight
    /// into the build string; an empty name yields the same `:prefix:mid:` shape
    /// the connectors already tolerate).
    fn get_incident_name(&self) -> String {
        Self::get_incident_name(self).unwrap_or_default()
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

    #[test]
    fn incident_number_slfo_other_maintenance_keeps_maintenance_id() {
        let rrid: RequestReviewID = "SUSE:SLFO:1.1:199773".parse().unwrap();
        assert_eq!(QemIncident::incident_number(&rrid), "1.1");
    }

    #[tokio::test]
    async fn metadata_and_shortest_package_name() {
        // Ported from test_qem_incident_metadata.
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
        let incident = QemIncident::with_client(rrid, client_for(&server)).await;

        assert!(incident.is_present());
        assert_eq!(incident.get_incident_name().as_deref(), Some("kernel-ec2"));
    }

    #[tokio::test]
    async fn missing_incident_is_not_present() {
        let server = MockServer::start().await;
        // No mount -> 404 -> data folds to None.
        let rrid: RequestReviewID = "SUSE:Maintenance:12358:199773".parse().unwrap();
        let incident = QemIncident::with_client(rrid, client_for(&server)).await;

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
        let incident = QemIncident::with_client(rrid, client_for(&server)).await;

        assert!(incident.is_present());
        assert_eq!(incident.get_incident_name(), None);
    }
}
