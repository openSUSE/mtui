//! `mtui-config` — TOML config parsing and XDG path resolution for mtui.
//!
//! Loads the `mtui.toml` file, resolved highest-precedence first from the
//! `--config` flag, `$MTUI_CONF`, the XDG per-user file
//! (`$XDG_CONFIG_HOME/mtui/mtui.toml`), the home dotfile (`~/.mtui.toml`), then
//! `/etc/mtui.toml`. The behavioural contract is: sectioned options, documented
//! defaults for missing keys, and **lenient loading** — a bad or missing file is
//! logged and skipped, never fatal.
//!
//! Every option section mtui understands is modelled here (see [`options`]);
//! CLI-argument merging lives with the `clap` args structs that own it,
//! `mtui_core::args` for the REPL and `mtui_mcp::args` for the MCP server.

pub mod atomic;
pub mod error;
pub mod notice;
pub mod options;
pub mod paths;

use std::path::Path;
use std::path::PathBuf;

pub(crate) use error::ConfigError;
pub use notice::{ConfigNotice, ConfigSource};
pub use options::{Config, SslVerify};
pub use paths::data_dir;
pub(crate) use paths::{Chain, config_chain};

use notice::{MAX_NOTICES_PER_FILE, clean_key};
use options::RawConfig;

impl Config {
    /// Load configuration from the resolved search paths.
    ///
    /// `explicit` is the optional `--config` path. Files are merged
    /// **lowest-precedence first** (see `config_chain`): `/etc` →
    /// `~/.mtui.toml` → the XDG file, so a per-user file overrides `/etc` on
    /// shared keys. An absent file is skipped silently, an unreadable or
    /// unparseable one is logged at ERROR and skipped — loading never
    /// hard-fails, and absent options take their documented defaults. What a
    /// loaded file held but no option reads is logged at WARN and kept in
    /// [`Config::notices`], as is an unread `~/.mtuirc` when no TOML file was
    /// loaded.
    #[must_use]
    pub fn load(explicit: Option<PathBuf>) -> Self {
        load_from(&config_chain(explicit))
    }
}

/// [`Config::load`] over a resolved chain, so tests need not touch the process
/// environment or the machine's `/etc`.
fn load_from(chain: &Chain) -> Config {
    let mut merged = RawConfig::default();
    let mut notices = Vec::new();
    let mut loaded_any = false;
    for path in &chain.layers {
        match read_file(path) {
            Ok(Some((raw, unknown))) => {
                merged.merge(raw);
                notices.extend(unknown);
                loaded_any = true;
            }
            Ok(None) => continue,
            Err(err) => {
                tracing::error!(
                    path = %path.display(),
                    error = %err,
                    "ignoring config file that failed to load; using defaults for its options"
                );
            }
        }
    }
    if let Some(rc) = &chain.legacy_rc
        && !loaded_any
        && rc.is_file()
    {
        notices.insert(
            0,
            ConfigNotice::LegacyRc {
                path: rc.clone(),
                user_file: chain.source.user_file.clone(),
            },
        );
    }
    for notice in &notices {
        tracing::warn!("{}", notice.render(None));
    }
    let mut config = Config::from_raw(merged);
    config.notices = notices;
    config.source = chain.source.clone();
    config
}

/// Read and parse one config file; `Ok(None)` means the file does not exist.
/// Alongside the options it returns a notice per key no option reads.
fn read_file(path: &Path) -> Result<Option<(RawConfig, Vec<ConfigNotice>)>, ConfigError> {
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
    let toml_err = |source| ConfigError::Toml {
        path: path.to_path_buf(),
        source,
    };
    let mut unknown: Vec<Vec<String>> = Vec::new();
    let raw: RawConfig = serde_ignored::deserialize(
        toml::Deserializer::parse(&contents).map_err(toml_err)?,
        |ignored| {
            let mut segments = Vec::new();
            path_segments(&ignored, &mut segments);
            unknown.push(segments);
        },
    )
    .map_err(toml_err)?;
    Ok(Some((raw, unknown_notices(path, &unknown, &contents))))
}

/// The keys leading to an ignored value; the value itself is never kept.
fn path_segments(path: &serde_ignored::Path<'_>, out: &mut Vec<String>) {
    use serde_ignored::Path as P;
    match path {
        P::Root => {}
        P::Seq { parent, index } => {
            path_segments(parent, out);
            out.push(index.to_string());
        }
        P::Map { parent, key } => {
            path_segments(parent, out);
            out.push(key.clone());
        }
        P::Some { parent } | P::NewtypeStruct { parent } | P::NewtypeVariant { parent } => {
            path_segments(parent, out);
        }
    }
}

/// Names each ignored key by table and key, a whole unknown table as one, and
/// only counts those past [`MAX_NOTICES_PER_FILE`].
fn unknown_notices(path: &Path, unknown: &[Vec<String>], contents: &str) -> Vec<ConfigNotice> {
    let mut top_level: Option<Option<toml::Table>> = None;
    let mut notices: Vec<ConfigNotice> = unknown
        .iter()
        .take(MAX_NOTICES_PER_FILE)
        .map(|segments| {
            let file = path.to_path_buf();
            match segments.as_slice() {
                [table, key @ ..] if !key.is_empty() => ConfigNotice::UnknownKey {
                    file,
                    table: Some(clean_key(table)),
                    key: clean_key(&key.join(".")),
                },
                [name]
                    if top_level
                        .get_or_insert_with(|| toml::from_str(contents).ok())
                        .as_ref()
                        .and_then(|t| t.get(name))
                        .is_some_and(is_table) =>
                {
                    ConfigNotice::UnknownTable {
                        file,
                        table: clean_key(name),
                    }
                }
                _ => ConfigNotice::UnknownKey {
                    file,
                    table: None,
                    key: clean_key(&segments.join(".")),
                },
            }
        })
        .collect();
    if unknown.len() > MAX_NOTICES_PER_FILE {
        notices.push(ConfigNotice::MoreUnknown {
            file: path.to_path_buf(),
            count: unknown.len() - MAX_NOTICES_PER_FILE,
        });
    }
    notices
}

/// A table, or an array of tables (`[[name]]`).
fn is_table(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(_) => true,
        toml::Value::Array(items) => !items.is_empty() && items.iter().all(toml::Value::is_table),
        _ => false,
    }
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
        let cfg = Config::load(Some(path.clone()));
        assert_eq!(cfg.source.explicit, Some(path));
        assert_eq!(
            Config {
                source: ConfigSource::default(),
                ..cfg
            },
            Config::default()
        );
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
        assert_eq!(cfg.reports_url, "https://qam.suse.de/testreports");
    }

    #[test]
    fn reboot_backoff_options_have_expected_defaults() {
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
    fn mcp_session_bounds_have_expected_defaults() {
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
        let path = write_tmp("broken.toml", "connection_timeout = = 42\n");
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.connection_timeout, 300);
    }

    #[test]
    fn load_wrong_type_logs_and_falls_back() {
        // A serde type error is logged and skipped, not a crash.
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
        // A native TOML boolean, not just the string spellings, must disable it.
        let path = write_tmp("ssl_bool.toml", "[mtui]\nssl_verify = false\n");
        assert_eq!(Config::load(Some(path)).ssl_verify, SslVerify::Disabled);
    }

    #[test]
    fn load_ssl_verify_native_bool_true_enables() {
        let path = write_tmp("ssl_bool_t.toml", "[mtui]\nssl_verify = true\n");
        assert_eq!(Config::load(Some(path)).ssl_verify, SslVerify::Enabled);
    }

    const UNKNOWN: &str = include_str!("../tests/fixtures/golden/unknown.toml");
    const UNKNOWN_GOLDEN: &str = include_str!("../tests/fixtures/golden/unknown.txt");

    fn chain(layers: &[PathBuf], legacy_rc: Option<&Path>, home: Option<&Path>) -> Chain {
        Chain {
            layers: layers.to_vec(),
            legacy_rc: legacy_rc.map(Path::to_path_buf),
            source: ConfigSource {
                explicit: None,
                user_file: home.map(|h| h.join(".config/mtui/mtui.toml")),
                home: home.map(Path::to_path_buf),
            },
        }
    }

    fn notices_of(chain: &Chain) -> (Vec<ConfigNotice>, String) {
        let mut notices = Vec::new();
        let logs = crate::options::tests::capture_fields(|| {
            notices = load_from(chain).notices;
        });
        (notices, logs)
    }

    fn unknown_key(file: &Path, table: &str, key: &str) -> ConfigNotice {
        ConfigNotice::UnknownKey {
            file: file.to_path_buf(),
            table: Some(table.to_owned()),
            key: key.to_owned(),
        }
    }

    #[test]
    fn unknown_keys_are_named_once_with_file_and_table_never_with_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mtui.toml");
        std::fs::write(&path, UNKNOWN).unwrap();

        let (notices, logs) = notices_of(&chain(std::slice::from_ref(&path), None, None));

        let file = path.display().to_string();
        let rendered: String = notices
            .iter()
            .map(|n| format!("{}\n", n.render(None).replace(&file, "<file>")))
            .collect();
        assert_eq!(rendered, UNKNOWN_GOLDEN);
        for notice in &notices {
            assert!(
                logs.contains(&format!("level=WARN message={} ", notice.render(None))),
                "{notice:?} not logged at WARN: {logs}"
            );
        }
        assert!(!logs.contains("SENTINEL"), "a value leaked: {logs}");
        assert!(!rendered.contains("SENTINEL"), "a value leaked: {rendered}");
    }

    #[test]
    fn unknown_keys_do_not_stop_the_known_ones_from_loading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mtui.toml");
        std::fs::write(&path, UNKNOWN).unwrap();
        let cfg = Config::load(Some(path));
        assert_eq!(cfg.session_user, "kept");
        assert_eq!(cfg.ssl_verify, SslVerify::Disabled);
        assert_eq!(cfg.target_protected_packages, vec!["kernel-foo".to_owned()]);
    }

    /// Guards against a known option being reported as unknown.
    #[test]
    fn a_known_option_is_never_reported_as_unknown() {
        for name in ["full_a.toml", "full_b.toml", "invalid.toml"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/golden")
                .join(name);
            assert!(path.is_file(), "fixture missing: {}", path.display());
            let (notices, _) = notices_of(&chain(&[path], None, None));
            assert!(notices.is_empty(), "{name}: {notices:?}");
        }
    }

    #[test]
    fn the_same_unknown_key_is_named_once_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml");
        let b = dir.path().join("b.toml");
        std::fs::write(&a, "[mtui]\ntempdir = \"/x\"\n").unwrap();
        std::fs::write(&b, "[mtui]\ntempdir = \"/y\"\n").unwrap();
        let (notices, _) = notices_of(&chain(&[a.clone(), b.clone()], None, None));
        assert_eq!(
            notices,
            vec![
                unknown_key(&a, "mtui", "tempdir"),
                unknown_key(&b, "mtui", "tempdir"),
            ]
        );
    }

    #[test]
    fn unknown_keys_past_the_cap_are_counted_not_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mtui.toml");
        let body: String = (0..MAX_NOTICES_PER_FILE + 5)
            .map(|i| format!("k{i:02} = 1\n"))
            .collect();
        std::fs::write(&path, format!("[mtui]\n{body}")).unwrap();

        let (notices, _) = notices_of(&chain(std::slice::from_ref(&path), None, None));

        assert_eq!(notices.len(), MAX_NOTICES_PER_FILE + 1, "{notices:?}");
        assert_eq!(notices[0], unknown_key(&path, "mtui", "k00"));
        assert_eq!(
            notices[MAX_NOTICES_PER_FILE],
            ConfigNotice::MoreUnknown {
                file: path.clone(),
                count: 5
            }
        );
    }

    #[test]
    fn a_key_name_reaches_a_notice_without_terminal_controls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mtui.toml");
        std::fs::write(&path, "[mtui]\n\"evil\\u001b]52;c;x\\u0007key\" = 1\n").unwrap();

        let (notices, logs) = notices_of(&chain(std::slice::from_ref(&path), None, None));

        assert_eq!(notices, vec![unknown_key(&path, "mtui", "evil]52;c;xkey")]);
        assert!(!logs.contains('\u{1b}'), "{logs:?}");
    }

    /// The note when no TOML file was loaded, in the form headless output uses.
    const MTUIRC_HEADLESS: &str = "~/.mtuirc is not read: mtui reads only TOML config \
         files; put its settings in ~/.config/mtui/mtui.toml; see \
         https://opensuse.github.io/mtui/configuration.html";

    #[test]
    fn the_mtuirc_note_appears_once_when_no_toml_file_was_loaded() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".mtuirc");
        std::fs::write(&rc, "[mtui]\ngitea_token = SENTINEL\n").unwrap();
        let broken = home.path().join("broken.toml");
        std::fs::write(&broken, "user = = 1\n").unwrap();
        let absent = home.path().join("absent.toml");

        for layers in [
            vec![absent.clone(), absent.clone()],
            vec![absent.clone(), broken],
        ] {
            let (notices, logs) = notices_of(&chain(&layers, Some(&rc), Some(home.path())));

            let rendered: Vec<String> = notices
                .iter()
                .map(|n| n.render(Some(home.path())))
                .collect();
            assert_eq!(rendered, [MTUIRC_HEADLESS], "{layers:?}");
            assert_eq!(
                logs.matches("level=WARN ").count(),
                1,
                "logged once: {logs}"
            );
            assert!(logs.contains(".mtuirc is not read"), "{logs}");
            assert_eq!(
                logs.contains("level=ERROR "),
                layers[1] != absent,
                "the parse error is still logged: {logs}"
            );
            assert!(!logs.contains("SENTINEL"), "{logs}");
        }
    }

    /// Guards against the note ignoring a loaded file in any slot, the system
    /// one included, and against naming an rc that does not exist.
    #[test]
    fn the_mtuirc_note_is_silenced_by_any_loaded_file_or_a_missing_rc() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".mtuirc");
        let loaded = home.path().join("loaded.toml");
        std::fs::write(&loaded, "[mtui]\nuser = \"u\"\n").unwrap();
        let absent = home.path().join("absent.toml");

        let (missing_rc, _) = notices_of(&chain(
            std::slice::from_ref(&absent),
            Some(&rc),
            Some(home.path()),
        ));
        assert!(missing_rc.is_empty(), "rc absent: {missing_rc:?}");

        std::fs::write(&rc, "[mtui]\n").unwrap();
        for layers in [
            vec![loaded.clone(), absent.clone(), absent.clone()],
            vec![absent.clone(), absent.clone(), loaded.clone()],
        ] {
            let (notices, _) = notices_of(&chain(&layers, Some(&rc), Some(home.path())));
            assert!(notices.is_empty(), "{layers:?}: {notices:?}");
        }
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let path = write_tmp("empty.toml", "");
        let cfg = Config::load(Some(path));
        assert_eq!(
            Config {
                source: ConfigSource::default(),
                ..cfg
            },
            Config::default()
        );
    }
}
