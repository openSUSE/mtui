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
//! ## Notable design points
//!
//! * **Native async fan-out.** Jobs are fanned out
//!   concurrently with `tokio`, each fetch guarded by
//!   [`tokio::time::timeout`] with a 60s per-future wall-clock cap
//!   ([`FUTURE_TIMEOUT`](dashboard_openqa::FUTURE_TIMEOUT)), preserving a
//!   fixed ordering (incident settings first, then update settings; jobs in
//!   submission order) and a warn-and-skip-on-timeout behaviour, without a
//!   thread pool.
//! * **No `config` dependency.** [`DashboardAutoOpenQA`]'s
//!   `openqa_install_distri` / `openqa_install_logs` are pinned Rust
//!   constants ([`OPENQA_INSTALL_DISTRI`](crate::openqa::OPENQA_INSTALL_DISTRI),
//!   [`install_logfile_for`](crate::openqa::install_logfile_for)), so the
//!   constructor takes no config.
//!
//! [`RequestReviewID`]: mtui_types::RequestReviewID

pub mod client;
pub mod dashboard_openqa;
pub mod incident;

pub use client::QemDashboardClient;
pub use dashboard_openqa::DashboardAutoOpenQA;
pub use incident::QemIncident;
