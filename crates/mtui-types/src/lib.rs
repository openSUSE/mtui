//! `mtui-types` — domain types and the error hierarchy for mtui-rs.
//!
//! Foundation crate: no I/O, no async. Real types land in Phase 1.

/// Returns the crate name. Placeholder until Phase 1 introduces domain types.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_reported() {
        assert_eq!(crate_name(), "mtui-types");
    }
}
