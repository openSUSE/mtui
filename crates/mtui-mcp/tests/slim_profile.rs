//! P7.9 golden contracts: the slimmed tool schemas and the `core` keep-set.
//!
//! These pin two things a client depends on:
//!
//! * the full **slimmed** JSON schema of every synthesised command tool (the
//!   half `tools_synthesis.rs` deferred to P7.9), so a schema-slimming or
//!   arg-spec regression surfaces in review; and
//! * the resolved `core` profile keep-set against the live registry, so a
//!   rename that drops a curated tool from `core` is caught.

#![cfg(feature = "mcp")]

use std::collections::BTreeSet;

use mtui_core::register_all;
use mtui_mcp::{
    CORE, build_tools, job_tool_descriptors, resolve_keep_set, slim_input_schema,
    testreport_tool_descriptors,
};
use serde_json::{Value, json};

/// Golden of every command tool's **slimmed** schema (name + read_only +
/// input_schema), mirroring `testreport_tools`' full-schema snapshot. Locks the
/// output of the slimming pass over the real synthesised surface.
#[test]
fn slimmed_command_tool_schemas_snapshot() {
    let mut rows: Vec<Value> = build_tools(&register_all())
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "read_only": d.read_only,
                "input_schema": Value::Object(slim_input_schema(&d.input_schema)),
            })
        })
        .collect();
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let pretty = serde_json::to_string_pretty(&Value::Array(rows)).unwrap();
    insta::assert_snapshot!(pretty);
}

/// The slimmed schemas carry no `"title"` keyword and no bare `{"type":"null"}`
/// null-arm anywhere — the two structural wins of the pass. (A property *named*
/// `title` would be fine; the registry has none today, so a blunt scan suffices.)
#[test]
fn slimmed_schemas_drop_titles_and_null_arms() {
    for descriptor in build_tools(&register_all()) {
        let slimmed = slim_input_schema(&descriptor.input_schema);
        let blob = Value::Object(slimmed).to_string();
        assert!(
            !blob.contains("\"title\""),
            "{} retains a title keyword: {blob}",
            descriptor.name
        );
        assert!(
            !blob.contains("{\"type\":\"null\"}"),
            "{} retains a null arm: {blob}",
            descriptor.name
        );
    }
}

/// The resolved `core` keep-set against the live registry surface. Pins exactly
/// which synthesised tools survive `profile = core`.
#[test]
fn core_keep_set_snapshot() {
    let mut registered: BTreeSet<String> = build_tools(&register_all())
        .iter()
        .map(|d| d.name.clone())
        .collect();
    registered.extend(job_tool_descriptors().iter().map(|d| d.name.clone()));
    registered.extend(testreport_tool_descriptors().iter().map(|d| d.name.clone()));

    let keep = resolve_keep_set(&registered, "core", &[], &[]);
    let rendered = keep.into_iter().collect::<Vec<_>>().join("\n");
    insta::assert_snapshot!(rendered);
}

/// Every name in the curated `CORE` set must exist in the live surface — guards
/// against a typo or a command rename silently shrinking `core`.
#[test]
fn core_names_all_exist_in_registry() {
    let mut registered: BTreeSet<String> = build_tools(&register_all())
        .iter()
        .map(|d| d.name.clone())
        .collect();
    registered.extend(job_tool_descriptors().iter().map(|d| d.name.clone()));
    registered.extend(testreport_tool_descriptors().iter().map(|d| d.name.clone()));

    let missing: Vec<&str> = CORE
        .iter()
        .copied()
        .filter(|n| !registered.contains(*n))
        .collect();
    assert!(missing.is_empty(), "CORE names not registered: {missing:?}");
}
