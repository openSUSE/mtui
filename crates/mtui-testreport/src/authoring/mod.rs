//! Typed document authoring — builds `testing.*` subtrees from the same data
//! the legacy text exporters already collect, instead of mutating a
//! `Vec<String>` template (P4-D1).
//!
//! Every function here is pure: no I/O, no template anchors, no `Vec<String>`.
//! The legacy exporters (`crate::export`) are untouched and stay the shipped
//! path until Phase 7 deletes them.
//!
//! Gated behind the `api-ingest` Cargo feature, the same switch Phase 3's
//! ingest path uses (P4-D2) — off in every default build, compiled and tested
//! by CI's `--all-features`/`--features api-ingest` jobs.

pub mod auto;
pub mod kernel;
pub mod manual;
pub mod overview;
