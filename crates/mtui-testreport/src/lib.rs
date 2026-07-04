//! `mtui-testreport` — TestReport lifecycle, metadata parsers, update workflow.
//!
//! Real testreport parsing and the update workflow land in Phase 4.

/// Returns the crate name. Placeholder until Phase 4 introduces testreports.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}
