//! `mtui-config` — INI config parsing and XDG path resolution for mtui-rs.
//!
//! Real config loading lands in Phase 1.

/// Returns the crate name. Placeholder until Phase 1 introduces config loading.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_reported() {
        assert_eq!(crate_name(), "mtui-config");
    }
}
