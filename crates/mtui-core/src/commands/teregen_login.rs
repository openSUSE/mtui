//! The `teregen_login` command: check the teregen v2 login without writing.

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_datasources::obs::oscrc;
use mtui_datasources::sanitize_url;
use mtui_datasources::sshsig::key_display;
use mtui_datasources::teregen::{TokenStore, describe_age};

use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Checks the teregen v2 login by forcing a fresh mint, writing nothing
/// except the token cache.
///
/// REPL-only (on the MCP deny-list): each call spends one of ~20
/// challenges/60s for the principal, so a looping client would lock the
/// tester out of `commit`/`approve`.
///
/// Every teregen v2 read is anonymous, so only a mint exercises the login;
/// `commit`, `approve` and `request_review` upload through it and fail at the
/// end of a validation when it is broken. This runs the same challenge +
/// verify as those paths (the oscrc principal only — no `--user`, so one
/// call cannot spend another principal's challenge budget) and prints the
/// principal, the key fingerprint, the base, the mint verdict and the age of
/// the previously cached token. The token itself is never printed.
/// Session-level: runs once ([`Scope::Single`]) and takes no template flag.
pub struct TeregenLogin;

#[async_trait]
impl Command for TeregenLogin {
    fn name(&self) -> &'static str {
        "teregen_login"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Checks the teregen v2 login by forcing a fresh mint.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn reads_resolved_report(&self) -> bool {
        // Session-level diagnostic; no report involved.
        false
    }

    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        self.call_with_store(session, TokenStore::new()).await
    }
}

impl TeregenLogin {
    /// The [`Command::call`] body with the token store injected: the
    /// previous-token line and the mint's cache write both go through `store`
    /// (see [`Whoami`](super::whoami::Whoami)'s seam for why tests must not
    /// rely on the ambient data dir).
    pub(crate) async fn call_with_store(
        &self,
        session: &mut Session,
        store: Option<TokenStore>,
    ) -> CommandResult {
        // REPL-only (on the MCP deny-list): error strings below can name
        // local paths (oscrc/key files), printed verbatim like `whoami`'s
        // REPL half — no headless redaction exists on this command.
        let creds = oscrc::read_credentials(&session.config.obs_api_url)
            .map_err(|e| CommandError::Other(format!("could not read oscrc credentials: {e}")))?;
        let key = key_display(
            creds.sshkey_path.as_deref(),
            creds.sshkey_fingerprint.as_deref(),
        );
        let base = session.config.teregen_api_v2.clone();

        let prev_line = match &store {
            None => "previous token: store unavailable (could not resolve the data dir)".to_owned(),
            Some(store) => match store.load(&base, &creds.user) {
                Some(cached) => previous_token_line(Some(&cached)),
                None => previous_token_line(None),
            },
        };

        let http = session
            .http_client()
            .map_err(|e| CommandError::Other(format!("could not build TeReGen client: {e}")))?;
        let auth = mtui_datasources::teregen::TeregenAuth::new(
            base.clone(),
            creds.user.clone(),
            creds.sshkey_path.clone(),
            creds.sshkey_fingerprint.clone(),
            http,
        )
        .with_store(store);

        // Identity context first: a failed mint leaves the previous entry
        // alone, so its age is most useful exactly when the mint fails —
        // and the display buffer reaches the caller on `Err` too.
        session
            .display
            .println(&format!("principal: {}", creds.user));
        session.display.println(&format!("signing key: {key}"));
        session
            .display
            .println(&format!("base: {}", sanitize_url(&base)));
        session.display.println(&prev_line);
        // The mint is the check: every v2 read is anonymous, so a probe
        // would prove nothing about the token.
        auth.refresh()
            .await
            .map_err(|e| CommandError::Other(format!("teregen login mint failed: {e}")))?;

        session.display.println("token: minted");
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
        }
        Ok(())
    }
}

/// One line describing the previously cached token: its mint time and age
/// when parseable, its mint time alone when the entry is corrupt, or `none`
/// when there is no entry. A corrupt entry is still an entry — it must not
/// read as `none`.
fn previous_token_line(cached: Option<&mtui_datasources::teregen::CachedToken>) -> String {
    match cached {
        Some(cached) => match describe_age(&cached.minted_at) {
            Some(age) => format!("previous token: minted {} ({age})", cached.minted_at),
            None => format!("previous token: minted {}", cached.minted_at),
        },
        None => "previous token: none".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_is_teregen_login() {
        assert_eq!(TeregenLogin.name(), "teregen_login");
    }

    #[test]
    fn previous_token_line_names_a_corrupt_entry() {
        use mtui_datasources::teregen::CachedToken;
        let corrupt = CachedToken {
            base: "b".to_owned(),
            principal: "u".to_owned(),
            token: "t".repeat(64),
            minted_at: "not-a-timestamp".to_owned(),
        };
        let line = previous_token_line(Some(&corrupt));
        assert!(line.contains("not-a-timestamp"), "line: {line}");
        assert!(
            !line.ends_with("none"),
            "a corrupt entry is still an entry: {line}"
        );
        assert_eq!(previous_token_line(None), "previous token: none");
    }

    /// Without oscrc credentials the command fails before any request.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn refuses_without_oscrc() {
        let (mut session, _buf) = empty_session();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section,
        // which also covers `XDG_DATA_HOME` (the only mutators of either
        // live under this key).
        unsafe { std::env::set_var("OSC_CONFIG", "/nonexistent/oscrc-for-tests") };
        let args = matches(&TeregenLogin, &[]);
        let res = TeregenLogin.call(&mut session, &args).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let err = res.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not read oscrc credentials")),
            "got {err:?}"
        );
    }

    /// REPL, an unreadable CA bundle fails the client build with the path
    /// shown: a REPL-only command prints local paths verbatim, so this pins
    /// the refusal reason (and covers the client-build error mapping).
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn ca_bundle_failure_names_path_at_repl() {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            "[https://api.example.invalid]\nuser = qamuser\nsshkey = SHA256:abc123\n",
        )
        .unwrap();
        let (mut session, _buf) = empty_session();
        session.is_repl = true;
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        session.config.teregen_api_v2 = "https://teregen.example.invalid/api/v2".to_owned();
        session.config.ssl_verify =
            mtui_config::SslVerify::CaBundle(dir.path().join("missing-ca.pem"));
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        let res = TeregenLogin.call_with_store(&mut session, None).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let err = res.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not build TeReGen client")),
            "got {err:?}"
        );
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("missing-ca.pem")),
            "at the REPL the bundle path shows: {err:?}"
        );
    }

    /// A fresh mint is forced even with a cached token, and the token itself
    /// is never printed. The store is injected: the previous-token line reads
    /// it and the mint overwrites it, never the ambient data dir.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn forces_a_fresh_mint_and_never_prints_the_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const TOKEN: &str = "tokentokentokentokentokentokentokentokentokentokentokentokentoken00";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/challenge"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"nonce": NONCE})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/verify"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": TOKEN})),
            )
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let key_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mtui-datasources/tests/fixtures/obs/id_ed25519");
        let key = dir.path().join("id_test");
        std::fs::copy(&key_src, &key).unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            format!(
                "[{}]\nuser = qamuser\nsshkey = {}\n",
                server.uri(),
                key.display()
            ),
        )
        .unwrap();
        // The injected cache pre-seeded with a usable entry: the command must
        // still hit the challenge once, then overwrite this file.
        let store_file = dir.path().join("teregen-token.json");
        std::fs::write(
            &store_file,
            serde_json::json!({
                "base": server.uri(),
                "principal": "qamuser",
                "token": "a".repeat(64),
                "minted_at": "2026-01-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();

        let (mut session, buf) = empty_session();
        session.config.obs_api_url = server.uri();
        session.config.teregen_api_v2 = server.uri();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        let res = TeregenLogin
            .call_with_store(&mut session, Some(TokenStore::at(store_file.clone())))
            .await;
        unsafe { std::env::remove_var("OSC_CONFIG") };
        res.expect("login check succeeds");
        let out = buf.contents();
        assert!(out.contains("principal: qamuser"), "output: {out}");
        assert!(
            out.contains("signing key: SHA256:lbmsoA0yIEcEiVDRnMWuzm+nV+3ZEEpVIURqFoeSspg"),
            "a file-backed key prints its fingerprint, not `file key`: {out}"
        );
        assert!(out.contains("token: minted"), "output: {out}");
        assert!(out.contains("previous token: minted"), "output: {out}");
        assert!(
            !out.contains("probe:"),
            "the mint is the check; no probe: {out}"
        );
        assert!(!out.contains(TOKEN), "the token itself must never print");
        // Exactly one challenge and one verify: the mint, with no second
        // cycle spent proving the token (reads are anonymous).
        let requests = server.received_requests().await.unwrap();
        for path in ["/auth/ssh/challenge", "/auth/ssh/verify"] {
            assert_eq!(
                requests.iter().filter(|r| r.url.path() == path).count(),
                1,
                "a cached token must not suppress the mint, nor may it mint twice"
            );
        }
        assert!(
            requests
                .iter()
                .all(|r| r.method != wiremock::http::Method::GET),
            "no probe request may be sent"
        );
        // The mint wrote through the injected store.
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store_file).unwrap()).unwrap();
        assert_eq!(stored["token"], serde_json::Value::String(TOKEN.to_owned()));
    }

    /// Without a store the previous-token line says so and the mint still
    /// runs (uncached): the seam's `None` path, failing here only because
    /// nothing listens, after the line was already printed.
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn unavailable_store_still_mints_uncached() {
        let dir = tempfile::TempDir::new().unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            "[https://api.example.invalid]\nuser = qamuser\nsshkey = SHA256:abc123\n",
        )
        .unwrap();
        let (mut session, buf) = empty_session();
        session.config.obs_api_url = "https://api.example.invalid".to_owned();
        session.config.teregen_api_v2 = "http://127.0.0.1:1".to_owned();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        let res = TeregenLogin.call_with_store(&mut session, None).await;
        // SAFETY: still inside that critical section.
        unsafe { std::env::remove_var("OSC_CONFIG") };
        let err = res.unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("mint failed")),
            "without a server the uncached mint fails: {err:?}"
        );
        let out = buf.contents();
        assert!(out.contains("store unavailable"), "output: {out}");
    }

    /// The previous-token line and the mint's cache write go through the
    /// injected store: with `$XDG_DATA_HOME` pointed at a decoy, the display
    /// still names the injected entry and the decoy file is never created.
    /// Fails when the store resolves from the environment (on macOS that is
    /// the real `~/Library/…` cache).
    #[tokio::test]
    #[serial_test::serial(osc_config_env)]
    #[allow(unsafe_code)]
    async fn uses_injected_store_not_the_env_dir() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const TOKEN: &str = "tokentokentokentokentokentokentokentokentokentokentokentokentoken00";

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/challenge"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"nonce": NONCE})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/ssh/verify"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": TOKEN})),
            )
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let key_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../mtui-datasources/tests/fixtures/obs/id_ed25519");
        let key = dir.path().join("id_test");
        std::fs::copy(&key_src, &key).unwrap();
        let oscrc = dir.path().join("oscrc");
        std::fs::write(
            &oscrc,
            format!(
                "[{}]\nuser = qamuser\nsshkey = {}\n",
                server.uri(),
                key.display()
            ),
        )
        .unwrap();
        let store_file = dir.path().join("injected-token.json");
        std::fs::write(
            &store_file,
            serde_json::json!({
                "base": server.uri(),
                "principal": "qamuser",
                "token": "a".repeat(64),
                "minted_at": "2026-01-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        let decoy_home = dir.path().join("decoy-home");
        std::fs::create_dir_all(decoy_home.join("mtui")).unwrap();

        let (mut session, buf) = empty_session();
        session.config.obs_api_url = server.uri();
        session.config.teregen_api_v2 = server.uri();
        // SAFETY: inside the `#[serial(osc_config_env)]` critical section,
        // which also covers `XDG_DATA_HOME`.
        unsafe { std::env::set_var("OSC_CONFIG", &oscrc) };
        unsafe { std::env::set_var("XDG_DATA_HOME", &decoy_home) };
        let res = TeregenLogin
            .call_with_store(&mut session, Some(TokenStore::at(store_file.clone())))
            .await;
        unsafe { std::env::remove_var("OSC_CONFIG") };
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        res.expect("login check succeeds");
        let out = buf.contents();
        assert!(
            out.contains("previous token: minted 2026-01-01T00:00:00Z"),
            "the injected entry is read, not the env dir: {out}"
        );
        assert!(
            !decoy_home.join("mtui/teregen-token.json").exists(),
            "the env dir must stay untouched"
        );
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&store_file).unwrap()).unwrap();
        assert_eq!(stored["token"], serde_json::Value::String(TOKEN.to_owned()));
    }
}
