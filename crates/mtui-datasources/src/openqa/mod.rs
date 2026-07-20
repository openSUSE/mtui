//! The openQA connectors, ported from `mtui/data_sources/openqa/`.
//!
//! openQA is the automated-test system that runs the maintenance-update install
//! and regression jobs. These connectors query an openQA instance for the jobs
//! belonging to an incident and map the raw job JSON into the typed results mtui
//! reports on:
//!
//! * [`base`] — the shared query-parameter build, job fetch, and the
//!   [`IncidentName`] seam and [`Job`] model.
//! * [`client`] — the REST client reproducing the `openqa_client` auth contract
//!   (INI `client.conf`, `X-API-Key`, HMAC-SHA1 `X-API-Hash`) over this crate's
//!   shared [`HttpClient`](crate::http::HttpClient).
//! * [`standard`] — the "auto" workflow ([`AutoOpenQA`]): install-log URLs.
//! * [`kernel`] — the "kernel" workflow ([`KernelOpenQA`]): the LTP test matrix.
//! * [`install`] — the install-job → log-filename map ([`install_logfile_for`]).

pub mod base;
pub mod client;
pub mod install;
pub mod kernel;
pub mod standard;

pub use base::{IncidentName, Job, JobModule, OPENQA_INSTALL_DISTRI, OpenQABase};
pub use client::{ApiCredentials, ClientConf, OpenQAClient, compute_api_hash};
pub use install::{DEFAULT_INSTALL_LOGFILE, install_logfile_for};
pub use kernel::KernelOpenQA;
pub use standard::AutoOpenQA;
