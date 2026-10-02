//! Which leaves of the report document a tester may edit.
//!
//! A pointer pattern is a `/`-separated list of tokens where `*` stands for
//! exactly one token. Everything not listed is shown read-only: the pipeline
//! owns it, or `export` rewrites it.

const EDITABLE: &[&str] = &[
    "/verdict",
    "/comment",
    "/people/reviewer/name",
    "/issues/*/reproducer",
    "/issues/*/status",
    "/issues/*/comment",
    "/testing/install/verdict",
    "/testing/install/comment",
    "/testing/install/checks/*/verdict",
    "/testing/regression/verdict",
    "/testing/regression/comment",
    "/review/source/*",
    "/review/build_log/test_suite_present",
    "/review/build_log/test_suite_sufficient",
    "/review/build_log/test_suite_passed",
    "/review/build_log/comment",
];

/// Optional subtrees the editor lists even when absent.
const CREATABLE_ROOTS: &[&str] = &["/testing/install", "/testing/regression"];

fn matches(pattern: &str, pointer: &str) -> bool {
    let mut pattern = pattern.split('/');
    let mut pointer = pointer.split('/');
    loop {
        match (pattern.next(), pointer.next()) {
            (None, None) => return true,
            (Some(p), Some(t)) if p == "*" || p == t => {}
            _ => return false,
        }
    }
}

/// Whether the leaf at `pointer` may be edited.
#[must_use]
pub fn is_editable(pointer: &str) -> bool {
    EDITABLE.iter().any(|pattern| matches(pattern, pointer))
}

/// Whether an absent subtree at `pointer` is worth listing (and, when a
/// minimal value can be built, creating).
#[must_use]
pub fn is_creatable_root(pointer: &str) -> bool {
    CREATABLE_ROOTS.contains(&pointer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_stands_for_exactly_one_token() {
        assert!(is_editable("/issues/bsc#1/status"));
        assert!(!is_editable("/issues/status"));
        assert!(!is_editable("/issues/bsc#1/extra/status"));
        assert!(is_editable("/review/source/comment"));
    }

    #[test]
    fn pipeline_facts_and_export_output_are_not_editable() {
        for pointer in [
            "/people/testers",
            "/people/reviewer/slack/ts",
            "/issues/bsc#1/title",
            "/issues/bsc#1/severity",
            "/testing/install/checks/0/refhost",
            "/testing/install/checks/0/after/foo",
            "/testing/openqa/install/verdict",
            "/review/build_log/results",
            "/update/packager",
        ] {
            assert!(!is_editable(pointer), "{pointer}");
        }
    }
}
