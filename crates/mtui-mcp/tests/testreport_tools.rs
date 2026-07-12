//! P7.8 testreport-tools integration test.
//!
//! Library-level (not stdio) checks that the hand-written testreport tools drive
//! a fixture checkout end-to-end through the public `dispatch_testreport_tool`
//! seam, plus a full-schema insta snapshot of the five tool descriptors so a
//! token-budget or field-name regression surfaces in review.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_mcp::{McpSession, dispatch_testreport_tool, testreport_tool_descriptors};
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use serde_json::{Map, Value, json};

const RRID: &str = "SUSE:Maintenance:1:1";

/// Build a session over a temp `template_dir` and load one active report whose
/// `log` file holds `content`. Returns the session + tempdir + log path.
async fn loaded(content: &str) -> (Arc<McpSession>, tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    let session = McpSession::new(config);

    let log = tmp.path().join("checkout").join("log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, content).unwrap();

    {
        let mut guard = session.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
        report.base_mut().path = Some(log.clone());
        guard.templates.add(Box::new(report));
        guard.templates.set_active(RRID);
    }
    (session, tmp, log)
}

async fn call(session: &McpSession, name: &str, kwargs: Value) -> Value {
    let map: Map<String, Value> = kwargs.as_object().cloned().unwrap_or_default();
    dispatch_testreport_tool(session, name, &map)
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"))
}

/// read → patch → read → fill → write, asserting each mutation lands on disk.
#[tokio::test]
async fn dispatch_end_to_end_flow() {
    let (session, _tmp, log) = loaded(
        "SUMMARY:            PASSED/FAILED\n\
         REPRODUCER_PRESENT: YES/NO\n\
         STATUS:             FIXED/NOT_FIXED/HYPOTHETICAL/NOT_REPRODUCIBLE/NO_ENVIRONMENT/TOO_COMPLEX/SKIPPED/OTHER\n\
         body line\n",
    )
    .await;

    // read the whole file
    let r = call(&session, "testreport_read", json!({})).await;
    assert_eq!(r["line_count"], 4);

    // patch the body line
    let p = call(
        &session,
        "testreport_patch",
        json!({ "start_line": 4, "end_line": 4, "replacement": "edited body" }),
    )
    .await;
    assert_eq!(p["new_line_count"], 4);
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("edited body\n")
    );

    // windowed read of just the patched line
    let w = call(
        &session,
        "testreport_read",
        json!({ "offset": 4, "limit": 1 }),
    )
    .await;
    assert_eq!(w["returned_lines"], 1);
    assert_eq!(w["content"], "edited body\n");

    // bulk-fill the placeholders
    let f = call(
        &session,
        "testreport_fill",
        json!({ "reproducer": "NO", "status": "SKIPPED", "summary": "PASSED" }),
    )
    .await;
    assert_eq!(f["filled"]["summary"], 1);
    assert_eq!(f["filled"]["reproducer"], 1);
    assert_eq!(f["filled"]["status"], 1);
    let after_fill = std::fs::read_to_string(&log).unwrap();
    assert!(
        after_fill.contains("SUMMARY:            PASSED\n"),
        "{after_fill}"
    );
    assert!(
        after_fill.contains("REPRODUCER_PRESENT: NO\n"),
        "{after_fill}"
    );
    assert!(
        after_fill.contains("STATUS:             SKIPPED\n"),
        "{after_fill}"
    );

    // full overwrite fallback
    let ow = call(
        &session,
        "testreport_write",
        json!({ "content": "brand new\ncontent\n" }),
    )
    .await;
    assert_eq!(ow["line_count"], 2);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "brand new\ncontent\n"
    );
}

/// `testreport_logs` lists the auxiliary checkout dirs; a file fetched back via
/// `testreport_read(relpath=…)` round-trips.
#[tokio::test]
async fn dispatch_logs_and_relpath_read() {
    let (session, tmp, log) = loaded("log\n").await;
    let checkout = log.parent().unwrap();
    std::fs::create_dir_all(checkout.join("install_logs")).unwrap();
    std::fs::write(
        checkout.join("install_logs/host1.log").as_path(),
        "x\ny\nz\n",
    )
    .unwrap();
    let _ = tmp; // keep alive

    let listed = call(&session, "testreport_logs", json!({})).await;
    let il = listed["install_logs"].as_array().unwrap();
    assert_eq!(il.len(), 1);
    assert_eq!(il[0]["name"], "host1.log");

    let read = call(
        &session,
        "testreport_read",
        json!({ "relpath": "install_logs/host1.log" }),
    )
    .await;
    assert_eq!(read["line_count"], 3);
    assert_eq!(read["content"], "x\ny\nz\n");
}

/// Full-schema golden: pins the five tool names, descriptions, input schemas,
/// and read-only hints so a token-budget or field-name regression is visible.
#[test]
fn testreport_tool_schemas_snapshot() {
    let descriptors = testreport_tool_descriptors();
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

/// The safe-path guard blocks traversal even via the dispatch seam.
#[tokio::test]
async fn dispatch_read_traversal_is_refused() {
    let (session, _tmp, _log) = loaded("log\n").await;
    let map: Map<String, Value> = json!({ "relpath": "../../etc/passwd" })
        .as_object()
        .cloned()
        .unwrap();
    let err = dispatch_testreport_tool(&session, "testreport_read", &map)
        .await
        .expect_err("traversal refused");
    assert!(err.stderr.contains("escapes"), "{err:?}");
}
