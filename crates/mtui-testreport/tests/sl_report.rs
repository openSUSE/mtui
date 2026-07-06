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
use mtui_testreport::{SlReport, TestReport};
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
    assert_eq!(r.check_hash().await, (true, String::new(), String::new()));
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

fn config_with_gitea() -> Config {
    let mut cfg = Config::default();
    // `Gitea::new` rejects an empty token; a non-empty one lets the client build.
    cfg.gitea_token = "tok".to_string();
    cfg
}

#[tokio::test]
async fn check_hash_gitea_compare_match() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "abc").await;

    let mut r = SlReport::new(config_with_gitea());
    r.base_mut().rrid = Some(rrid_with_maint("2.0"));
    r.base_mut().giteacohash = Some("abc".to_string());
    r.base_mut().giteaprapi = Some(format!("{}/api/v1/repos/owner/repo/pulls/1", server.uri()));

    let (ok, old, new) = r.check_hash().await;
    assert!(ok);
    assert_eq!(old, "abc");
    assert_eq!(new, "abc");
}

#[tokio::test]
async fn check_hash_gitea_compare_mismatch() {
    let server = MockServer::start().await;
    mount_pr_head_sha(&server, "xyz").await;

    let mut r = SlReport::new(config_with_gitea());
    r.base_mut().rrid = Some(rrid_with_maint("2.0"));
    r.base_mut().giteacohash = Some("abc".to_string());
    r.base_mut().giteaprapi = Some(format!("{}/api/v1/repos/owner/repo/pulls/1", server.uri()));

    let (ok, old, new) = r.check_hash().await;
    assert!(!ok);
    assert_eq!(old, "abc");
    assert_eq!(new, "xyz");
}

#[test]
fn list_update_commands_is_a_noop_stub() {
    let r = SlReport::new(config());
    // Deferred doer-rendering: must not panic and must not require a doer seam.
    r.list_update_commands(&HostsGroup::new(Vec::new(), false));
}
