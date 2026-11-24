//! Which leaves of the report document a tester may edit.
//!
//! A pointer pattern is a `/`-separated list of tokens where `*` stands for
//! exactly one token. Everything not listed is shown read-only: the pipeline
//! owns it, or `export` rewrites it.
//!
//! The report editor and the MCP field write share this one set.

use std::sync::LazyLock;

use serde_json::{Map, Value};

use super::SCHEMA_JSON;

const EDITABLE: &[&str] = &[
    "/verdict",
    "/comment",
    "/people/reviewer/name",
    "/issues/*/reproducer",
    "/issues/*/status",
    "/issues/*/comment",
    "/new_issues/*/title",
    "/new_issues/*/comment",
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
pub const CREATABLE_ROOTS: &[&str] = &["/testing/install", "/testing/regression"];

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

/// Whether `pointer` holds an editable leaf below it rather than being one.
#[must_use]
pub fn is_editable_parent(pointer: &str) -> bool {
    let depth = pointer.split('/').count();
    EDITABLE.iter().any(|pattern| {
        let mut tokens = pattern.split('/');
        let head: Vec<&str> = tokens.by_ref().take(depth).collect();
        tokens.next().is_some() && matches(&head.join("/"), pointer)
    })
}

/// Whether the entry `value` of the map at `parent` is listed elsewhere: a
/// tester-filed issue shows once, in `new_issues`, not again among `issues`.
#[must_use]
pub fn is_hidden_entry(parent: &str, value: &Value) -> bool {
    parent == "/issues" && value.get("new") == Some(&Value::Bool(true))
}

/// Whether an absent subtree at `pointer` is worth listing (and, when a
/// minimal value can be built, creating).
#[must_use]
pub fn is_creatable_root(pointer: &str) -> bool {
    CREATABLE_ROOTS.contains(&pointer)
}

/// The value a creatable subtree at `pointer` starts as: every required key
/// `null`. `None` when `pointer` is not creatable, or when a required key
/// cannot be `null`, so only `export` can build it.
#[must_use]
pub fn creatable_skeleton(pointer: &str) -> Option<Value> {
    if !is_creatable_root(pointer) {
        return None;
    }
    let node = schema_node(pointer)?;
    let props = node.get("properties")?;
    let mut map = Map::new();
    for name in node.get("required")?.as_array()? {
        let name = name.as_str()?;
        if !admits_null(resolve_ref(props.get(name)?)?) {
            return None;
        }
        map.insert(name.to_owned(), Value::Null);
    }
    Some(Value::Object(map))
}

/// The bundled schema node at the document `pointer`, through `$ref`s.
fn schema_node(pointer: &str) -> Option<&'static Value> {
    pointer
        .split('/')
        .skip(1)
        .try_fold(schema(), |node, token| {
            resolve_ref(node.get("properties")?.get(token)?)
        })
}

fn schema() -> &'static Value {
    static SCHEMA: LazyLock<Value> =
        LazyLock::new(|| serde_json::from_str(SCHEMA_JSON).expect("the bundled schema parses"));
    &SCHEMA
}

fn resolve_ref(node: &'static Value) -> Option<&'static Value> {
    let Some(target) = node.get("$ref").and_then(Value::as_str) else {
        return Some(node);
    };
    schema().get("$defs")?.get(target.strip_prefix("#/$defs/")?)
}

fn admits_null(node: &Value) -> bool {
    let in_enum = node
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|options| options.contains(&Value::Null));
    let in_type = match node.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(types)) => types.iter().any(|t| t == "null"),
        _ => false,
    };
    in_enum || in_type
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_creatable_root_whose_required_keys_admit_null_has_a_skeleton() {
        assert_eq!(
            creatable_skeleton("/testing/regression"),
            Some(serde_json::json!({"verdict": null, "comment": null}))
        );
        assert_eq!(creatable_skeleton("/testing/install"), None);
        assert_eq!(creatable_skeleton("/testing/openqa"), None);
    }

    #[test]
    fn a_parent_of_an_editable_leaf_is_told_apart_from_a_leaf() {
        for pointer in [
            "",
            "/testing",
            "/testing/regression",
            "/testing/install/checks/0",
            "/review/source",
        ] {
            assert!(is_editable_parent(pointer), "{pointer}");
        }
        for pointer in [
            "/verdict",
            "/testing/install/checks/0/before",
            "/update",
            "/testing/openqa",
        ] {
            assert!(!is_editable_parent(pointer), "{pointer}");
        }
    }

    #[test]
    fn a_star_stands_for_exactly_one_token() {
        assert!(is_editable("/issues/bsc#1/status"));
        assert!(!is_editable("/issues/status"));
        assert!(!is_editable("/issues/bsc#1/extra/status"));
        assert!(is_editable("/review/source/comment"));
    }

    #[test]
    fn a_tester_filed_issue_is_hidden_only_among_the_issues() {
        let new = serde_json::json!({"new": true});
        assert!(is_hidden_entry("/issues", &new));
        assert!(!is_hidden_entry(
            "/issues",
            &serde_json::json!({"title": "t"})
        ));
        assert!(!is_hidden_entry("/new_issues", &new));
    }

    #[test]
    fn a_filed_bugs_title_and_comment_are_editable_but_not_its_id() {
        assert!(is_editable("/new_issues/0/title"));
        assert!(is_editable("/new_issues/0/comment"));
        assert!(!is_editable("/new_issues/0/id"));
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
