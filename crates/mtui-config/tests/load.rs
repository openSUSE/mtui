//! Integration tests for `mtui-config`.
//!
//! Ports the intent of upstream `tests/test_config.py`
//! (`test_mtuirc_fixture_parses_all_sections`) to the TOML format: a realistic
//! multi-section document round-trips into a fully-typed `Config` across all
//! three value kinds the parser knows (string, integer, boolean), plus tilde
//! expansion and `ssl_verify` coercion.

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

    // String values across multiple sections.
    assert_eq!(cfg.session_user, "qauser");
    assert_eq!(cfg.ssh_strict_host_key_checking, "warn");
    assert_eq!(cfg.bugzilla_url, "https://bugzilla.example.com");
    assert_eq!(cfg.reports_url, "https://qam.example.com/testreports");
    assert_eq!(cfg.refhosts_resolvers, "https,path");
    assert_eq!(cfg.svn_path, "svn+ssh://svn@svn.example/testreports");

    // Integer-typed options.
    assert_eq!(cfg.connection_timeout, 450);
    assert_eq!(cfg.refhosts_https_expiration, 3600);

    // Boolean-typed option.
    assert!(cfg.chdir_to_template_dir);

    // ssl_verify: a non-boolean string is treated as a CA bundle path.
    assert_eq!(
        cfg.ssl_verify,
        SslVerify::CaBundle(PathBuf::from("warn.example/ca.pem"))
    );

    // Tilde in a path option expands to $HOME.
    if let Some(base) = directories::BaseDirs::new() {
        assert_eq!(
            cfg.refhosts_path,
            base.home_dir().join("qam/refhosts.yml"),
            "refhosts.path should have its ~ expanded"
        );
    }

    // An option absent from the fixture keeps its upstream default.
    assert_eq!(cfg.fancy_reports_url, "https://qam.suse.de/reports");
}
