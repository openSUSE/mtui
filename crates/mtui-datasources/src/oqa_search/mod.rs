//! The openQA / QAM Dashboard / build-check overview search, adapting the
//! heuristics from <https://github.com/mjdonis/oqa-search>.
//!
//! This connector answers "what is the openQA state of this incident?" along
//! three paths, each a public entry point returning typed rows the command
//! layer renders (this connector never prints):
//!
//! * [`single_incidents`] — per-SLE-version PASSED / FAILED / RUNNING status for
//!   an incident build.
//! * [`aggregated_updates`] — the same status, but walked back over the last N
//!   days of aggregated maintenance builds until one covering the incident is
//!   found.
//! * [`build_checks`] — parse the qam.suse.de `build_checks` directory index and
//!   extract per-package test summaries from each `.log`.
//!
//! Two lower-level fetch helpers are also public because the command layer calls
//! them directly: [`get_incident_info`] (build name + affected versions from the
//! Dashboard) and [`incident_jobs`] (the individual openQA jobs for a build).
//!
//! Submodule seams: `heuristics` (the constants / blocklists driving group
//! filtering and log-line extraction), [`results`] (the public result shapes),
//! [`search`] (the fetch layer, pure helpers and entry points) and [`render`]
//! (the plain-text renderer plus the `OVERVIEW_*` block markers the command
//! layer and the export injector share).

pub(crate) mod heuristics;
pub mod render;
pub mod results;
pub mod search;

pub use render::{OVERVIEW_BEGIN_MARKER, OVERVIEW_END_MARKER, render_overview};
pub use results::{BuildCheckResult, GroupResult, JobResult, OpenQAOverviewResult, VersionResult};
pub use search::{
    aggregated_updates, build_checks, extract_test_results, get_incident_info, incident_jobs,
    single_incidents, summarize_test_results,
};
