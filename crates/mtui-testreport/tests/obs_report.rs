//! Covers the `ObsReport` surface: `id`, `update_repos_parser` (from the report
//! document), `check_hash` (constant) and `set_repo`.
//!
//! `list_update_commands` doer-rendering awaits the `OperationGroup` seam; only
//! the no-op stub is smoke-checked.

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_testreport::{HashCheck, ObsReport, TestReport};
use mtui_types::{RequestReviewID, SystemProduct};

fn config() -> Config {
    Config::default()
}

fn rrid(s: &str) -> RequestReviewID {
    RequestReviewID::parse(s).expect("valid rrid")
}

#[test]
fn id_returns_rrid_string() {
    let mut r = ObsReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:Maintenance:12358:199773"));
    assert_eq!(r.id(), "SUSE:Maintenance:12358:199773");
}

#[test]
fn id_empty_when_no_rrid() {
    let r = ObsReport::new(config());
    assert_eq!(r.id(), "");
}

/// With a document loaded the update repos come from `install.targets[]`.
#[test]
fn update_repos_parser_reads_the_document() {
    let dir = tempfile::tempdir().unwrap();
    let doc: mtui_types::report_document::ReportDocument =
        include_str!("../../mtui-types/tests/fixtures/document/maintenance_addon.json")
            .parse()
            .unwrap();
    let mut r = ObsReport::new(config());
    r.base_mut().path = Some(dir.path().join("log"));
    r.base_mut().document = Some(doc);

    let out = r.update_repos_parser();
    assert_eq!(out.len(), 1);
    assert!(out.values().all(|u| u.contains("SUSE_Updates_")));
}

/// With no document `update_repos_parser` degrades to an empty map, and never
/// reads a `project.xml` from the report directory.
#[test]
fn update_repos_parser_empty_without_a_document() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("project.xml"),
        r#"<project><repository name="SUSE_Updates_SLES_15_x86_64">
             <path project="p" repository="update"/>
             <releasetarget project="SUSE:SLE-15:Update:x86_64" repository="standard"/>
           </repository></project>"#,
    )
    .unwrap();
    let mut r = ObsReport::new(config());
    r.base_mut().path = Some(dir.path().join("log"));
    assert!(r.base().document.is_none());
    assert!(r.update_repos_parser().is_empty());
}

#[tokio::test]
async fn check_hash_always_true() {
    let mut r = ObsReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:Maintenance:12358:199773"));
    assert_eq!(r.check_hash().await, HashCheck::Ok);
}

/// The doer-rendering is deferred; smoke-check the stub does not panic.
#[test]
fn list_update_commands_is_a_noop_stub() {
    let r = ObsReport::new(config());
    r.list_update_commands(&HostsGroup::new(Vec::new(), false));
}

// --- set_repo (SetRepo impl -> RepoManager::run_zypper) ---------------------

use std::collections::BTreeSet;

use mtui_hosts::{MockConnection, RepoOp, SetRepo, Target};
use mtui_types::enums::TargetState;
use mtui_types::system::System;

/// An enabled single target whose product matches the seeded repo.
fn sles_target() -> (Target, MockConnection) {
    let conn = MockConnection::new("h1");
    let handle = conn.clone();
    let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
    t.set_system(
        System::new(
            SystemProduct::new("SLES", "15.5", "x86_64"),
            BTreeSet::new(),
            false,
        ),
        false,
    );
    (t, handle)
}

fn obs_with_repo() -> ObsReport {
    let mut r = ObsReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:Maintenance:1:2"));
    r.base_mut().update_repos.insert(
        SystemProduct::new("SLES", "15.5", "x86_64"),
        "https://example/repo".to_owned(),
    );
    r
}

#[tokio::test]
async fn set_repo_add_uses_obs_specific_ar_flags() {
    let r = obs_with_repo();
    let (mut t, handle) = sles_target();

    r.set_repo(&mut t, RepoOp::Add).await;

    let cmds = handle.commands();
    // OBS uses `-n ar -ckn` (no `fG`), distinct from SL/PI's `-n ar -cfGkn`.
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("zypper -n ar -ckn ") && c.contains("issue-SLES:15.5:p=1:2")),
        "expected OBS `zypper -n ar -ckn ...` add, got {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("-cfGkn")),
        "OBS must NOT use SL/PI's -cfGkn flags, got {cmds:?}"
    );
    assert_eq!(cmds.last().map(String::as_str), Some("zypper -n ref"));
}

#[tokio::test]
async fn set_repo_remove_uses_rr() {
    let r = obs_with_repo();
    let (mut t, handle) = sles_target();

    r.set_repo(&mut t, RepoOp::Remove).await;

    assert!(
        handle
            .commands()
            .iter()
            .any(|c| c == "zypper -n rr https://example/repo"),
        "expected `zypper -n rr <url>`, got {:?}",
        handle.commands()
    );
}
