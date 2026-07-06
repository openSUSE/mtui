//! Ported from upstream `tests/test_pi_report.py`.
//!
//! Covers the `PITestReport` surface that lands in task nbv.12: `id`, `parser`,
//! `update_repos_parser` (delegating to `reporepoparse`), and `check_hash` (the
//! constant `(true, "", "")`).
//!
//! Not covered here (deferred by design, mirroring the `SlReport` boundary):
//! * `set_repo` — lands with the `SetRepo` impl in task nbv.fly
//!   (upstream `test_pi_set_repo_add_uses_ar_form` / `_remove` / `_unknown_raises`).
//! * `list_update_commands` doer-rendering — awaits the `OperationGroup` seam
//!   (upstream `test_pi_list_update_commands_invokes_display`); only the no-op
//!   stub is smoke-checked.
//! * `_show_yourself_data` — not on the trait skeleton yet
//!   (upstream `test_pi_show_yourself_data_includes_repo_rows`).

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_testreport::{PiReport, TestReport};
use mtui_types::RequestReviewID;

fn config() -> Config {
    Config::default()
}

fn rrid(s: &str) -> RequestReviewID {
    RequestReviewID::parse(s).expect("valid rrid")
}

/// Upstream `test_pi_id_returns_rrid_str`.
#[test]
fn id_returns_rrid_string() {
    let mut r = PiReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:PI:42:99"));
    assert_eq!(r.id(), "SUSE:PI:42:99");
}

#[test]
fn id_empty_when_no_rrid() {
    let r = PiReport::new(config());
    assert_eq!(r.id(), "");
}

/// Upstream `test_pi_parser_returns_hosts_and_json`.
#[test]
fn parser_returns_hosts_and_json_keys() {
    let r = PiReport::new(config());
    let keys: std::collections::BTreeSet<_> = r.parser().into_keys().collect();
    assert_eq!(
        keys,
        ["hosts".to_string(), "json".to_string()]
            .into_iter()
            .collect()
    );
}

/// PI dispatches unconditionally to `reporepoparse` (no maintenance-id branch).
#[test]
fn update_repos_parser_uses_reporepoparse() {
    let mut r = PiReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:PI:42:99"));
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
fn update_repos_parser_empty_when_no_repositories() {
    let mut r = PiReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:PI:42:99"));
    r.base_mut().products = vec!["SLES 15 (x86_64)".to_string()];
    assert!(r.update_repos_parser().is_empty());
}

/// Upstream `test_pi_check_hash_always_true`.
#[tokio::test]
async fn check_hash_always_true() {
    let mut r = PiReport::new(config());
    r.base_mut().rrid = Some(rrid("SUSE:PI:42:99"));
    assert_eq!(r.check_hash().await, (true, String::new(), String::new()));
}

#[test]
fn list_update_commands_is_a_noop_stub() {
    let r = PiReport::new(config());
    // Deferred doer-rendering: must not panic and must not require a doer seam.
    r.list_update_commands(&HostsGroup::new(Vec::new(), false));
}
