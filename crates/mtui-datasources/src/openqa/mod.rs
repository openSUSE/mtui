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
//! * [`kernel`] — the "kernel" workflow ([`KernelOpenQA`]): the LTP test matrix.
//! * [`install`] — the install-job → log-filename map ([`install_logfile_for`]).

pub mod base;
pub mod client;
pub(crate) mod install;
pub mod kernel;

pub(crate) use base::OPENQA_INSTALL_DISTRI;
pub use base::{IncidentName, Job, JobModule, OpenQABase};
pub use client::{ApiCredentials, ClientConf, OpenQAClient};
pub(crate) use install::install_logfile_for;
pub use kernel::KernelOpenQA;
