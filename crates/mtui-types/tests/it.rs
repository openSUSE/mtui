//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its dev-deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.

#[path = "refhost.rs"]
mod refhost;
#[path = "report_document.rs"]
mod report_document;
#[path = "rpmver.rs"]
mod rpmver;
#[path = "rrid.rs"]
mod rrid;
// Shared test support rather than a test module in its own right: the one
// `validate_against_schema` helper every conformance assertion goes through.
#[path = "schema_conformance.rs"]
mod schema_conformance;
#[path = "updateid.rs"]
mod updateid;
