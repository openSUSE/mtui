//! `mtui-config` — TOML config parsing and XDG path resolution for mtui-rs.
//!
//! ## What this crate does
//!
//! Loads mtui's configuration from a **TOML** file named `mtui.toml`, resolved
//! from (highest precedence first): the `--config` flag, then `$MTUI_CONF`, then
//! the XDG per-user file (`$XDG_CONFIG_HOME/mtui/mtui.toml`), then the home
//! dotfile (`~/.mtui.toml`), then `/etc/mtui.toml`. Missing keys fall back to
//! defaults that match upstream mtui exactly.
//!
//! ## Intentional deviation from upstream
//!
//! Upstream mtui reads **INI** (`configparser`) from `/etc/mtui.cfg` and
//! `~/.mtuirc`. mtui-rs deliberately adopts **TOML** with XDG paths — a cleaner,
//! typed, modern format — as this is a redesign, not a 1:1 port. The
//! *behavioural* contract is preserved: sectioned options, upstream default
//! values, and **lenient loading** (a bad or missing file is logged and skipped,
//! never fatal).
//!
//! ## Scope (Phase 1)
//!
//! Only the Phase-1-relevant option subset is modelled here (paths, connection
//! timeout, refhosts, URLs, svn, target). Later phases add their own sections
//! (`[lock]`, `[openqa]`, `[mcp]`, ...) additively. CLI-argument merging
//! (`merge_args`) is deferred to Phase 6, where the `clap` args struct exists.

pub mod atomic;
pub mod error;
pub mod options;
pub mod paths;

use std::path::{Path, PathBuf};

pub use error::ConfigError;
pub use options::{Config, SslVerify};
pub use paths::{
    cache_dir, config_search_paths, data_dir, home_config_file, terms_path, xdg_config_file,
};

use options::RawConfig;

impl Config {
    /// Load configuration from the resolved search paths.
    ///
    /// `explicit` is the optional `--config` path. Files are merged
    /// **lowest-precedence first** (see [`config_search_paths`]): `/etc` →
    /// `~/.mtui.toml` → the XDG file, so a per-user file overrides `/etc` on
    /// shared keys and the XDG file wins over the home dotfile. A file that does not exist is
    /// silently skipped; a file that fails to read or parse is logged at ERROR
    /// and skipped — loading never hard-fails. Absent options take their
    /// upstream defaults.
    #[must_use]
    pub fn load(explicit: Option<PathBuf>) -> Self {
        let mut merged = RawConfig::default();
        for path in config_search_paths(explicit) {
            match read_file(&path) {
                Ok(Some(raw)) => merged.merge(raw),
                Ok(None) => { /* absent file: not an error */ }
                Err(err) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %err,
                        "ignoring config file that failed to load; using defaults for its options"
                    );
                }
            }
        }
        Config::from_raw(merged)
    }
}

/// Read and parse a single config file.
///
/// Returns:
/// * `Ok(Some(raw))` — the file existed and parsed cleanly.
/// * `Ok(None)` — the file does not exist (a normal, non-error condition).
/// * `Err(_)` — the file existed but could not be read or was invalid TOML.
fn read_file(path: &Path) -> Result<Option<RawConfig>, ConfigError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let raw = toml::from_str::<RawConfig>(&contents).map_err(|source| ConfigError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("mtui-config-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn load_missing_file_yields_defaults() {
        let path = std::env::temp_dir().join("mtui-config-does-not-exist.toml");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_reads_values_from_explicit_file() {
        let path = write_tmp(
            "explicit.toml",
            "[connection]\nconnection_timeout = 600\n\n[url]\nbugzilla = \"https://bz.example\"\n",
        );
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.connection_timeout, 600);
        assert_eq!(cfg.bugzilla_url, "https://bz.example");
        // Unset option keeps its default.
        assert_eq!(cfg.reports_url, "https://qam.suse.de/testreports");
    }

    #[test]
    fn reboot_backoff_options_default_to_upstream() {
        let d = Config::default();
        assert_eq!(d.reboot_timeout, 10);
        assert_eq!(d.reboot_retries, 10);
    }

    #[test]
    fn load_reads_reboot_backoff_options() {
        let path = write_tmp(
            "reboot.toml",
            "[connection]\nreboot_timeout = 20\nreboot_retries = 5\n",
        );
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.reboot_timeout, 20);
        assert_eq!(cfg.reboot_retries, 5);
    }

    #[test]
    fn mcp_max_output_bytes_defaults_to_100k() {
        assert_eq!(Config::default().mcp_max_output_bytes, 100_000);
    }

    #[test]
    fn load_reads_mcp_max_output_bytes() {
        let path = write_tmp("mcp.toml", "[mcp]\nmax_output_bytes = 4096\n");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_max_output_bytes, 4096);
    }

    #[test]
    fn mcp_max_input_bytes_defaults_to_10m() {
        assert_eq!(Config::default().mcp_max_input_bytes, 10_000_000);
    }

    #[test]
    fn load_reads_mcp_max_input_bytes() {
        let path = write_tmp("mcp_in.toml", "[mcp]\nmax_input_bytes = 8192\n");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_max_input_bytes, 8192);
    }

    #[test]
    fn mcp_max_request_bytes_defaults_to_10m() {
        assert_eq!(Config::default().mcp_max_request_bytes, 10_000_000);
    }

    #[test]
    fn load_reads_mcp_max_request_bytes() {
        let path = write_tmp("mcp_req.toml", "[mcp]\nmax_request_bytes = 8192\n");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_max_request_bytes, 8192);
    }

    #[test]
    fn mcp_session_bounds_default_to_upstream() {
        let d = Config::default();
        assert_eq!(d.mcp_session_cap, 32);
        assert_eq!(d.mcp_session_idle_timeout, 14_400);
        assert_eq!(d.mcp_sweep_parallel, 4);
    }

    #[test]
    fn load_reads_mcp_session_bounds() {
        let path = write_tmp(
            "mcp-session.toml",
            "[mcp]\nsession_cap = 8\nsession_idle_timeout = 120\nsweep_parallel = 2\n",
        );
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_session_cap, 8);
        assert_eq!(cfg.mcp_session_idle_timeout, 120);
        assert_eq!(cfg.mcp_sweep_parallel, 2);
    }

    #[test]
    fn mcp_job_limits_default_to_conservative_values() {
        let d = Config::default();
        assert_eq!(d.mcp_max_active_jobs, 16);
        assert_eq!(d.mcp_max_completed_jobs, 128);
    }

    #[test]
    fn load_reads_mcp_job_limits() {
        let path = write_tmp(
            "mcp-jobs.toml",
            "[mcp]\nmax_active_jobs = 4\nmax_completed_jobs = 10\n",
        );
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_max_active_jobs, 4);
        assert_eq!(cfg.mcp_max_completed_jobs, 10);
    }

    #[test]
    fn mcp_job_limits_zero_disables() {
        // `0` is a valid "disabled" sentinel (not clamped like session_cap).
        let path = write_tmp(
            "mcp-jobs-zero.toml",
            "[mcp]\nmax_active_jobs = 0\nmax_completed_jobs = 0\n",
        );
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_max_active_jobs, 0);
        assert_eq!(cfg.mcp_max_completed_jobs, 0);
    }

    #[test]
    fn mcp_profile_defaults_to_full_with_empty_overrides() {
        let d = Config::default();
        assert_eq!(d.mcp_profile, "full");
        assert!(d.mcp_tools_allow.is_empty());
        assert!(d.mcp_tools_deny.is_empty());
    }

    #[test]
    fn load_reads_mcp_profile_and_tool_overrides() {
        let path = write_tmp(
            "mcp-profile.toml",
            "[mcp]\nprofile = \"core\"\ntools_allow = [\"whoami\"]\ntools_deny = [\"run\", \"update\"]\n",
        );
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.mcp_profile, "core");
        assert_eq!(cfg.mcp_tools_allow, vec!["whoami".to_owned()]);
        assert_eq!(
            cfg.mcp_tools_deny,
            vec!["run".to_owned(), "update".to_owned()]
        );
    }

    #[test]
    fn load_malformed_file_logs_and_falls_back_to_defaults() {
        // Invalid TOML must not panic; defaults apply.
        let path = write_tmp("broken.toml", "connection_timeout = = 42\n");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.connection_timeout, 300);
    }

    #[test]
    fn load_wrong_type_logs_and_falls_back() {
        // connection_timeout declared as a string -> serde type error ->
        // logged + skipped -> default applied (no crash).
        let path = write_tmp("typed.toml", "[connection]\nconnection_timeout = \"abc\"\n");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.connection_timeout, 300);
    }

    #[test]
    fn read_file_reports_none_for_absent() {
        let path = std::env::temp_dir().join("mtui-config-absent-xyz.toml");
        assert!(matches!(read_file(&path), Ok(None)));
    }

    #[test]
    fn read_file_parses_ssl_verify_path() {
        let path = write_tmp("ssl.toml", "[mtui]\nssl_verify = \"/etc/ca.pem\"\n");
        let cfg = Config::load(Some(path));
        assert_eq!(
            cfg.ssl_verify,
            SslVerify::CaBundle(PathBuf::from("/etc/ca.pem"))
        );
    }

    #[test]
    fn load_ssl_verify_native_bool_false_disables() {
        // Regression: `ssl_verify = false` as a native TOML boolean must
        // actually disable verification (previously it silently defaulted to
        // Enabled because the field only accepted strings).
        let path = write_tmp("ssl_bool.toml", "[mtui]\nssl_verify = false\n");
        assert_eq!(Config::load(Some(path)).ssl_verify, SslVerify::Disabled);
    }

    #[test]
    fn load_ssl_verify_native_bool_true_enables() {
        let path = write_tmp("ssl_bool_t.toml", "[mtui]\nssl_verify = true\n");
        assert_eq!(Config::load(Some(path)).ssl_verify, SslVerify::Enabled);
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let path = write_tmp("empty.toml", "");
        assert_eq!(Config::load(Some(path)), Config::default());
    }
}
