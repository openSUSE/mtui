//! The `whoami` command.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_datasources::obs::oscrc;
use mtui_datasources::sanitize_url;
use mtui_datasources::sshsig::key_display;
use mtui_datasources::teregen::{TokenStore, describe_age};

use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Displays the current user name and session PID, plus the remote identity
/// that OBS review actions and teregen v2 writes authenticate as.
///
/// [`Config::session_user`](mtui_config::Config) plus the running process's PID
/// is the session identity used for host locking and logging — but OBS/teregen
/// authenticate as the oscrc principal for `obs_api_url` instead, which need
/// not match. The principal and the signing-key fingerprint print on both
/// surfaces (a refused `assign` or `commit` is triaged against them); key and
/// oscrc file paths, the teregen base and the token-cache age are REPL-only.
/// Session-level: runs once ([`Scope::Single`]) and takes no template flag.
pub struct Whoami;

#[async_trait]
impl Command for Whoami {
    fn name(&self) -> &'static str {
        "whoami"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Displays the current user name, session PID and remote identity.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Session identity; no report involved.
        false
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        self.call_with_store(session, TokenStore::new()).await
    }
}

impl Whoami {
    /// The [`Command::call`] body with the token store injected: production
    /// passes [`TokenStore::new`], tests pass [`TokenStore::at`] a temp path
    /// so they never touch the operator's real cache (`directories` ignores
    /// `$XDG_DATA_HOME` on macOS, resolving to `~/Library/…` instead).
    pub(crate) async fn call_with_store(
        &self,
        session: &mut Session,
        store: Option<TokenStore>,
    ) -> CommandResult {
        let user = session.config.session_user.clone();
        let pid = std::process::id();
        session
            .display
            .println(&format!("User: {user}, app pid: {pid}"));
        match oscrc::read_credentials(&session.config.obs_api_url) {
            Ok(creds) => {
                let key = key_display(
                    creds.sshkey_path.as_deref(),
                    creds.sshkey_fingerprint.as_deref(),
                );
                session
                    .display
                    .println(&format!("principal: {}", creds.user));
                session.display.println(&format!("signing key: {key}"));
                if session.is_repl {
                    if let Some(path) = &creds.sshkey_path {
                        session
                            .display
                            .println(&format!("key path: {}", path.display()));
                    }
                    session.display.println(&format!(
                        "oscrc: [{}] in {}",
                        sanitize_url(&session.config.obs_api_url),
                        creds.source
                    ));
                    let base = sanitize_url(&session.config.teregen_api_v2);
                    session.display.println(&format!("teregen v2: {base}"));
                    match store {
                        None => {
                            session.display.println(
                                "token store unavailable (could not resolve the data dir)",
                            );
                        }
                        Some(store) => {
                            match store.load(&session.config.teregen_api_v2, &creds.user) {
                                Some(cached) => {
                                    let age = describe_age(&cached.minted_at)
                                        .map(|age| format!(" ({age})"))
                                        .unwrap_or_default();
                                    session.display.println(&format!(
                                        "cached token minted {}{age}, at {}",
                                        cached.minted_at,
                                        store.path().display()
                                    ));
                                }
                                None => {
                                    session.display.println("no cached token");
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // oscrc errors name the oscrc file and possibly the key file.
                // Headlessly print a fixed reason so no path can leak and
                // nothing needs redacting after the fact.
                if session.is_repl {
                    session.display.println(&format!("oscrc: unreadable ({e})"));
                } else {
                    session.display.println("oscrc: unreadable");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_with_hosts};

    #[test]
    fn name_is_whoami() {
        assert_eq!(Whoami.name(), "whoami");
    }

    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    // `read_credentials` consults the process-global `$OSC_CONFIG`, so every
    // test driving `call` here serializes on the same key.
    #[allow(unsafe_code)]
    async fn prints_user_and_pid() {
        let (mut session, buf) = empty_session();
        session.config.session_user = "alice".to_owned();
        let args = matches(&Whoami, &[]);
        Whoami.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("User: alice, app pid: "), "output: {out}");
        assert!(
            out.contains(&std::process::id().to_string()),
            "output: {out}"
        );
    }

    /// Headless with several templates loaded, the session identity is printed
    /// once — not once per template under a banner (#597).
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn runs_once_headless_with_several_templates_loaded() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        assert!(!session.is_repl && session.templates.len() == 2);
        let args = matches(&Whoami, &[]);
        Whoami.run(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("User: ")).count(),
            1,
            "output: {out}"
        );
        assert!(!out.contains("=== "), "output: {out}");
    }

    /// The remote identity prints on both surfaces; paths stay REPL-only.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn prints_remote_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            "[https://api.example.invalid]\nuser = qamuser\nsshkey = SHA256:abc123\n",
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.config.session_user = "alice".to_owned();
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        let args = matches(&Whoami, &[]);
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section,
        // which also covers `XDG_DATA_HOME` (the only mutators of either
        // live under this key).
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        Whoami.call(&mut session, &args).await.unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(out.contains("User: alice, app pid: "), "output: {out}");
        assert!(out.contains("principal: qamuser"), "output: {out}");
        assert!(out.contains("signing key: SHA256:abc123"), "output: {out}");
        for marker in ["key path:", "oscrc: [", "teregen v2:", "cached token"] {
            assert!(
                !out.contains(marker),
                "headless must not print {marker:?}: {out}"
            );
        }
    }

    /// An unreadable oscrc is reported, not a failure. Headlessly the
    /// reason is fixed, so no path can leak.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn unreadable_oscrc_prints_not_fails() {
        let (mut session, buf) = empty_session();
        session.config.session_user = "alice".to_owned();
        let args = matches(&Whoami, &[]);
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section,
        // which also covers `XDG_DATA_HOME` (the only mutators of either
        // live under this key).
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        Whoami.call(&mut session, &args).await.unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(out.contains("User: alice, app pid: "), "output: {out}");
        assert_eq!(
            out.lines().find(|l| l.starts_with("oscrc:")).map(str::trim),
            Some("oscrc: unreadable"),
            "headless prints the fixed reason: {out}"
        );
        assert!(!out.contains("/nonexistent"), "no raw path headless: {out}");
    }

    /// At the REPL the unreadable-oscrc reason prints verbatim (paths
    /// included): local paths stay REPL-only by construction.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn repl_oscrc_error_prints_verbatim() {
        let (mut session, buf) = empty_session();
        session.is_repl = true;
        session.config.session_user = "alice".to_owned();
        let args = matches(&Whoami, &[]);
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        Whoami.call(&mut session, &args).await.unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(out.contains("oscrc: unreadable"), "output: {out}");
        assert!(
            out.contains("/nonexistent/oscrc-for-tests"),
            "at the REPL the path shows: {out}"
        );
    }

    /// At the REPL the key path, oscrc source, base and cache state print.
    /// The store is injected, never the ambient data dir.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn repl_prints_paths_and_cache_state() {
        let dir = tempfile::TempDir::new().unwrap();
        // The real fixture private key (copied without its `.pub`, so the
        // fingerprint must come from the public half of the private key).
        let key_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mtui-datasources/tests/fixtures/obs/id_ed25519");
        let key = dir.path().join("id_test");
        std::fs::copy(&key_src, &key).unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            format!(
                "[https://api.example.invalid]\nuser = qamuser\nsshkey = {}\n",
                key.display()
            ),
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.is_repl = true;
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        session.config.teregen_api_v2 = "https://teregen.example.invalid/api/v2".to_owned();
        // Seed the injected cache with a known token: the display must name
        // its age and path, never its value.
        const SEEDED: &str = "seededtokenseededtokenseededtokenseededtokenseeded01";
        let store_file = dir.path().join("teregen-token.json");
        std::fs::write(
            &store_file,
            serde_json::json!({
                "base": "https://teregen.example.invalid/api/v2",
                "principal": "qamuser",
                "token": SEEDED,
                "minted_at": "2026-01-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        Whoami
            .call_with_store(&mut session, Some(TokenStore::at(store_file.clone())))
            .await
            .unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(
            out.contains("signing key: SHA256:lbmsoA0yIEcEiVDRnMWuzm+nV+3ZEEpVIURqFoeSspg"),
            "a file-backed key prints its fingerprint, not `file key`: {out}"
        );
        assert!(out.contains("key path: "), "output: {out}");
        assert!(
            out.contains("oscrc: [https://api.example.invalid]"),
            "output: {out}"
        );
        assert!(out.contains("teregen v2: "), "output: {out}");
        assert!(out.contains("cached token minted"), "output: {out}");
        assert!(
            out.contains(&store_file.display().to_string()),
            "the injected store path shows: {out}"
        );
        assert!(!out.contains(SEEDED), "the cached token must never print");
    }

    /// The injected store wins over `$XDG_DATA_HOME`: with a decoy token in
    /// the env dir for the same principal/base, the display still names the
    /// injected entry's age and path. Fails when the store resolves from the
    /// environment (on macOS that is the real `~/Library/…` cache, which the
    /// old test even overwrote with a fake).
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn injected_store_is_used_not_the_env_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            "[https://api.example.invalid]\nuser = qamuser\nsshkey = SHA256:abc123\n",
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.is_repl = true;
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        session.config.teregen_api_v2 = "https://teregen.example.invalid/api/v2".to_owned();
        // Decoy env cache for the same principal/base, older timestamp.
        let env_home = dir.path().join("env-home");
        std::fs::create_dir_all(env_home.join("mtui")).unwrap();
        std::fs::write(
            env_home.join("mtui/teregen-token.json"),
            serde_json::json!({
                "base": "https://teregen.example.invalid/api/v2",
                "principal": "qamuser",
                "token": "b".repeat(64),
                "minted_at": "2020-05-05T05:05:05Z",
            })
            .to_string(),
        )
        .unwrap();
        let store_file = dir.path().join("injected-token.json");
        std::fs::write(
            &store_file,
            serde_json::json!({
                "base": "https://teregen.example.invalid/api/v2",
                "principal": "qamuser",
                "token": "c".repeat(64),
                "minted_at": "2026-01-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section,
        // which also covers `XDG_DATA_HOME`.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        unsafe { std::env::set_var("XDG_DATA_HOME", &env_home) };
        Whoami
            .call_with_store(&mut session, Some(TokenStore::at(store_file.clone())))
            .await
            .unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        let out = buf.contents();
        assert!(
            out.contains("2026-01-01T00:00:00Z"),
            "the injected entry wins: {out}"
        );
        assert!(
            !out.contains("2020-05-05T05:05:05Z"),
            "the env decoy must not show: {out}"
        );
        assert!(
            out.contains(&store_file.display().to_string()),
            "the injected store path shows: {out}"
        );
    }

    /// Without a store the REPL says so instead of showing cache state.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn unavailable_store_prints_not_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            "[https://api.example.invalid]\nuser = qamuser\nsshkey = SHA256:abc123\n",
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.is_repl = true;
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        Whoami.call_with_store(&mut session, None).await.unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(out.contains("principal: qamuser"), "output: {out}");
        assert!(out.contains("store unavailable"), "output: {out}");
    }

    /// Headless, a missing key file must not leak its path: the reason is
    /// fixed, so neither the oscrc path nor the key path can survive.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn missing_key_file_prints_fixed_reason_headlessly() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("id_missing");
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            format!(
                "[https://api.example.invalid]\nuser = qamuser\nsshkey = {}\n",
                missing.display()
            ),
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        let args = matches(&Whoami, &[]);
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        Whoami.call(&mut session, &args).await.unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(out.contains("oscrc: unreadable"), "output: {out}");
        assert!(
            !out.contains("id_missing"),
            "the missing key file must not print: {out}"
        );
    }

    /// Headless, a `~/…` sshkey leaks neither form: the reason is fixed, so
    /// neither the raw value nor the expanded path can survive.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn tilde_key_prints_fixed_reason_headlessly() {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            "[https://api.example.invalid]\nuser = qamuser\nsshkey = ~/keys/id_missing\n",
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        let args = matches(&Whoami, &[]);
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        Whoami.call(&mut session, &args).await.unwrap();
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let out = buf.contents();
        assert!(out.contains("oscrc: unreadable"), "output: {out}");
        assert!(
            !out.contains("~/keys/id_missing"),
            "the raw sshkey value must not print: {out}"
        );
        assert!(
            !out.contains("keys/id_missing"),
            "neither sshkey form may survive: {out}"
        );
    }
}
