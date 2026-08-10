//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its heavy deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.
//!
//! `log_capture` is shared test support rather than a test module: it installs
//! one process-global tracing subscriber, which cannot be done per file.

#[path = "gitea.rs"]
mod gitea;
#[path = "http_client.rs"]
mod http_client;
#[path = "log_capture.rs"]
mod log_capture;
#[path = "obs_auth.rs"]
mod obs_auth;
#[path = "obs_client.rs"]
mod obs_client;
#[path = "obs_facade.rs"]
mod obs_facade;
#[path = "obs_oscrc.rs"]
mod obs_oscrc;
#[path = "obs_qam.rs"]
mod obs_qam;
#[path = "obs_sshsig.rs"]
mod obs_sshsig;
#[path = "openqa.rs"]
mod openqa;
#[path = "oqa_search.rs"]
mod oqa_search;
#[path = "qem_dashboard.rs"]
mod qem_dashboard;
#[path = "refhost.rs"]
mod refhost;
#[path = "slack.rs"]
mod slack;
