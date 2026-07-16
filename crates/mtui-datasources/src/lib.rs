//! `mtui-datasources` — shared HTTP client, refhosts, openQA/QEM/Gitea/osc-qam.
//!
//! Every outbound integration lives here; consumers (commands, MCP) get typed
//! clients. The first landed surface is the shared HTTP policy layer
//! ([`mod@http`]) ported from upstream `mtui/support/http.py`: one client with a
//! unified timeout and TLS-verify posture that every later Phase-3 client
//! builds on.

pub mod error;
pub mod gitea;
pub mod http;
pub mod obs;
pub mod openqa;
pub mod oqa_search;
pub mod qem_dashboard;
pub mod refhost;
pub mod teregen;

pub use error::{
    GiteaError, HttpError, OpenQAError, OqaSearchError, QemDashboardError, RefhostError, Result,
};
pub use gitea::{Comment, DEFAULT_GROUP, Gitea, assign_marker, pr_api_url, unassign_marker};
pub use http::{
    HTTP_TIMEOUT, HttpClient, VerifyPolicy, default_pool_size, disable_insecure_warnings,
    is_ssl_verification_error, resolve_verify, sanitize_url, ssl_verification_hint,
    system_ca_bundle,
};
pub use obs::{NoAuth, ObsAuth, ObsClient, ObsError, Osc, error_summary};
pub use openqa::{
    ApiCredentials, AutoOpenQA, ClientConf, IncidentName, Job, KernelOpenQA, OpenQABase,
    OpenQAClient, install_logfile_for,
};
pub use oqa_search::{
    BuildCheckResult, GroupResult, JobResult, OVERVIEW_BEGIN_MARKER, OVERVIEW_END_MARKER,
    OpenQAOverviewResult, VersionResult, aggregated_updates, build_checks, get_incident_info,
    incident_jobs, render_overview, single_incidents,
};
pub use qem_dashboard::{DashboardAutoOpenQA, QemDashboardClient, QemIncident};
pub use refhost::{Attributes, ProductDiff, Refhosts};
pub use teregen::{RegenOutcome, TeReGen, UpdatesQuery};
