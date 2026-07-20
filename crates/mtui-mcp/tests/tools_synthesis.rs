//! P7.6 synthesis contract test.
//!
//! Library-level (not stdio) checks that the tool set synthesised from the
//! command registry honours the deny-list, the `config` fan-out, the slow-command
//! `background` flag, and the read-only allow-list. The full stdio round-trip
//! (`tools/list` + `tools/call` over a transport) is P7.7's gating test.

#![cfg(feature = "mcp")]

use mtui_core::register_all;
use mtui_mcp::{build_tools, job_tool_descriptors, testreport_tool_descriptors};
use serde_json::Value;

/// Deny-listed commands never appear as tools.
#[test]
fn deny_list_is_filtered() {
    let tools = build_tools(&register_all());
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    for denied in [
        "quit", "exit", "EOF", "edit", "shell", "lrun", "help", "terms", "switch",
    ] {
        assert!(!names.contains(&denied), "{denied} leaked into tools");
    }
    // A representative exposed command is present.
    assert!(names.contains(&"run"), "run should be a tool");
}

/// `config` is fanned out; the bare `config` tool is absent.
#[test]
fn config_fan_out() {
    let tools = build_tools(&register_all());
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(!names.contains(&"config"), "bare config must be absent");
    assert!(names.contains(&"config_show"));
    assert!(names.contains(&"config_set"));
}

/// Snapshot the synthesised surface — tool name + read_only flag — plus the job
/// tools, so an accidental deny/rename/hint change surfaces in review. Full-schema
/// goldens are P7.9's job; this pins names + hints only.
#[test]
fn tool_surface_snapshot() {
    let mut rows: Vec<String> = build_tools(&register_all())
        .iter()
        .map(|t| format!("{} read_only={}", t.name, t.read_only))
        .collect();
    rows.sort();

    let mut jobs: Vec<String> = job_tool_descriptors()
        .iter()
        .map(|t| format!("{} read_only={}", t.name, t.read_only))
        .collect();
    jobs.sort();

    let rendered = format!(
        "command tools:\n{}\n\njob tools:\n{}\n",
        rows.join("\n"),
        jobs.join("\n"),
    );
    insta::assert_snapshot!(rendered);
}

/// Every advertised tool schema — synthesised command tools, the job tools, and
/// the hand-written testreport tools — must be strict (`additionalProperties:
/// false`), so a client cannot silently pass a misspelled field. A drift guard:
/// a future tool that regresses to an open schema fails here.
#[test]
fn every_tool_schema_rejects_unknown_properties() {
    let all = build_tools(&register_all())
        .into_iter()
        .chain(job_tool_descriptors())
        .chain(testreport_tool_descriptors());
    for tool in all {
        assert_eq!(
            tool.input_schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "tool `{}` schema must set additionalProperties:false, got {:?}",
            tool.name,
            tool.input_schema.get("additionalProperties"),
        );
    }
}
