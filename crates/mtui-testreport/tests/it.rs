//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its heavy deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.

#[path = "export_idempotency.rs"]
mod export_idempotency;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "metadata_parsers.rs"]
mod metadata_parsers;
#[path = "null_report.rs"]
mod null_report;
#[path = "obs_report.rs"]
mod obs_report;
#[path = "overview_inject.rs"]
mod overview_inject;
#[path = "pi_report.rs"]
mod pi_report;
#[path = "products.rs"]
mod products;
#[path = "repoparse.rs"]
mod repoparse;
#[path = "sl_report.rs"]
mod sl_report;
#[path = "svn_io.rs"]
mod svn_io;
#[path = "updateid_checkout.rs"]
mod updateid_checkout;
