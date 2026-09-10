//! Row-budget crush reaches the MCP client intact.
//!
//! Drives the real `updates` / `list_refhosts` commands through
//! [`McpSession::run_command`] with unbounded mocked backends: `--json` tool
//! output parses as-is (the truncation notice goes to stderr, keeping stdout
//! valid JSON) while human output keeps the notice naming the narrowing flags.
//! Also pins the additive paging flags and that row-cap is not byte-cap.

#![cfg(feature = "mcp")]

use mtui_config::Config;
use mtui_core::register_all;
use mtui_mcp::McpSession;

/// 150 TeReGen rows with one mid-queue non-`testing` anomaly.
fn queue_fixture() -> serde_json::Value {
    let mut rows: Vec<serde_json::Value> = (0..150)
        .map(|i| {
            serde_json::json!({
                "priority": 1, "status": "testing", "kind": "Maintenance",
                "id": format!("row-{i:03}"),
            })
        })
        .collect();
    rows[100] = serde_json::json!({
        "priority": 1, "status": "failed", "kind": "Maintenance",
        "id": "row-anomaly",
    });
    serde_json::json!({"updates": rows})
}

/// `updates --json` over an unbounded queue: stdout stays valid JSON, anomaly kept.
#[tokio::test]
async fn updates_json_crush_stays_valid() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/updates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_fixture()))
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.teregen_api = server.uri();
    let sess = McpSession::new(config);
    let registry = register_all();

    let argv = ["--status", "all", "--json"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>();
    let out = sess
        .run_command(&registry, "updates", &argv)
        .await
        .expect("updates succeeds");
    // Notice on stderr: tool output parses as-is with no trailing notice line.
    assert!(!out.contains("…[truncated"), "{out}");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let rows = parsed.as_array().unwrap();
    assert!(rows.len() <= 100, "row budget holds: {}", rows.len());
    assert!(rows.iter().any(|r| r["id"] == "row-anomaly"), "{out}");
    assert!(!rows.iter().any(|r| r["id"] == "row-060"), "{out}");
}

/// `list_refhosts` over a 150-host inventory: head+tail kept, notice names filters.
#[tokio::test]
async fn list_refhosts_crush_notifies_with_narrowing_flags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refhosts.yml");
    let mut yaml = String::from("default:\n");
    for i in 0..150 {
        yaml.push_str(&format!(
            "  - name: host-{i:03}\n    arch: x86_64\n    product:\n      name: sles\n      version:\n        major: 15\n        minor: 6\n"
        ));
    }
    std::fs::write(&path, yaml).unwrap();

    let mut config = Config::default();
    config.refhosts_resolvers = "path".to_owned();
    config.refhosts_path = path;
    let sess = McpSession::new(config);
    let registry = register_all();

    let out = sess
        .run_command(&registry, "list_refhosts", &[])
        .await
        .expect("list_refhosts succeeds");
    assert!(
        out.lines()
            .last()
            .is_some_and(|l| l.starts_with("…[truncated")),
        "{out}"
    );
    assert!(
        out.contains("--limit/--offset/--name/--arch/--product/--version/--addon"),
        "{out}"
    );
    assert!(
        out.contains("host-000") && out.contains("host-149"),
        "{out}"
    );
    assert!(!out.contains("host-060"), "middle row dropped: {out}");
}

/// Paging recovers a dropped middle row through MCP.
#[tokio::test]
async fn updates_offset_recovers_middle_row() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/updates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(queue_fixture()))
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.teregen_api = server.uri();
    let sess = McpSession::new(config);
    let registry = register_all();

    let argv = [
        "--status", "all", "--json", "--offset", "50", "--limit", "50",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect::<Vec<_>>();
    let out = sess
        .run_command(&registry, "updates", &argv)
        .await
        .expect("updates succeeds");
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let rows = parsed.as_array().unwrap();
    assert!(rows.iter().any(|r| r["id"] == "row-060"), "{out}");
}

/// Row-cap is not byte-cap: fat rows still hit max_output_bytes after crush.
#[tokio::test]
async fn fat_rows_hit_byte_cap_after_crush() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let filler = "x".repeat(1024);
    let rows: Vec<serde_json::Value> = (0..150)
        .map(|i| {
            serde_json::json!({
                "priority": 1, "status": "testing", "kind": "Maintenance",
                "id": format!("row-{i:03}"), "title": filler,
            })
        })
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/updates"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"updates": rows})),
        )
        .mount(&server)
        .await;

    let mut config = Config::default();
    config.teregen_api = server.uri();
    config.mcp_max_output_bytes = 2000;
    let sess = McpSession::new(config);
    let registry = register_all();

    let argv = ["--status", "all", "--json"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>();
    let out = sess
        .run_command(&registry, "updates", &argv)
        .await
        .expect("updates succeeds");
    assert!(out.contains("max_output_bytes=2000"), "{out}");
    assert!(out.contains("bytes"), "{out}");
}

/// Additive paging flags only: no tool renames/removals.
#[test]
fn crushed_tool_schemas_unchanged() {
    use std::collections::HashMap;

    use mtui_mcp::build_tools;

    let tools: HashMap<String, Vec<String>> = build_tools(&register_all())
        .into_iter()
        .map(|d| {
            let props = d
                .input_schema
                .get("properties")
                .and_then(|v| v.as_object())
                .map(|m| m.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            (d.name.clone(), props)
        })
        .collect();
    for name in ["updates", "list_refhosts", "openqa_overview"] {
        assert!(tools.contains_key(name), "tool {name} renamed?");
    }
    // Paging is additive: `updates` gains `--offset`, `list_refhosts` gains
    // `--limit`/`--offset`, `openqa_overview` gains none.
    assert!(tools["updates"].contains(&"limit".to_owned()));
    assert!(tools["updates"].contains(&"offset".to_owned()));
    assert!(tools["list_refhosts"].contains(&"limit".to_owned()));
    assert!(tools["list_refhosts"].contains(&"offset".to_owned()));
    assert!(!tools["openqa_overview"].contains(&"limit".to_owned()));
}
