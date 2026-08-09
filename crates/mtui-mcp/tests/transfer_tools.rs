//! Surface-level pins for the hand-written in-band `get`/`put` tools (#434).

#![cfg(feature = "mcp")]

use mtui_core::registry::register_all;
use mtui_mcp::{
    build_tools, job_tool_descriptors, testreport_tool_descriptors, transfer_tool_descriptors,
};
use serde_json::{Value, json};

/// Full-schema golden: pins both tool names, descriptions, input schemas, and
/// read-only hints so a field-name or cap-wording regression is visible.
#[test]
fn transfer_tool_schemas_snapshot() {
    let descriptors = transfer_tool_descriptors();
    let rendered: Vec<Value> = descriptors
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "read_only": d.read_only,
                "description": d.description,
                "input_schema": Value::Object(d.input_schema.clone()),
            })
        })
        .collect();
    let pretty = serde_json::to_string_pretty(&Value::Array(rendered)).unwrap();
    insta::assert_snapshot!(pretty);
}

/// `McpServer::build` chains descriptor sources with no dedup, so same-name
/// reuse is only sound while the synthesized `get`/`put` stay denied. This is
/// the tripwire: a name collision anywhere across the four sources means a
/// deny-list regression (or a new hand-written tool shadowing a command).
#[test]
fn tool_surface_has_no_duplicate_names() {
    let mut names: Vec<String> = build_tools(&register_all())
        .into_iter()
        .map(|d| d.name)
        .chain(job_tool_descriptors().into_iter().map(|d| d.name))
        .chain(testreport_tool_descriptors().into_iter().map(|d| d.name))
        .chain(transfer_tool_descriptors().into_iter().map(|d| d.name))
        .collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "duplicate tool names on the wire surface"
    );
}

/// The hand-written pair replaces the synthesized commands under the same
/// names: present in the default (full) surface, absent from `core` — matching
/// the pre-#434 status quo where the synthesized pair was full-profile only.
#[test]
fn get_and_put_present_in_full_absent_in_core() {
    let synthesized: Vec<String> = build_tools(&register_all())
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        !synthesized.contains(&"get".to_owned()) && !synthesized.contains(&"put".to_owned()),
        "synthesized get/put must stay denied: {synthesized:?}"
    );

    let transfer: Vec<String> = transfer_tool_descriptors()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(transfer, vec!["get", "put"]);
    for name in &transfer {
        assert!(
            !mtui_mcp::CORE.contains(&name.as_str()),
            "{name} must not be in the core profile"
        );
    }
}
