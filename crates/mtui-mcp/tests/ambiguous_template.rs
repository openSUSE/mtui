//! Anti-drift test for #575: the three surfaces that resolve "which template
//! does this call address" — the core fan-out driver's `Scope::Explicit` path,
//! the `get`/`put` transfer tools, and the `testreport_*` tools — must render
//! the byte-identical ambiguity stem when several templates are loaded and none
//! is named. All three ultimately call
//! [`mtui_core::ambiguous_template_message`]; this test is what would catch one
//! of them re-hand-rolling its own wording instead.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::{Registry, ambiguous_template_message, register_all};
use mtui_mcp::{McpSession, dispatch_testreport_tool, dispatch_transfer_tool};
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use serde_json::{Map, json};

const RRID_A: &str = "SUSE:Maintenance:1:1";
const RRID_B: &str = "SUSE:Maintenance:2:2";

/// A session with two loaded templates, neither active pointer addressable
/// from outside (headless, like every real MCP session).
async fn two_loaded() -> Arc<McpSession> {
    let session = McpSession::new(Config::default());
    let mut guard = session.session().lock().await;
    for rrid in [RRID_A, RRID_B] {
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        guard.templates.add(Box::new(report));
    }
    drop(guard);
    session
}

/// The stem every surface's refusal must contain verbatim.
fn expected_stem() -> String {
    let loaded = vec![RRID_A.to_owned(), RRID_B.to_owned()];
    // Only the stem — "more than one template is loaded (...)" — is shared;
    // each surface's own remedy differs by design (#575 decision 5), so this
    // helper strips the remedy off `ambiguous_template_message`'s output.
    let full = ambiguous_template_message(&loaded, "REMEDY");
    full.split("; REMEDY").next().unwrap().to_owned()
}

#[tokio::test]
async fn core_dispatch_explicit_scope_matches_the_shared_stem() {
    let session = two_loaded().await;
    let registry: Registry = register_all();

    let err = session
        .run_command(&registry, "update", &[])
        .await
        .expect_err("ambiguous headless dispatch must refuse");
    assert!(
        err.stderr.contains(&expected_stem()),
        "core dispatch stderr must contain the shared stem: {}",
        err.stderr
    );
}

#[tokio::test]
async fn transfer_tool_get_matches_the_shared_stem() {
    let session = two_loaded().await;
    let mut kwargs = Map::new();
    kwargs.insert("remote".to_owned(), json!("/etc/hostname"));

    let err = dispatch_transfer_tool(&session, "get", &kwargs, None)
        .await
        .expect_err("ambiguous get must refuse");
    assert!(
        err.stderr.contains(&expected_stem()),
        "get stderr must contain the shared stem: {}",
        err.stderr
    );
}

#[tokio::test]
async fn testreport_read_matches_the_shared_stem() {
    let session = two_loaded().await;

    let err = dispatch_testreport_tool(&session, "testreport_read", &Map::new(), None)
        .await
        .expect_err("ambiguous testreport_read must refuse");
    assert!(
        err.stderr.contains(&expected_stem()),
        "testreport_read stderr must contain the shared stem: {}",
        err.stderr
    );
}
