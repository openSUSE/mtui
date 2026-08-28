//! Integration tests for `mtui-config`.
//!
//! A realistic multi-section document round-trips into a fully-typed `Config`
//! across all three value kinds the parser knows (string, integer, boolean),
//! plus tilde expansion and `ssl_verify` coercion.

use std::path::{Path, PathBuf};

use mtui_config::{Config, SslVerify};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn config_toml_fixture_parses_all_sections() {
    let path = fixture("config.toml");
    assert!(path.is_file(), "fixture missing: {}", path.display());

    let cfg = Config::load(Some(path));

    assert_eq!(cfg.session_user, "qauser");
    assert_eq!(cfg.ssh_strict_host_key_checking, "warn");
    assert_eq!(cfg.bugzilla_url, "https://bugzilla.example.com");
    assert_eq!(cfg.reports_url, "https://qam.example.com/testreports");
    assert_eq!(cfg.refhosts_resolvers, "https,path");
    assert_eq!(cfg.svn_path, "svn+ssh://svn@svn.example/testreports");

    assert_eq!(cfg.connection_timeout, 450);
    assert_eq!(cfg.reboot_timeout, 25);
    assert_eq!(cfg.reboot_retries, 10); // omitted -> default
    assert_eq!(cfg.max_parallel, 8);
    assert_eq!(cfg.max_oqa_parallel, 4);
    assert_eq!(cfg.refhosts_https_expiration, 3600);

    // ssl_verify: a non-boolean string is treated as a CA bundle path.
    assert_eq!(
        cfg.ssl_verify,
        SslVerify::CaBundle(PathBuf::from("warn.example/ca.pem"))
    );

    if let Some(base) = directories::BaseDirs::new() {
        assert_eq!(
            cfg.refhosts_path,
            base.home_dir().join("qam/refhosts.yml"),
            "refhosts.path should have its ~ expanded"
        );
    }

    // [lock]
    assert!(!cfg.lock_reap_stale);
    assert_eq!(cfg.lock_stale_age, 3600);
    assert_eq!(cfg.pool_stale_age, 7200);
    assert!(cfg.pool_reap_stale); // omitted -> default (true)
    assert_eq!(cfg.lock_wait, 30);
    assert_eq!(cfg.lock_wait_poll, 15); // omitted -> default

    // [mcp]
    assert_eq!(cfg.mcp_max_output_bytes, 65536);
    assert_eq!(cfg.mcp_profile, "core");
    assert_eq!(cfg.mcp_tools_allow, vec!["whoami".to_owned()]);
    assert_eq!(cfg.mcp_tools_deny, vec!["run".to_owned()]);

    // [slack]
    assert!(cfg.slack_enabled);
    assert_eq!(cfg.slack_token, "xoxb-fixture");
    assert_eq!(cfg.slack_channel, "#qam-review");
    assert_eq!(cfg.slack_poll_interval, 90);
    assert_eq!(cfg.slack_watch_timeout, 3600);

    // [obs]
    assert_eq!(cfg.obs_api_url, "https://api.example.de");
    assert_eq!(cfg.obs_request_timeout, 90);

    assert_eq!(cfg.fancy_reports_url, "https://qam.suse.de/reports");
}

/// An `mtui.toml` still carrying the retired `[mtui] tempdir` key must keep
/// loading — and applying its other keys — rather than erroring on the
/// now-unknown field.
#[test]
fn retired_mtui_tempdir_key_is_ignored_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mtui.toml");
    std::fs::write(
        &path,
        "[mtui]\ntempdir = \"/scratch\"\nuser = \"leniencyuser\"\n",
    )
    .unwrap();

    let cfg = Config::load(Some(path));

    assert_eq!(cfg.session_user, "leniencyuser");
}
