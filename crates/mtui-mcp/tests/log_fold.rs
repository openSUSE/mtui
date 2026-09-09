//! Log folding for parallel fan-out outputs (STEP 2).
//!
//! Repetitive multi-host success spam folds to budget while verdicts and
//! errors survive, and the verdict stays at the head so `max_output_bytes`
//! truncation preserves it.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::{Registry, register_all};
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_mcp::McpSession;
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use tempfile::TempDir;

const RRID: &str = "SUSE:Maintenance:42:7";

fn spam_target(hostname: &str, stdout: &str) -> Target {
    let conn = MockConnection::new(hostname).with_default(CommandLog::new("", stdout, "", 0, 0));
    Target::with_connection(hostname, TargetState::Enabled, Box::new(conn))
}

async fn session_with_spam(budget: usize, stdout: &str) -> (Arc<McpSession>, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    config.mcp_max_output_bytes = budget;
    let session = McpSession::new(config);
    {
        let mut guard = session.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
        let targets = vec![spam_target("h1", stdout), spam_target("h2", stdout)];
        report.base_mut().targets = HostsGroup::new(targets, false);
        guard.templates.add(Box::new(report));
        guard.templates.set_active(RRID);
    }
    (session, tmp)
}

#[tokio::test]
async fn repetitive_success_folds_to_budget() {
    let spam = "spam\n".repeat(50);
    let (sess, _tmp) = session_with_spam(100_000, &spam).await;
    let registry: Registry = register_all();
    let out = sess
        .run_command(&registry, "run", &["true".to_owned()])
        .await
        .expect("run should succeed");
    assert!(
        out.contains("run completed on h1 (exit 0), h2 (exit 0)"),
        "{out}"
    );
    assert!(out.contains("identical"), "spam must fold: {out}");
    assert!(out.lines().count() < 15, "folded to budget: {out}");
    let snap = format!("folded run:\n{out}");
    insta::assert_snapshot!(snap);
}

#[tokio::test]
async fn verdict_stays_at_head_under_a_tiny_budget() {
    let spam = "spam\n".repeat(200);
    let (sess, _tmp) = session_with_spam(60, &spam).await;
    let registry: Registry = register_all();
    let out = sess
        .run_command(&registry, "run", &["true".to_owned()])
        .await
        .expect("run should succeed");
    let verdict = out.find("run completed on").expect("verdict kept: {out}");
    assert_eq!(verdict, 0, "verdict must stay at head: {out:?}");
    assert!(
        out.contains("truncated"),
        "over-budget tail is marked: {out:?}"
    );
    assert!(out.contains("max_output_bytes=60"), "{out:?}");
}
