//! The QEM Dashboard connector, ported from `mtui/data_sources/qem_dashboard/`.
//!
//! The QEM Dashboard is the read-only source of truth for an incident's openQA
//! state in the *auto* update workflow. This module mirrors the upstream package
//! 1:1 along three seams:
//!
//! * [`client`] — [`QemDashboardClient`], the low-level read-only HTTP client
//!   over the shared [`HttpClient`](crate::http::HttpClient). Every fetch folds
//!   any failure into `None`/`[]` (upstream `QEMDashboardClient._get`).
//! * [`incident`] — [`QemIncident`], the incident-metadata model: it resolves
//!   the dashboard incident number from an [`RequestReviewID`] and fetches the
//!   incident record.
//! * [`dashboard_openqa`] — [`DashboardAutoOpenQA`], the auto-workflow data
//!   provider that loads the incident + aggregate openQA jobs and renders the
//!   review-facing `Results from openQA jobs` block.
//!
//! ## Deviations from upstream (this is a redesign, not a transpile)
//!
//! * **Native async fan-out.** Upstream `_load_jobs` bolts a `ThreadPoolExecutor`
//!   onto the synchronous `requests` client with a 60s per-future wall-clock cap
//!   ([`FUTURE_TIMEOUT`](dashboard_openqa::FUTURE_TIMEOUT)). This port fans out
//!   concurrently with `tokio` and guards each fetch with
//!   [`tokio::time::timeout`], preserving the exact ordering (incident settings
//!   first, then update settings; jobs in submission order) and the
//!   warn-and-skip-on-timeout behaviour, without a thread pool.
//! * **No `config` dependency.** Upstream `DashboardAutoOpenQA` reads
//!   `config.openqa_install_distri` / `config.openqa_install_logs`; both are now
//!   pinned Rust constants ([`OPENQA_INSTALL_DISTRI`](crate::openqa::OPENQA_INSTALL_DISTRI),
//!   [`install_logfile_for`](crate::openqa::install_logfile_for)), so the
//!   constructor takes no config.
//!
//! [`RequestReviewID`]: mtui_types::RequestReviewID

pub mod client;
pub mod dashboard_openqa;
pub mod incident;

pub use client::{FAILED_RESULTS, FUTURE_TIMEOUT, QemDashboardClient};
pub use dashboard_openqa::DashboardAutoOpenQA;
pub use incident::QemIncident;
