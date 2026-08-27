//! The QEM Dashboard connector.
//!
//! The QEM Dashboard is the read-only source of truth for an incident's openQA
//! state in the *auto* update workflow. This module is split along three
//! seams:
//!
//! * [`client`] — [`QemDashboardClient`], the low-level read-only HTTP client
//!   over the shared [`HttpClient`](crate::http::HttpClient). Every fetch folds
//!   any failure into `None`/`[]`.
//! * [`incident`] — [`QemIncident`], the incident-metadata model: it resolves
//!   the dashboard incident number from an [`RequestReviewID`] and fetches the
//!   incident record.
//! * [`dashboard_openqa`] — [`DashboardAutoOpenQA`], the auto-workflow data
//!   provider that loads the incident + aggregate openQA jobs and renders the
//!   review-facing `Results from openQA jobs` block.
//!
//! Jobs are fanned out concurrently with `tokio`, each fetch guarded by
//! [`tokio::time::timeout`] with a 60s per-future cap (`FUTURE_TIMEOUT`), while
//! preserving a fixed ordering (incident settings first, then update settings;
//! jobs in submission order) and warn-and-skip on timeout. [`DashboardAutoOpenQA`]
//! takes no config: `openqa_install_distri` / `openqa_install_logs` are pinned
//! constants (`OPENQA_INSTALL_DISTRI`, `install_logfile_for`).
//!
//! [`RequestReviewID`]: mtui_types::RequestReviewID

pub mod client;
pub mod dashboard_openqa;
pub mod incident;

pub use client::QemDashboardClient;
pub use dashboard_openqa::DashboardAutoOpenQA;
pub use incident::QemIncident;
