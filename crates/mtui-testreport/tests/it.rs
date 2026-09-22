//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its heavy deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.

// Assembling authored subtrees onto a document and validating them against
// the schema only exist under the `api-ingest` feature.
#[cfg(feature = "api-ingest")]
#[path = "authoring.rs"]
mod authoring;
#[path = "export_idempotency.rs"]
mod export_idempotency;
#[path = "fs_responsiveness.rs"]
mod fs_responsiveness;
// The document-vs-SVN equivalence test (`apply_document`) only exists under
// the `api-ingest` feature.
#[cfg(feature = "api-ingest")]
#[path = "ingest.rs"]
mod ingest;
// SVN-driven `make_testreport` behaviour only: under `--features api-ingest`
// that path is replaced by `lifecycle::load_via_document`, whose own coverage
// lives in `lifecycle.rs`'s `ingest_tests` unit-test module (colocated so it
// can call the private `load_via_document`/`document_fetch_message` seams
// directly) — there is no "SVN case" left here to run under the feature.
#[cfg(not(feature = "api-ingest"))]
#[path = "lifecycle.rs"]
mod lifecycle;
// The document-driven `make_testreport` end-to-end coverage — the public
// entry point's `#[cfg(feature = "api-ingest")]` wiring, exercised on top of
// (not instead of) the colocated `ingest_tests` unit tests above.
#[cfg(feature = "api-ingest")]
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
