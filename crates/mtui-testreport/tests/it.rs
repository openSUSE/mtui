//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its heavy deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.

#[path = "authoring.rs"]
mod authoring;
#[path = "commit_upload.rs"]
mod commit_upload;
#[path = "document_refresh.rs"]
mod document_refresh;
#[path = "export_idempotency.rs"]
mod export_idempotency;
#[path = "fs_responsiveness.rs"]
mod fs_responsiveness;
#[path = "ingest.rs"]
mod ingest;
// `TestReport::read` against on-disk fixtures.
#[path = "lifecycle.rs"]
mod lifecycle;
// The document-driven `make_testreport` end-to-end coverage, on top of
// `lifecycle.rs`'s colocated `ingest_tests` unit tests.
#[path = "lifecycle_ingest.rs"]
mod lifecycle_ingest;
// Shared test support rather than a test module: it installs a *global*
// subscriber, so the whole `it` binary must share one copy.
#[path = "log_capture.rs"]
mod log_capture;
#[path = "metadata_parsers.rs"]
mod metadata_parsers;
#[path = "null_report.rs"]
mod null_report;
#[path = "obs_report.rs"]
mod obs_report;
#[path = "overview_inject.rs"]
mod overview_inject;
#[path = "pairs_fixtures.rs"]
mod pairs_fixtures;
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
