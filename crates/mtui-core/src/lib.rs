//! `mtui-core` — Command trait + registry, Session, engine, and wiring.
//!
//! This is the composition root. The command engine lands in Phase 5.

/// Returns the crate name. Placeholder until Phase 5 introduces the engine.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}
