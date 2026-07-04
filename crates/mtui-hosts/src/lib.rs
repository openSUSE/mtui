//! `mtui-hosts` — SSH/SFTP host layer (russh), host groups, locks, targets.
//!
//! Async host connections land in Phase 2.

/// Returns the crate name. Placeholder until Phase 2 introduces the SSH layer.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}
