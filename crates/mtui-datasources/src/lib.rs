//! `mtui-datasources` — shared HTTP client, refhosts, openQA/QEM/Gitea/osc-qam.
//!
//! Async data-source clients land in Phase 3.

/// Returns the crate name. Placeholder until Phase 3 introduces data sources.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}
