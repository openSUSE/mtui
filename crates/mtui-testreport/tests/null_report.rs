//! Pins the null-object contract: falsy, empty ID, unset report path, empty
//! parser tables, `target_wd` rooted under `config.target_tempdir`, no-op
//! update-command listing, and a trivially-valid hash.

use std::path::PathBuf;

use mtui_config::options::Config;
use mtui_hosts::HostsGroup;
use mtui_testreport::{HashCheck, NullReport, TestReport};

/// Builds a config whose `template_dir`/`target_tempdir` point under a
/// unique, deterministic base.
fn config_with_tmp(base: &str) -> (Config, PathBuf) {
    let tmp = std::env::temp_dir().join(format!("mtui-null-report-{base}"));
    let target = tmp.join("target");
    let mut cfg = Config::default();
    cfg.template_dir = tmp;
    cfg.target_tempdir = target;
    let target = cfg.target_tempdir.clone();
    (cfg, target)
}

#[test]
fn null_bool_is_false() {
    let (cfg, _) = config_with_tmp("bool");
    assert!(!NullReport::new(cfg).is_loaded());
}

#[test]
fn null_id_empty() {
    let (cfg, _) = config_with_tmp("id");
    assert_eq!(NullReport::new(cfg).id(), "");
}

/// #524: the sentinel must carry no path at all. A `<cwd>/None` placeholder
/// made `report_wd()` resolve to the process cwd, so a dispatch that fell back
/// here acted on the cwd instead of refusing.
#[test]
fn null_path_is_unset() {
    let (cfg, _) = config_with_tmp("path");
    assert_eq!(NullReport::new(cfg).base().path, None);
}

/// The consequence the `"no report working directory"` `map_err`s all rely on:
/// catches a placeholder path whose parent happens to exist (the cwd always
/// does).
#[test]
fn null_report_wd_is_not_found() {
    let (cfg, _) = config_with_tmp("report-wd");
    let err = NullReport::new(cfg)
        .base()
        .report_wd()
        .expect_err("the null report has no working directory");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
}

#[test]
fn null_parser_returns_empty() {
    let (cfg, _) = config_with_tmp("parser");
    assert!(NullReport::new(cfg).parser().is_empty());
}

#[test]
fn null_update_repos_parser_returns_empty() {
    let (cfg, _) = config_with_tmp("update-repos");
    assert!(NullReport::new(cfg).update_repos_parser().is_empty());
}

#[test]
fn null_target_wd_returns_path_join() {
    let (cfg, target) = config_with_tmp("target-wd");
    let n = NullReport::new(cfg);
    assert_eq!(n.target_wd(&["a", "b"]), target.join("a").join("b"));
}

#[test]
fn null_list_update_commands_noop() {
    let (cfg, _) = config_with_tmp("list-update");
    let n = NullReport::new(cfg);
    // Must not panic and must not touch the (empty) host group.
    n.list_update_commands(&HostsGroup::new(Vec::new(), false));
}

#[tokio::test]
async fn null_check_hash_returns_true() {
    let (cfg, _) = config_with_tmp("check-hash");
    assert_eq!(NullReport::new(cfg).check_hash().await, HashCheck::Ok);
}
