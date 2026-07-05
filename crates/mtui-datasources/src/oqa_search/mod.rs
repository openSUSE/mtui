//! The openQA / QAM Dashboard / build-check overview search, ported from
//! `mtui/data_sources/oqa_search/` (itself an adaptation of
//! <https://github.com/mjdonis/oqa-search>).
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
//! The module is split along the upstream submodule seams:
//!
//! * [`heuristics`] — the verbatim upstream constants / blocklists that drive
//!   group filtering and log-line extraction.
//! * [`results`] — the public result shapes.
//! * [`search`] — the fetch layer, the pure helpers, and the entry points.
//!
//! The plain-text renderer (`render_overview` + the `OVERVIEW_*` markers)
//! upstream ships is intentionally deferred until its consumers land (the
//! command layer in Phase 5 and the export injector in Phase 4).

pub mod heuristics;
pub mod results;
pub mod search;

pub use results::{BuildCheckResult, GroupResult, JobResult, VersionResult};
pub use search::{
    aggregated_updates, build_checks, extract_test_results, get_incident_info, incident_jobs,
    log_matches_package, single_incidents, summarize_test_results,
};
