use mtui_tui::SchemaError;
use mtui_tui::schema::{Kind, Schema};
use mtui_types::report_document::SCHEMA_JSON;
use serde_json::{Value, json};

fn required(node: &mtui_tui::schema::Node) -> Vec<String> {
    match &node.kind {
        Kind::Object { required, .. } => required.iter().cloned().collect(),
        other => panic!("not an object: {other:?}"),
    }
}

#[test]
fn the_shipped_schema_loads() {
    Schema::load().expect("every keyword in the shipped schema is understood");
}

#[test]
fn an_unknown_keyword_fails_the_load_with_its_location() {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["$defs"]["review"]["properties"]["source"]["oneOf"] = json!([]);

    let err = Schema::parse(&schema.to_string()).unwrap_err();

    let SchemaError::UnsupportedKeyword { pointer, keyword } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(keyword, "oneOf");
    assert_eq!(pointer, "/$defs/review/properties/source");
}

#[test]
fn a_root_only_keyword_is_refused_below_the_root() {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["properties"]["verdict"]["$defs"] = json!({});

    assert!(matches!(
        Schema::parse(&schema.to_string()),
        Err(SchemaError::UnsupportedKeyword { .. })
    ));
}

#[test]
fn a_pattern_the_regex_engine_rejects_fails_the_load() {
    let mut schema: Value = serde_json::from_str(SCHEMA_JSON).unwrap();
    schema["properties"]["id"]["pattern"] = json!("(unclosed");

    assert!(matches!(
        Schema::parse(&schema.to_string()),
        Err(SchemaError::BadPattern { .. })
    ));
}

#[test]
fn an_issue_status_is_a_nullable_enum_of_the_eight_statuses() {
    let root = Schema::load().unwrap().for_kind("maintenance");

    let node = root.lookup("/issues/bsc#1/status").expect("status node");

    let (inner, nullable) = node.unwrapped();
    assert!(nullable);
    let Kind::Enum(options) = &inner.kind else {
        panic!("not an enum: {inner:?}");
    };
    assert_eq!(
        options,
        &[
            "FIXED",
            "NOT_FIXED",
            "HYPOTHETICAL",
            "NOT_REPRODUCIBLE",
            "NO_ENVIRONMENT",
            "TOO_COMPLEX",
            "SKIPPED",
            "OTHER"
        ]
    );
    assert!(
        node.description
            .as_deref()
            .is_some_and(|d| d.contains("Validation outcome"))
    );
}

#[test]
fn a_referenced_node_keeps_the_description_written_beside_the_reference() {
    let root = Schema::load().unwrap().for_kind("maintenance");

    let l3 = root.lookup("/issues/bsc#1/l3").unwrap();

    assert_eq!(
        l3.description.as_deref(),
        Some("SolidGround record, present only for L3-tracked bugs.")
    );
}

#[test]
fn properties_keep_their_declared_order() {
    let root = Schema::load().unwrap().for_kind("maintenance");

    let Kind::Object { props, .. } = &root.lookup("/review/source").unwrap().kind else {
        panic!("not an object");
    };

    let names: Vec<_> = props.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "new_version_or_package",
            "all_tracked_issues_documented",
            "untracked_changes",
            "comment"
        ]
    );
}

#[test]
fn review_is_required_for_maintenance_and_slfo_but_not_for_a_pi() {
    let schema = Schema::load().unwrap();

    assert!(required(&schema.for_kind("maintenance")).contains(&"review".to_owned()));
    assert!(required(&schema.for_kind("slfo")).contains(&"review".to_owned()));
    assert!(!required(&schema.for_kind("pi")).contains(&"review".to_owned()));
}

#[test]
fn only_a_pi_requires_update_patches() {
    let schema = Schema::load().unwrap();
    let patches = |kind| {
        required(schema.for_kind(kind).lookup("/update").unwrap()).contains(&"patches".to_owned())
    };

    assert!(patches("pi"));
    assert!(!patches("maintenance"));
}

#[test]
fn a_map_node_carries_its_key_pattern() {
    let root = Schema::load().unwrap().for_kind("maintenance");

    let Kind::Map { key_pattern, .. } = &root.lookup("/issues").unwrap().kind else {
        panic!("issues is not a map");
    };

    assert!(key_pattern.as_ref().unwrap().is_match("bsc#12345"));
    assert!(!key_pattern.as_ref().unwrap().is_match("CVE-2026-1"));
}
