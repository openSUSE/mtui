//! Ported from upstream `tests/test_sl_report.py`.
//!
//! Covers the `SLTestReport` surface that lands in task nbv.4: `id`, `parser`,
//! `update_repos_parser` (all three dispatch branches), and `check_hash` (the
//! `1.1` fast path plus the Gitea match/mismatch compare via wiremock).
//!
//! Not covered here (deferred by design):
//! * `set_repo` — lands with the `SetRepo` impl in task nbv.fly.
//! * `list_update_commands` doer-rendering — awaits the `OperationGroup` seam;
//!   only the no-op stub is smoke-checked.

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_testreport::{HashCheck, SlReport, TestReport};
use mtui_types::RequestReviewID;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config() -> Config {
    Config::default()
}

fn rrid(s: &str) -> RequestReviewID {
    RequestReviewID::parse(s).expect("valid rrid")
}

/// Build an RRID then override its `maintenance_id`, mirroring the upstream
/// `_make_rrid_with_maint` helper (which patches the field directly).
fn rrid_with_maint(maint: &str) -> RequestReviewID {
    let mut r = rrid("SUSE:SLFO:99:7");
    r.maintenance_id = maint.to_string();
    r
}

#[test]
fn id_returns_rrid_string() {
    let mut r = SlReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:SLFO:1.1:7"));
    assert_eq!(r.id(), "SUSE:SLFO:1.1:7");
}

#[test]
fn id_empty_when_no_rrid() {
    let r = SlReport::new(config());
    assert_eq!(r.id(), "");
}

#[test]
fn parser_returns_hosts_and_json_keys() {
    let r = SlReport::new(config());
    let keys: std::collections::BTreeSet<_> = r.parser().into_keys().collect();
    assert_eq!(
        keys,
        ["hosts".to_string(), "json".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn update_repos_parser_uses_reporepoparse_when_repositories_set() {
    let mut r = SlReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:SLFO:1.1:7"));
    r.base_mut()
        .repositories
        .insert("https://example.com/SLES-15-x86_64/".to_string());
    r.base_mut().products = vec!["SLES 15 (x86_64)".to_string()];
    let out = r.update_repos_parser();
    // reporepoparse matched the repo by name-version-arch.
    assert_eq!(
        out.values().next().unwrap(),
        "https://example.com/SLES-15-x86_64/"
    );
}

#[test]
fn update_repos_parser_uses_slrepoparse_for_1_1() {
    let mut r = SlReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:SLFO:1.1:7"));
    r.base_mut().repository = "https://example.com".to_string();
    r.base_mut().products = vec!["SLES 15 (x86_64)".to_string()];
    let out = r.update_repos_parser();
    // slrepoparse builds the images/repo path.
    assert_eq!(
        out.values().next().unwrap(),
        "https://example.com/images/repo/SLES-15-x86_64/"
    );
}

#[test]
fn update_repos_parser_falls_back_to_gitrepoparse() {
    let mut r = SlReport::new(config());
    r.base_mut().rrid = Some(rrid_with_maint("2.0"));
    r.base_mut().repository = "https://example.com".to_string();
    r.base_mut().products = vec!["SLES 15 (x86_64)".to_string()];
    let out = r.update_repos_parser();
    assert_eq!(out.values().next().unwrap(), "https://example.com/standard");
}

#[tokio::test]
async fn check_hash_maintenance_id_1_1_bypasses_gitea() {
    let mut r = SlReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:SLFO:1.1:7"));
    assert_eq!(r.check_hash().await, HashCheck::Ok);
}

/// Mount a GET on the PR endpoint returning `{ "head": { "sha": <sha> } }`,
/// which `Gitea::get_hash` reads.
async fn mount_pr_head_sha(server: &MockServer, sha: &str) {
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/owner/repo/pulls/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "head": { "sha": sha } })),
        )
        .mount(server)
        .await;
}

fn config_with_gitea(server: &MockServer) -> Config {
    let mut cfg = Config::default();
    // `Gitea::new` rejects an empty token; a non-empty one lets the client build.
    cfg.gitea_token = "tok".to_string();
    // Trust the (loopback) mock origin so the token guard allows the request.
    cfg.gitea_url = server.uri();
    cfg
}

#[tokio::test]
async fn check_hash_gitea_compare_match() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "abc").await;

    let mut r = SlReport::new(config_with_gitea(&server));
    r.base_mut().rrid = Some(rrid_with_maint("2.0"));
    r.base_mut().giteacohash = Some("abc".to_string());
    r.base_mut().giteaprapi = Some(format!("{}/api/v1/repos/owner/repo/pulls/1", server.uri()));

    assert_eq!(r.check_hash().await, HashCheck::Ok);
}

#[tokio::test]
async fn check_hash_gitea_compare_mismatch() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "xyz").await;

    let mut r = SlReport::new(config_with_gitea(&server));
    r.base_mut().rrid = Some(rrid_with_maint("2.0"));
    r.base_mut().giteacohash = Some("abc".to_string());
    r.base_mut().giteaprapi = Some(format!("{}/api/v1/repos/owner/repo/pulls/1", server.uri()));

    assert_eq!(
        r.check_hash().await,
        HashCheck::Mismatch {
            expected: "abc".to_string(),
            actual: "xyz".to_string(),
        }
    );
}

#[test]
fn list_update_commands_is_a_noop_stub() {
    let r = SlReport::new(config());
    // Deferred doer-rendering: must not panic and must not require a doer seam.
    r.list_update_commands(&HostsGroup::new(Vec::new(), false));
}

// --- perform_install / perform_uninstall (OperationGroup seam) --------------

use std::collections::BTreeSet;
use std::sync::Arc;

use mtui_hosts::{Check, CheckArgs, Doer, HostError, MockConnection, PlanProvider, Target};
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::system::{System, SystemProduct};

/// A minimal provider yielding one fixed installer/uninstaller doer, so the
/// report's `perform_*` drives a real command through the group.
struct FixedProvider;

impl PlanProvider for FixedProvider {
    fn doer(&self, _role: &str, _release: &str, _transactional: bool) -> Result<Doer, HostError> {
        Ok(Doer::new(
            "zypper -n in -y -l $packages",
            "systemctl reboot",
        ))
    }
    fn check(&self, _role: &str, _release: &str, _transactional: bool) -> Check {
        Box::new(|_a: CheckArgs<'_>| {})
    }
}

fn sles_group() -> (HostsGroup, MockConnection) {
    let conn = MockConnection::new("h1").with_default(CommandLog::new("", "done", "", 0, 1));
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
    let group = HostsGroup::new(vec![t], false).with_plan_provider(Arc::new(FixedProvider));
    (group, handle)
}

#[tokio::test]
async fn perform_install_drives_installer_doer() {
    let r = SlReport::new(config());
    let (mut group, handle) = sles_group();

    let res = r
        .perform_install(&mut group, &["pkg-a".to_owned(), "pkg-b".to_owned()])
        .await;
    assert!(res.is_ok(), "a clean install returns Ok: {res:?}");

    assert_eq!(
        handle.commands(),
        vec!["zypper -n in -y -l pkg-a pkg-b".to_owned()]
    );
}

#[tokio::test]
async fn perform_uninstall_drives_uninstaller_doer() {
    let r = SlReport::new(config());
    let (mut group, handle) = sles_group();

    let res = r.perform_uninstall(&mut group, &["pkg".to_owned()]).await;
    assert!(res.is_ok(), "a clean uninstall returns Ok: {res:?}");

    assert_eq!(handle.commands(), vec!["zypper -n in -y -l pkg".to_owned()]);
}

// --- set_repo (SetRepo impl -> RepoManager::run_zypper) ---------------------

use mtui_hosts::{RepoOp, SetRepo};

/// Builds an enabled single target over a mock connection whose product matches
/// `sles`, returning the target plus a handle to inspect issued commands.
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

/// An SL report whose `update_repos` maps the host's product to one URL.
fn sl_with_repo() -> SlReport {
    let mut r = SlReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:SLFO:1:2"));
    r.base_mut().update_repos.insert(
        SystemProduct::new("SLES", "15.5", "x86_64"),
        "https://example/repo".to_owned(),
    );
    r
}

#[tokio::test]
async fn set_repo_add_uses_sl_ar_flags() {
    let r = sl_with_repo();
    let (mut t, handle) = sles_target();

    r.set_repo(&mut t, RepoOp::Add).await;

    let cmds = handle.commands();
    assert!(
        cmds.iter().any(|c| c.starts_with("zypper -n ar -cfGkn ")
            && c.contains("issue-SLES:15.5:p=1:2")
            && c.contains("https://example/repo")),
        "expected SL `zypper -n ar -cfGkn ... issue-...` add, got {cmds:?}"
    );
    assert_eq!(cmds.last().map(String::as_str), Some("zypper -n ref"));
}

#[tokio::test]
async fn set_repo_remove_uses_rr() {
    let r = sl_with_repo();
    let (mut t, handle) = sles_target();

    r.set_repo(&mut t, RepoOp::Remove).await;

    let cmds = handle.commands();
    assert!(
        cmds.iter()
            .any(|c| c == "zypper -n rr https://example/repo"),
        "expected `zypper -n rr <url>`, got {cmds:?}"
    );
}

#[tokio::test]
async fn set_repo_no_rrid_is_a_noop() {
    // No RRID loaded -> nothing to (un)register; must not touch the host.
    let mut r = SlReport::new(config());
    r.base_mut().update_repos.insert(
        SystemProduct::new("SLES", "15.5", "x86_64"),
        "https://example/repo".to_owned(),
    );
    let (mut t, handle) = sles_target();

    r.set_repo(&mut t, RepoOp::Add).await;

    assert!(
        handle.commands().is_empty(),
        "no-RRID set_repo must issue no commands"
    );
}
