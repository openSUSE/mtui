//! Round-trip, conformance, key-set and unknown-key tests for
//! [`ReportDocument`].

use std::collections::{BTreeMap, BTreeSet};

use mtui_types::report_document::{
    BuildLog, DocWorkflow, DocumentKind, Install, Origin, People, Product, ReportDocument, Review,
    ReviewSource, Reviewer, SchemaVersion, Target, Testing, Update, dropped_pointers,
};
use serde_json::{Value, json};

use crate::schema_conformance::validate_against_schema;

const MAINTENANCE_OBS: &str = include_str!("fixtures/document/maintenance_obs.json");
const MAINTENANCE_ADDON: &str = include_str!("fixtures/document/maintenance_addon.json");
const SLFO_GITEA: &str = include_str!("fixtures/document/slfo_gitea.json");
const MAINTENANCE_OPENQA_L3: &str = include_str!("fixtures/document/maintenance_openqa_l3.json");
const MAXIMAL: &str = include_str!("fixtures/document/maximal.json");
const PI: &str = include_str!("fixtures/document/pi.json");

/// Every fixture this phase's four properties are checked against.
const FIXTURES: [(&str, &str); 6] = [
    ("maintenance_obs", MAINTENANCE_OBS),
    ("maintenance_addon", MAINTENANCE_ADDON),
    ("slfo_gitea", SLFO_GITEA),
    ("maintenance_openqa_l3", MAINTENANCE_OPENQA_L3),
    ("maximal", MAXIMAL),
    ("pi", PI),
];

// --- Property 1: semantic round-trip (P1-D3). ---

#[test]
fn every_fixture_round_trips_to_an_identical_value() {
    for (name, raw) in FIXTURES {
        let doc: ReportDocument = raw
            .parse()
            .unwrap_or_else(|e| panic!("{name}: failed to parse: {e}"));
        let round_tripped = serde_json::to_value(&doc).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(round_tripped, original, "{name}: round-trip mismatch");
    }
}

// --- Property 2: schema conformance. ---

#[test]
fn every_fixture_serialised_form_validates_against_the_schema() {
    for (name, raw) in FIXTURES {
        let doc: ReportDocument = raw.parse().unwrap();
        let value = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            validate_against_schema(&value),
            Ok(()),
            "{name}: serialised form does not validate"
        );
    }
}

// --- Property 3: required/optional key-set table. ---

/// A document with every optional field absent — the executable form of the
/// module's B rules. `kind: maintenance` is chosen deliberately: it is the
/// one allOf-conditional branch that requires `review`, so building this
/// with `review: None` would itself be schema-invalid and this test would be
/// validating a document real teregen could never emit.
fn minimal_document() -> ReportDocument {
    ReportDocument {
        schema_version: SchemaVersion,
        id: "SUSE:Maintenance:1:2".to_owned(),
        kind: DocumentKind::Maintenance,
        workflow: DocWorkflow::Obs,
        generated_at: "2026-01-01T00:00:00Z".to_owned(),
        verdict: None.into(),
        comment: None.into(),
        people: People {
            testers: vec![],
            reviewer: Reviewer {
                name: None.into(),
                slack: None,
            },
        },
        update: Update {
            packager: "someone@suse.com".to_owned(),
            source_packages: vec!["pkg".to_owned()],
            origin: Origin::default(),
            products: vec![Product {
                name: "SLES".to_owned(),
                version: "15".to_owned(),
                archs: vec!["x86_64".to_owned()],
            }],
            category: None,
            rating: None,
            patches: None,
        },
        install: Install {
            repository: "http://example.com/".to_owned(),
            targets: vec![Target {
                product: "SLES".to_owned(),
                version: "15".to_owned(),
                arch: "x86_64".to_owned(),
                repository: "http://example.com/repo".to_owned(),
                binaries: BTreeMap::from([("pkg".to_owned(), "1.0-1.x86_64".to_owned())]),
            }],
            test_platforms: vec![],
        },
        issues: BTreeMap::new(),
        testing: Testing::default(),
        review: Some(Review {
            source: ReviewSource {
                new_version_or_package: None.into(),
                all_tracked_issues_documented: None.into(),
                untracked_changes: None.into(),
                comment: None.into(),
            },
            build_log: BuildLog {
                test_suite_present: None.into(),
                test_suite_sufficient: None.into(),
                test_suite_passed: None.into(),
                results: None,
                comment: None.into(),
            },
        }),
    }
}

/// The committed schema, parsed once for the key-set walk.
fn schema() -> Value {
    serde_json::from_str(include_str!("fixtures/document/report-template-v1.json")).unwrap()
}

/// Resolve a `$ref` (e.g. `#/$defs/people`) against `root`; returns `schema`
/// unchanged if it carries no `$ref`.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => {
            let pointer = r.strip_prefix('#').expect("schema $ref is a fragment");
            root.pointer(pointer)
                .unwrap_or_else(|| panic!("unresolvable $ref {r}"))
        }
        None => schema,
    }
}

/// Recursively assert that every object `instance` matches, wherever the
/// resolved schema declares a `required` list, that `instance`'s key set is
/// *exactly* that list — the schema's own required/optional split, applied to
/// the minimal document. `extra_required` names keys this specific object may
/// carry beyond its unconditional `required` list (only ever the root
/// document's `review`, added by an `allOf` conditional rather than the
/// top-level `required` array).
fn assert_required_key_sets(
    root: &Value,
    schema: &Value,
    instance: &Value,
    path: &str,
    extra_required: &[&str],
) {
    let schema = resolve(root, schema);
    let Some(obj) = instance.as_object() else {
        return;
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let mut expected: BTreeSet<&str> = required.iter().filter_map(Value::as_str).collect();
        expected.extend(extra_required);
        let actual: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(actual, expected, "{path}: emitted key set mismatch");
    }
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (key, subschema) in props {
            if let Some(subvalue) = obj.get(key) {
                let subpath = format!("{path}/{key}");
                recurse_into(root, subschema, subvalue, &subpath);
            }
        }
    }
}

fn recurse_into(root: &Value, schema: &Value, instance: &Value, path: &str) {
    let resolved = resolve(root, schema);
    if resolved.get("type").and_then(Value::as_str) == Some("array") {
        if let (Some(items), Some(arr)) = (resolved.get("items"), instance.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                assert_required_key_sets(root, items, item, &format!("{path}/{i}"), &[]);
            }
        }
    } else {
        assert_required_key_sets(root, schema, instance, path, &[]);
    }
}

#[test]
fn minimal_document_emits_exactly_the_schema_required_keys_at_every_level() {
    let doc = minimal_document();
    let value = serde_json::to_value(&doc).unwrap();
    // Sanity: the minimal document must itself be schema-valid — otherwise
    // this test would be pinning a shape teregen could never emit.
    assert_eq!(validate_against_schema(&value), Ok(()));

    let root = schema();
    assert_required_key_sets(&root, &root, &value, "", &["review"]);
}

// --- Property 4: unknown-key diff (P1-D2). ---

#[test]
fn unknown_key_on_a_closed_object_is_dropped_and_reported() {
    let mut value = serde_json::to_value(minimal_document()).unwrap();
    value["update"]["bogus_key"] = json!("nope");
    let raw = serde_json::to_string(&value).unwrap();

    let doc: ReportDocument = raw.parse().unwrap();
    let round_tripped = serde_json::to_value(&doc).unwrap();
    let dropped = dropped_pointers(&value, &round_tripped);

    assert_eq!(dropped, vec!["/update/bogus_key".to_owned()]);
    // And it never reappears in the serialised form.
    assert!(round_tripped["update"].get("bogus_key").is_none());
}

#[test]
fn unknown_key_under_testing_and_testing_openqa_survives_and_is_never_reported() {
    let mut value = serde_json::to_value(minimal_document()).unwrap();
    value["testing"] = json!({
        "an_unknown_testing_key": "survives",
        "openqa": { "an_unknown_openqa_key": 42 }
    });
    let raw = serde_json::to_string(&value).unwrap();

    let doc: ReportDocument = raw.parse().unwrap();
    let round_tripped = serde_json::to_value(&doc).unwrap();
    let dropped = dropped_pointers(&value, &round_tripped);

    assert!(dropped.is_empty(), "unexpected drops: {dropped:?}");
    assert_eq!(
        round_tripped["testing"]["an_unknown_testing_key"],
        json!("survives")
    );
    assert_eq!(
        round_tripped["testing"]["openqa"]["an_unknown_openqa_key"],
        json!(42)
    );
}

// --- Step 10: the two synthesised fixtures the wild cannot supply. ---

#[test]
fn maximal_document_snapshot() {
    let doc: ReportDocument = MAXIMAL.parse().unwrap();
    let pretty = serde_json::to_string_pretty(&doc).unwrap();
    insta::assert_snapshot!(pretty);
}

#[test]
fn empty_testing_object_parses() {
    // The shape 150 of 166 live documents have.
    let mut value = serde_json::to_value(minimal_document()).unwrap();
    value["testing"] = json!({});
    let raw = serde_json::to_string(&value).unwrap();
    let doc: ReportDocument = raw.parse().unwrap();
    assert!(doc.testing.openqa.is_none());
}
