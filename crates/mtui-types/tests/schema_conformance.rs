//! JSON-Schema conformance test helper. Compiles the committed schema **once**
//! with format assertions enabled (teregen's JSON::Validator 5.19 asserts
//! `format` even though draft 2019-09 makes it annotation-only, so a malformed
//! `generated_at` is a real `422` on the live server) and exposes one function
//! every later conformance test goes through, so a future swap to
//! `jsonschema` is a single-file change.
//!
//! `boon` is a dev-dependency only — never a dependency of `mtui`/`mtui-mcp`.

use std::sync::LazyLock;

use boon::{Compiler, Draft, SchemaIndex, Schemas};
use serde_json::Value;

/// The schema copy fixture: semantically identical to the live
/// `GET /api/v2/schema` endpoint, byte-different only in JSON formatting
/// (the live form is Mojo::JSON's compact, key-sorted canonical encoding;
/// this copy is pretty-printed for readability) — see `cargo xtask
/// schema-check`, which compares the two by value rather than by bytes.
const SCHEMA_JSON: &str = include_str!("fixtures/document/report-template-v1.json");

/// A fake `loc` URL for `add_resource`/`compile`: only used as a compiler-side
/// key, never dereferenced over the network.
const SCHEMA_LOC: &str = "https://qam.suse.de/schema/report-template-v1.json";

static COMPILED: LazyLock<(Schemas, SchemaIndex)> = LazyLock::new(|| {
    let schema_value: Value =
        serde_json::from_str(SCHEMA_JSON).expect("committed schema fixture is valid JSON");
    let mut compiler = Compiler::new();
    compiler.set_default_draft(Draft::V2019_09);
    compiler.enable_format_assertions();
    compiler
        .add_resource(SCHEMA_LOC, schema_value)
        .expect("adding the schema resource");
    let mut schemas = Schemas::new();
    let index = compiler
        .compile(SCHEMA_LOC, &mut schemas)
        .expect("compiling the committed schema");
    (schemas, index)
});

/// One conformance failure: the RFC 6901 instance pointer (array indices
/// included) and the human-readable message for that leaf assertion.
pub type ConformanceError = (String, String);

/// Validate `instance` against the committed report-document schema (draft
/// 2019-09, format assertions on).
///
/// # Errors
///
/// Returns one `(instance_pointer, message)` pair per **leaf** validation
/// failure (a node with no sub-causes) — the container kinds (`allOf`/`anyOf`/
/// `$ref` indirection) are skipped since they carry no assertion of their own,
/// only their children do.
pub fn validate_against_schema(instance: &Value) -> Result<(), Vec<ConformanceError>> {
    let (schemas, index) = &*COMPILED;
    match schemas.validate(instance, *index) {
        Ok(()) => Ok(()),
        Err(err) => {
            let mut leaves = Vec::new();
            collect_leaves(&err, &mut leaves);
            // A validation error always has at least one leaf cause; an empty
            // vec here would silently turn a real failure into `Ok`-shaped
            // noise downstream.
            debug_assert!(!leaves.is_empty(), "validation error with no leaf causes");
            Err(leaves)
        }
    }
}

/// Recursively collect leaf `(instance_location, message)` pairs from a
/// `boon::ValidationError` tree. A leaf is a node with no further causes.
fn collect_leaves(err: &boon::ValidationError<'_, '_>, out: &mut Vec<ConformanceError>) {
    if err.causes.is_empty() {
        out.push((err.instance_location.to_string(), err.kind.to_string()));
    } else {
        for cause in &err.causes {
            collect_leaves(cause, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_schema_compiles() {
        // Forces `COMPILED` to run; a schema syntax error panics here rather
        // than surfacing as a mysterious failure in an unrelated test.
        let _ = &*COMPILED;
    }

    #[test]
    fn a_minimal_valid_document_passes() {
        let doc = serde_json::json!({
            "schema_version": "1.0",
            "id": "SUSE:Maintenance:1:2",
            "kind": "maintenance",
            "workflow": "obs",
            "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null,
            "comment": null,
            "people": {"testers": [], "reviewer": {"name": null}},
            "update": {
                "packager": "someone@suse.com",
                "source_packages": ["pkg"],
                "origin": {},
                "products": [{"name": "SLES", "version": "15", "archs": ["x86_64"]}]
            },
            "install": {
                "repository": "http://example.com/",
                "targets": [{
                    "product": "SLES", "version": "15", "arch": "x86_64",
                    "repository": "http://example.com/repo",
                    "binaries": {"pkg": "1.0-1.x86_64"}
                }],
                "test_platforms": []
            },
            "issues": {},
            "testing": {},
            "review": {
                "source": {
                    "new_version_or_package": null,
                    "all_tracked_issues_documented": null,
                    "untracked_changes": null,
                    "comment": null
                },
                "build_log": {
                    "test_suite_present": null,
                    "test_suite_sufficient": null,
                    "test_suite_passed": null,
                    "comment": null
                }
            }
        });
        assert_eq!(validate_against_schema(&doc), Ok(()));
    }

    #[test]
    fn a_bad_generated_at_is_rejected_by_format_assertions() {
        let doc = serde_json::json!({
            "schema_version": "1.0",
            "id": "SUSE:Maintenance:1:2",
            "kind": "pi",
            "workflow": "obs",
            "generated_at": "not-a-date",
            "verdict": null,
            "comment": null,
            "people": {"testers": [], "reviewer": {"name": null}},
            "update": {
                "packager": "someone@suse.com",
                "source_packages": ["pkg"],
                "origin": {},
                "products": [{"name": "SLES", "version": "15", "archs": ["x86_64"]}]
            },
            "install": {
                "repository": "http://example.com/",
                "targets": [{
                    "product": "SLES", "version": "15", "arch": "x86_64",
                    "repository": "http://example.com/repo",
                    "binaries": {"pkg": "1.0-1.x86_64"}
                }],
                "test_platforms": []
            },
            "issues": {},
            "testing": {}
        });
        let errs = validate_against_schema(&doc).expect_err("bad date-time must be rejected");
        assert!(
            errs.iter().any(|(ptr, _)| ptr == "/generated_at"),
            "{errs:?}"
        );
    }

    #[test]
    fn a_bogus_target_element_reports_the_exact_array_index() {
        let mut doc = serde_json::json!({
            "schema_version": "1.0",
            "id": "SUSE:Maintenance:1:2",
            "kind": "maintenance",
            "workflow": "obs",
            "generated_at": "2026-01-01T00:00:00Z",
            "verdict": null,
            "comment": null,
            "people": {"testers": [], "reviewer": {"name": null}},
            "update": {
                "packager": "someone@suse.com",
                "source_packages": ["pkg"],
                "origin": {},
                "products": [{"name": "SLES", "version": "15", "archs": ["x86_64"]}]
            },
            "install": {
                "repository": "http://example.com/",
                "targets": [],
                "test_platforms": []
            },
            "issues": {},
            "testing": {},
            "review": {
                "source": {
                    "new_version_or_package": null,
                    "all_tracked_issues_documented": null,
                    "untracked_changes": null,
                    "comment": null
                },
                "build_log": {
                    "test_suite_present": null,
                    "test_suite_sufficient": null,
                    "test_suite_passed": null,
                    "comment": null
                }
            }
        });
        // Five good targets, then one deliberately broken at index 5: `arch`
        // is a number, `binaries` is empty, and a stray key is present.
        let targets = doc["install"]["targets"].as_array_mut().unwrap();
        for _ in 0..5 {
            targets.push(serde_json::json!({
                "product": "SLES", "version": "15", "arch": "x86_64",
                "repository": "http://example.com/repo",
                "binaries": {"pkg": "1.0-1.x86_64"}
            }));
        }
        targets.push(serde_json::json!({
            "product": "SLES", "version": "15", "arch": 42,
            "repository": "http://example.com/repo",
            "binaries": {},
            "bogus_key": "nope"
        }));

        let errs = validate_against_schema(&doc).expect_err("broken target must be rejected");
        assert!(
            errs.iter().any(|(ptr, _)| ptr == "/install/targets/5/arch"),
            "expected /install/targets/5/arch in {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|(ptr, msg)| ptr == "/install/targets/5/binaries"
                    && msg.contains("minimum 1 properties")),
            "expected a minProperties failure on /install/targets/5/binaries in {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|(ptr, msg)| ptr == "/install/targets/5" && msg.contains("bogus_key")),
            "expected additionalProperties 'bogus_key' in {errs:?}"
        );
    }
}
