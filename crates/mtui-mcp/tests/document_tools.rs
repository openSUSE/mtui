//! Integration test for the `report_*` document tools.
//!
//! Drives the public `dispatch_document_tool` seam over a session holding a
//! report whose document comes from the `mtui-types` fixtures, plus a
//! full-schema snapshot of the five descriptors.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_mcp::{McpCommandError, McpSession, dispatch_document_tool, document_tool_descriptors};
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::report_document::ReportDocument;
use serde_json::{Map, Value, json};

const RRID: &str = "SUSE:Maintenance:1:1";
const OTHER: &str = "SUSE:Maintenance:2:2";
const MAINTENANCE_OBS: &str =
    include_str!("../../mtui-types/tests/fixtures/document/maintenance_obs.json");
const PI: &str = include_str!("../../mtui-types/tests/fixtures/document/pi.json");

fn report(session: &mtui_core::Session, rrid: &str, document: Option<&str>) -> ObsReport {
    let mut report = ObsReport::new(session.config.clone());
    report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
    report.base_mut().document = document.map(|raw| raw.parse::<ReportDocument>().unwrap());
    report
}

/// A session with one active report holding `document`.
async fn loaded(document: Option<&str>) -> Arc<McpSession> {
    let session = McpSession::new(Config::default());
    {
        let mut guard = session.session().lock().await;
        let report = report(&guard, RRID, document);
        guard.templates.add(Box::new(report));
        guard.templates.set_active(RRID);
    }
    session
}

async fn try_call(
    session: &McpSession,
    name: &str,
    kwargs: Value,
) -> Result<Value, McpCommandError> {
    let map: Map<String, Value> = kwargs.as_object().cloned().unwrap_or_default();
    dispatch_document_tool(session, name, &map, None).await
}

async fn call(session: &McpSession, name: &str, kwargs: Value) -> Value {
    try_call(session, name, kwargs)
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"))
}

async fn refusal(session: &McpSession, name: &str, kwargs: Value) -> String {
    match try_call(session, name, kwargs).await {
        Ok(v) => panic!("{name} should have been refused, got {v}"),
        Err(e) => e.stderr,
    }
}

/// The serialised document and dirty flag of the report under `RRID`.
async fn state(session: &McpSession, rrid: &str) -> (String, bool) {
    let guard = session.session().lock().await;
    guard
        .with_report(rrid, |r| {
            let base = r.base();
            (
                serde_json::to_string(&base.document).unwrap(),
                base.document_dirty,
            )
        })
        .expect("report reachable")
}

#[tokio::test]
async fn a_section_write_reads_back_and_marks_the_report_dirty() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let before = call(&session, "report_sections", json!({})).await;
    assert_eq!(before["dirty"], false);

    let value = json!({"regression": {"verdict": "PASSED", "comment": "all green"}});
    let written = call(
        &session,
        "report_section_write",
        json!({"section": "testing", "value": value}),
    )
    .await;
    assert_eq!(written["section"], "testing");
    assert_eq!(written["dirty"], true);
    assert_eq!(written["size"], value.to_string().len());

    let read = call(
        &session,
        "report_section_read",
        json!({"section": "testing"}),
    )
    .await;
    assert_eq!(read["data"], value);
    assert_eq!(read["section"], "testing");

    let after = call(&session, "report_sections", json!({})).await;
    assert_eq!(after["dirty"], true);
    assert!(state(&session, RRID).await.1, "the base flag is set too");
}

#[tokio::test]
async fn a_rejected_write_leaves_the_document_untouched_and_clean() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let (document, _) = state(&session, RRID).await;

    let msg = refusal(
        &session,
        "report_section_write",
        json!({"section": "verdict", "value": "MAYBE"}),
    )
    .await;
    assert!(msg.contains("/verdict"), "{msg}");

    let mut people = call(
        &session,
        "report_section_read",
        json!({"section": "people"}),
    )
    .await["data"]
        .clone();
    people["reviewer"]["bogus"] = json!(1);
    let msg = refusal(
        &session,
        "report_section_write",
        json!({"section": "people", "value": people}),
    )
    .await;
    assert!(msg.contains("/people/reviewer/bogus"), "{msg}");

    assert_eq!(state(&session, RRID).await, (document, false));
}

#[tokio::test]
async fn a_pipeline_owned_section_write_is_refused() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let (document, _) = state(&session, RRID).await;
    let current = call(
        &session,
        "report_section_read",
        json!({"section": "update"}),
    )
    .await["data"]
        .clone();

    let msg = refusal(
        &session,
        "report_section_write",
        json!({"section": "update", "value": current}),
    )
    .await;

    assert!(msg.contains("read-only"), "{msg}");
    assert_eq!(state(&session, RRID).await, (document, false));
}

#[tokio::test]
async fn a_section_the_report_lacks_reads_and_writes_as_a_refusal() {
    let session = loaded(Some(PI)).await;
    let names: Vec<String> = call(&session, "report_sections", json!({})).await["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(!names.contains(&"review".to_owned()), "{names:?}");

    let msg = refusal(
        &session,
        "report_section_read",
        json!({"section": "review"}),
    )
    .await;
    assert!(msg.contains("not present"), "{msg}");
    let msg = refusal(
        &session,
        "report_section_write",
        json!({"section": "review", "value": {}}),
    )
    .await;
    assert!(msg.contains("not present"), "{msg}");
}

#[tokio::test]
async fn sections_report_completeness_with_a_pointer_per_null_leaf() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let out = call(&session, "report_sections", json!({})).await;

    assert_eq!(out["complete"], false);
    let unfilled: Vec<&str> = out["unfilled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(unfilled.contains(&"/verdict"), "{unfilled:?}");
    assert!(unfilled.contains(&"/people/reviewer/name"), "{unfilled:?}");
    assert_eq!(out["sections"].as_array().unwrap().len(), 8);
}

#[tokio::test]
async fn a_report_without_a_document_refuses_every_tool() {
    let session = loaded(None).await;
    for (name, kwargs) in [
        ("report_sections", json!({})),
        ("report_section_read", json!({"section": "verdict"})),
        (
            "report_section_write",
            json!({"section": "verdict", "value": null}),
        ),
        ("report_issue_read", json!({})),
        (
            "report_issue_write",
            json!({"issue_id": "bsc#1", "value": {}}),
        ),
    ] {
        let msg = refusal(&session, name, kwargs).await;
        assert!(msg.contains("SVN-path"), "{name}: {msg}");
        assert!(msg.contains("testreport_"), "{name}: {msg}");
    }
}

#[tokio::test]
async fn more_than_one_loaded_template_needs_a_template_argument() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    {
        let mut guard = session.session().lock().await;
        let second = report(&guard, OTHER, Some(PI));
        guard.templates.add(Box::new(second));
    }

    let msg = refusal(&session, "report_sections", json!({})).await;
    assert!(msg.contains("more than one template is loaded"), "{msg}");

    let out = call(&session, "report_sections", json!({"template": OTHER})).await;
    assert_eq!(out["id"], serde_json::from_str::<Value>(PI).unwrap()["id"]);
}

#[tokio::test]
async fn a_template_held_elsewhere_is_busy() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let _held = {
        let guard = session.session().lock().await;
        guard
            .templates
            .handle(RRID)
            .unwrap()
            .try_lock_owned()
            .unwrap()
    };

    let msg = refusal(&session, "report_sections", json!({"template": RRID})).await;
    assert_eq!(msg, format!("template busy: {RRID}"));
}

#[tokio::test]
async fn the_issue_index_and_a_single_issue_read() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;

    let index = call(&session, "report_issue_read", json!({})).await;
    let rows = index["issues"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "bnc#1276308");
    assert_eq!(rows[0]["severity"], "major");
    assert_eq!(rows[0]["status"], Value::Null);
    assert!(rows[0]["title"].is_string());

    let one = call(
        &session,
        "report_issue_read",
        json!({"issue_id": "bnc#1276308"}),
    )
    .await;
    assert_eq!(one["issue_id"], "bnc#1276308");
    assert_eq!(one["issue"]["title"], rows[0]["title"]);

    let msg = refusal(&session, "report_issue_read", json!({"issue_id": "bsc#1"})).await;
    assert!(msg.contains("no issue"), "{msg}");
}

#[tokio::test]
async fn an_issue_write_changes_one_entry_and_marks_dirty() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let mut issue = call(
        &session,
        "report_issue_read",
        json!({"issue_id": "bnc#1276308"}),
    )
    .await["issue"]
        .clone();
    issue["status"] = json!("FIXED");
    issue["reproducer"] = json!(true);

    let written = call(
        &session,
        "report_issue_write",
        json!({"issue_id": "bnc#1276308", "value": issue}),
    )
    .await;
    assert_eq!(written["dirty"], true);

    let back = call(
        &session,
        "report_issue_read",
        json!({"issue_id": "bnc#1276308"}),
    )
    .await;
    assert_eq!(back["issue"]["status"], "FIXED");
    assert!(state(&session, RRID).await.1);
}

#[tokio::test]
async fn an_issue_write_to_an_unknown_issue_is_refused_and_clean() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let (document, _) = state(&session, RRID).await;

    let msg = refusal(
        &session,
        "report_issue_write",
        json!({"issue_id": "bsc#999", "value": {}}),
    )
    .await;

    assert!(msg.contains("no issue"), "{msg}");
    assert_eq!(state(&session, RRID).await, (document, false));
}

#[tokio::test]
async fn an_issues_section_write_cannot_change_the_key_set() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    let (document, _) = state(&session, RRID).await;

    let msg = refusal(
        &session,
        "report_section_write",
        json!({"section": "issues", "value": {}}),
    )
    .await;

    assert!(msg.contains("bnc#1276308"), "{msg}");
    assert_eq!(state(&session, RRID).await, (document, false));
}

#[tokio::test]
async fn an_unanswered_value_can_be_written_as_null() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;
    call(
        &session,
        "report_section_write",
        json!({"section": "verdict", "value": "PASSED"}),
    )
    .await;
    call(
        &session,
        "report_section_write",
        json!({"section": "verdict", "value": null}),
    )
    .await;

    let read = call(
        &session,
        "report_section_read",
        json!({"section": "verdict"}),
    )
    .await;
    assert_eq!(read["data"], Value::Null);
}

#[tokio::test]
async fn bad_arguments_are_refused() {
    let session = loaded(Some(MAINTENANCE_OBS)).await;

    let msg = refusal(&session, "report_sections", json!({"bogus": 1})).await;
    assert!(msg.contains("bogus"), "{msg}");

    let msg = refusal(
        &session,
        "report_section_read",
        json!({"section": "Verdict"}),
    )
    .await;
    assert!(msg.contains("valid sections"), "{msg}");

    let msg = refusal(&session, "report_section_read", json!({})).await;
    assert!(msg.contains("`section` is required"), "{msg}");

    let msg = refusal(
        &session,
        "report_section_write",
        json!({"section": "verdict"}),
    )
    .await;
    assert!(msg.contains("`value` is required"), "{msg}");

    let msg = refusal(&session, "report_issue_write", json!({"value": {}})).await;
    assert!(msg.contains("`issue_id` is required"), "{msg}");

    let msg = refusal(&session, "report_nope", json!({})).await;
    assert!(msg.contains("unknown document tool"), "{msg}");
}

#[tokio::test]
async fn an_over_budget_result_is_flagged_not_clipped_into_invalid_json() {
    let mut config = Config::default();
    config.mcp_max_output_bytes = 200;
    let session = McpSession::new(config);
    {
        let mut guard = session.session().lock().await;
        let report = report(&guard, RRID, Some(MAINTENANCE_OBS));
        guard.templates.add(Box::new(report));
        guard.templates.set_active(RRID);
    }

    let out = call(
        &session,
        "report_section_read",
        json!({"section": "update"}),
    )
    .await;

    assert_eq!(out["truncated"], true);
    assert!(out["size"].as_u64().unwrap() > 200);
    assert!(out["content"].as_str().unwrap().contains("truncated"));
}

/// Full-schema golden: pins the five tool names, descriptions, input schemas
/// and read-only hints.
#[test]
fn document_tool_schemas_snapshot() {
    let rendered: Vec<Value> = document_tool_descriptors()
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
