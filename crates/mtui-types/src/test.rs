//! A single openQA test result, ported from the `Test` `NamedTuple` in
//! `mtui/types/test.py`.
//!
//! Produced by the kernel openQA connector to describe one job: its name,
//! overall result, job id, architecture, and the per-module results.
//!
//! ## Deviation from upstream
//!
//! `modules` is a [`BTreeMap`] (rather than an unordered `dict`) so iteration
//! order, equality, and hashing are deterministic — matching the crate-wide
//! convention (see [`crate::system::System`]).

use std::collections::BTreeMap;

/// One openQA test/job result with its per-module breakdown.
///
/// Mirrors the upstream `Test` `NamedTuple`
/// `(name, result, test_id, arch, modules)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Test {
    /// The name of the test (e.g. `qam-kernel`).
    pub name: String,
    /// The overall result (e.g. `passed`, `failed`).
    pub result: String,
    /// The openQA job id.
    pub test_id: i64,
    /// The architecture the job ran on (e.g. `x86_64`).
    pub arch: String,
    /// Per-module results, keyed by module name.
    pub modules: BTreeMap<String, String>,
}

impl Test {
    /// Creates a new [`Test`].
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        result: impl Into<String>,
        test_id: i64,
        arch: impl Into<String>,
        modules: BTreeMap<String, String>,
    ) -> Self {
        Self {
            name: name.into(),
            result: result.into(),
            test_id,
            arch: arch.into(),
            modules,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_all_fields() {
        let mut modules = BTreeMap::new();
        modules.insert("mod_a".to_string(), "passed".to_string());
        let t = Test::new("qam-kernel", "passed", 123, "x86_64", modules.clone());
        assert_eq!(t.name, "qam-kernel");
        assert_eq!(t.result, "passed");
        assert_eq!(t.test_id, 123);
        assert_eq!(t.arch, "x86_64");
        assert_eq!(t.modules, modules);
    }
}
