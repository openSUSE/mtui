//! Ported from upstream `tests/test_obs_report.py`.
//!
//! Covers the `OBSTestReport` surface that lands in task nbv.11: `id`, `parser`,
//! `update_repos_parser` (dispatching to `obsrepoparse` over `report_wd()`), and
//! `check_hash` (the constant `(true, "", "")`).
//!
//! Not covered here (deferred by design, mirroring the `SlReport`/`PiReport`
//! boundary):
//! * `list_update_commands` doer-rendering — awaits the `OperationGroup` seam
//!   (upstream `test_obs_list_update_commands_invokes_display`); only the no-op
//!   stub is smoke-checked.
//! * `_show_yourself_data` — not on the trait skeleton yet
//!   (upstream `test_obs_show_yourself_data_includes_rrid_and_rating`).

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::{RequestReviewID, SystemProduct};

fn config() -> Config {
    Config::default()
}

fn rrid(s: &str) -> RequestReviewID {
    RequestReviewID::parse(s).expect("valid rrid")
}

/// Upstream `test_obs_id_returns_rrid_str`.
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

/// Upstream `test_obs_parser_returns_hosts_and_json`.
#[test]
fn parser_returns_hosts_and_json_keys() {
    let r = ObsReport::new(config());
    let keys: std::collections::BTreeSet<_> = r.parser().into_keys().collect();
    assert_eq!(
        keys,
        ["hosts".to_string(), "json".to_string()]
            .into_iter()
            .collect()
    );
}

/// OBS dispatches to `obsrepoparse`, reading `project.xml` from `report_wd()`
/// (the parent dir of the loaded report path). Points `base.path` into the
/// ported OBS fixture directory so `report_wd()` resolves there.
#[test]
fn update_repos_parser_parses_obs_project_xml() {
    let fixture_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/obs");
    let mut r = ObsReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:Maintenance:12358:199773"));
    r.base_mut().repository = "https://example.com".to_string();
    // report_wd() == path.parent(), so make the parent the fixture dir.
    r.base_mut().path = Some(std::path::Path::new(fixture_dir).join("log"));

    let out = r.update_repos_parser();
    let product = SystemProduct::new("SLES", "15", "x86_64");
    assert_eq!(out[&product], "https://example.com/SLE-15-x86_64");
}

/// When no report is loaded, `report_wd()` errors and `update_repos_parser`
/// degrades to an empty map (upstream asserts `self.path`; the Rust port
/// degrades gracefully like the sibling reports).
#[test]
fn update_repos_parser_empty_when_no_report_loaded() {
    let r = ObsReport::new(config());
    assert!(r.update_repos_parser().is_empty());
}

/// Upstream `test_obs_check_hash_always_true`.
#[tokio::test]
async fn check_hash_always_true() {
    let mut r = ObsReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:Maintenance:12358:199773"));
    assert_eq!(r.check_hash().await, (true, String::new(), String::new()));
}

/// Upstream `test_obs_list_update_commands_invokes_display` — the doer-rendering
/// is deferred, so only smoke-check the no-op stub does not panic.
#[test]
fn list_update_commands_is_a_noop_stub() {
    let r = ObsReport::new(config());
    r.list_update_commands(&HostsGroup::new(Vec::new(), false));
}

// --- set_repo (SetRepo impl -> RepoManager::run_zypper) ---------------------

use std::collections::BTreeSet;

use mtui_hosts::{MockConnection, RepoOp, SetRepo, Target};
use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::system::System;

/// An enabled single target whose product matches the seeded repo.
fn sles_target() -> (Target, MockConnection) {
    let conn = MockConnection::new("h1");
    let handle = conn.clone();
    let mut t = Target::with_connection(
        "h1",
        TargetState::Enabled,
        ExecutionMode::Parallel,
        Box::new(conn),
    );
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
