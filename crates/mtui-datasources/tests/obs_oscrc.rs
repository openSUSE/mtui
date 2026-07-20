//! Golden tests for the native oscrc credential reader
//! (`mtui_datasources::obs::oscrc`), ported 1:1 from upstream
//! `tests/test_obs_oscrc.py`.
//!
//! The reader locates oscrc exactly like `osc` (`$OSC_CONFIG` →
//! `$XDG_CONFIG_HOME/osc/oscrc` → `~/.oscrc`), so each case writes a throwaway
//! oscrc under a `TempDir` and points `$OSC_CONFIG` at it, then asserts the
//! resolved [`ObsCredentials`] or the fail-closed [`ObsError::Config`] message
//! substring upstream pins. No network, no real `~/.oscrc`, no interactive
//! prompt.
//!
//! `$OSC_CONFIG`/`$HOME`/`$XDG_CONFIG_HOME` are process-global, so every test
//! that mutates them is `#[serial(osc_config_env)]` and installs an [`EnvGuard`]
//! that isolates all three under a temp dir (mirroring upstream's autouse
//! `_isolate_oscrc_discovery` fixture) and restores them on drop.

use std::fs;
use std::path::{Path, PathBuf};

use mtui_datasources::obs::oscrc::{self, ObsCredentials};
use serial_test::serial;
use tempfile::TempDir;

const API: &str = "https://api.suse.de";

/// Scoped isolation of the oscrc-discovery environment.
///
/// On construction it clears `$OSC_CONFIG` and redirects `$HOME` and
/// `$XDG_CONFIG_HOME` under a fresh temp dir, so the reader never touches the
/// developer's real files (mirrors upstream's autouse `_isolate_oscrc_discovery`
/// fixture). On drop it restores every variable to its prior value. Must only be
/// used inside `#[serial(osc_config_env)]` tests: `std::env` mutation is
/// process-global.
struct EnvGuard {
    _dir: TempDir,
    xdg: PathBuf,
    home: PathBuf,
    prev_osc_config: Option<std::ffi::OsString>,
    prev_home: Option<std::ffi::OsString>,
    prev_xdg: Option<std::ffi::OsString>,
}

impl EnvGuard {
    #[allow(unsafe_code)]
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let xdg = dir.path().join("xdg");
        let home = dir.path().join("home");
        let prev_osc_config = std::env::var_os("OSC_CONFIG");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: guarded by `#[serial(osc_config_env)]`, so no other test
        // reads/writes these vars concurrently.
        unsafe {
            std::env::remove_var("OSC_CONFIG");
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", &xdg);
        }
        Self {
            _dir: dir,
            xdg,
            home,
            prev_osc_config,
            prev_home,
            prev_xdg,
        }
    }

    /// The XDG oscrc path under the isolated `$XDG_CONFIG_HOME`.
    fn xdg_oscrc(&self) -> PathBuf {
        self.xdg.join("osc").join("oscrc")
    }

    /// The `~/.oscrc` path under the isolated `$HOME`.
    fn home_oscrc(&self) -> PathBuf {
        self.home.join(".oscrc")
    }

    /// Point `$OSC_CONFIG` at `path` so discovery selects it.
    #[allow(unsafe_code)]
    fn set_osc_config(&self, path: &Path) {
        // SAFETY: guarded by `#[serial(osc_config_env)]`.
        unsafe { std::env::set_var("OSC_CONFIG", path) };
    }
}

impl Drop for EnvGuard {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: guarded by `#[serial(osc_config_env)]`.
        unsafe {
            restore("OSC_CONFIG", self.prev_osc_config.take());
            restore("HOME", self.prev_home.take());
            restore("XDG_CONFIG_HOME", self.prev_xdg.take());
        }
    }
}

#[allow(unsafe_code)]
unsafe fn restore(key: &str, prev: Option<std::ffi::OsString>) {
    // SAFETY: only ever called from `EnvGuard::drop`, itself gated behind
    // `#[serial(osc_config_env)]`.
    unsafe {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

/// Write an oscrc file (and, when given, a dummy key file it references) into
/// `dir` and point `$OSC_CONFIG` at it via `guard`.
fn write_oscrc(guard: &EnvGuard, dir: &Path, body: &str, keyfile: Option<&Path>) -> PathBuf {
    if let Some(kf) = keyfile {
        fs::write(kf, "dummy-key").unwrap();
    }
    let path = dir.join("oscrc");
    fs::write(&path, body).unwrap();
    guard.set_osc_config(&path);
    path
}

fn read() -> Result<ObsCredentials, mtui_datasources::obs::ObsError> {
    oscrc::read_credentials(API)
}

#[test]
#[serial(osc_config_env)]
fn reads_user_and_sshkey() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_ed25519");
    let conf = write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[general]\napiurl = {API}\n\n[{API}]\nuser = bob\nsshkey = {}\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read().unwrap();
    assert_eq!(creds.user, "bob");
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.apiurl, API);
    assert_eq!(creds.source, conf.display().to_string());
}

#[test]
#[serial(osc_config_env)]
fn password_is_never_read_for_signature_target() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_ed25519");
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[{API}]\nuser = bob\npass = s3cret\npassx = AAAA==\nsshkey = {}\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read().unwrap();
    assert_eq!(creds.user, "bob");
    // The struct structurally cannot carry a password, and its Debug never
    // surfaces one.
    assert!(!format!("{creds:?}").contains("s3cret"));
}

#[test]
#[serial(osc_config_env)]
fn missing_conffile_raises() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    guard.set_osc_config(&dir.path().join("nope"));
    let err = read().unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn missing_section_raises() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        "[https://api.opensuse.org]\nuser = bob\n",
        None,
    );
    let err = read().unwrap_err();
    assert!(
        err.to_string().contains("no [https://api.suse.de] section"),
        "{err}"
    );
}

#[test]
#[serial(osc_config_env)]
fn missing_user_raises() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}]\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    let err = read().unwrap_err();
    assert!(err.to_string().contains("no 'user'"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn missing_sshkey_raises() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    write_oscrc(&guard, dir.path(), &format!("[{API}]\nuser = bob\n"), None);
    let err = read().unwrap_err();
    assert!(err.to_string().contains("no 'sshkey'"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn credentials_manager_ignored_when_sshkey_usable() {
    // A usable 'sshkey' wins; `credentials_mgr_class` is not consulted. osc orders
    // Signature auth ahead of Basic and disables the password path for the
    // transient manager, so an oscrc carrying both authenticates by signature
    // there too. Rejecting it turned a working configuration into a hard failure.
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[{API}]\nuser = bob\nsshkey = {}\n\
             credentials_mgr_class = osc.credentials.KeyringCredentialsManager\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read().unwrap();
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.user, "bob");
}

#[test]
#[serial(osc_config_env)]
fn reported_transient_manager_with_sshkey_and_pass() {
    // Regression for the reported failure: sshkey + transient mgr + pass. This
    // exact oscrc shape made `approve` fail with "supports only SSH-signature
    // auth" even though the sshkey was usable. The password must never be read,
    // nor leak into the credentials' Debug.
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_rsa");
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[{API}]\nuser = simonlm\nsshkey = {}\n\
             credentials_mgr_class = osc.credentials.TransientCredentialsManager\n\
             pass = s3cret-not-read\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read().unwrap();
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.user, "simonlm");
    assert!(!format!("{creds:?}").contains("s3cret-not-read"));
}

#[test]
#[serial(osc_config_env)]
fn unsupported_credentials_manager_without_sshkey_raises() {
    // With no usable key, a keyring/transient manager still fails closed: its
    // secret is not in the file and mtui never prompts.
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[{API}]\nuser = bob\n\
             credentials_mgr_class = osc.credentials.KeyringCredentialsManager\n"
        ),
        None,
    );
    let err = read().unwrap_err();
    assert!(err.to_string().contains("credentials_mgr_class"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn agent_fingerprint_sshkey_is_accepted() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = SHA256:abc123\n"),
        None,
    );
    let creds = read().unwrap();
    assert_eq!(creds.sshkey_fingerprint, Some("SHA256:abc123".to_owned()));
    assert_eq!(creds.sshkey_path, None);
    assert_eq!(creds.user, "bob");
}

#[test]
#[serial(osc_config_env)]
fn pub_only_key_on_disk_is_accepted() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let priv_key = dir.path().join("id_ed25519");
    fs::write(
        dir.path().join("id_ed25519.pub"),
        "ssh-ed25519 AAAA comment\n",
    )
    .unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", priv_key.display()),
        None,
    );
    let creds = read().unwrap();
    assert_eq!(creds.sshkey_path, Some(priv_key));
    assert_eq!(creds.sshkey_fingerprint, None);
}

#[test]
#[serial(osc_config_env)]
fn missing_key_file_raises() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let absent = dir.path().join("absent");
    write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", absent.display()),
        None,
    );
    let err = read().unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn unparsable_oscrc_raises() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        "not = ini = at = all\n[unclosed\n",
        None,
    );
    let err = read().unwrap_err();
    assert!(err.to_string().contains("could not parse"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn parse_error_does_not_leak_secret() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    // A value before any section is a parse error under a strict reader; its
    // source line (here a password) must not surface in the error message.
    write_oscrc(&guard, dir.path(), "pass = SUPERSECRET\n[general\n", None);
    let err = read().unwrap_err();
    assert!(!err.to_string().contains("SUPERSECRET"), "{err}");
}

#[test]
#[serial(osc_config_env)]
fn sshkey_inherited_from_general() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_ed25519");
    fs::write(&key, "dummy-key").unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[general]\nsshkey = {}\n\n[{API}]\nuser = bob\n",
            key.display()
        ),
        None,
    );
    let creds = read().unwrap();
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.user, "bob");
}

#[test]
#[serial(osc_config_env)]
fn general_credentials_manager_does_not_veto_host_sshkey() {
    // A [general] manager must not veto a per-host key.
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[general]\ncredentials_mgr_class = osc.credentials.KeyringCredentialsManager\n\n\
             [{API}]\nuser = bob\nsshkey = {}\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read().unwrap();
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.user, "bob");
}

#[test]
#[serial(osc_config_env)]
fn credentials_manager_not_inherited_from_general() {
    // `credentials_mgr_class` is host-section only (osc gives it no parent).
    // Discriminating case: with NO sshkey anywhere, a [general] manager must not
    // be picked up — the failure must be the plain "no 'sshkey'" one, never the
    // manager-specific message.
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    write_oscrc(
        &guard,
        dir.path(),
        &format!(
            "[general]\ncredentials_mgr_class = osc.credentials.KeyringCredentialsManager\n\n\
             [{API}]\nuser = bob\n"
        ),
        None,
    );
    let err = read().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("no 'sshkey'"), "{msg}");
    assert!(!msg.contains("credentials_mgr_class"), "{msg}");
}

#[test]
#[serial(osc_config_env)]
fn trailing_slash_section_header_matches() {
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}/]\nuser = bob\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    let creds = read().unwrap();
    assert_eq!(creds.user, "bob");
}

/// Run `f` under a scoped tracing subscriber that captures each event's message
/// **synchronously** into an in-memory buffer, returning the joined records.
///
/// Unlike an `fmt` subscriber writing through a `MakeWriter`, this appends the
/// rendered message the instant the event fires (no formatter/flush step), so the
/// captured buffer never depends on writer flush ordering under scheduler
/// pressure — the condition that made the previous `fmt`-based capture flake under
/// `cargo test --workspace`. The subscriber is a thread-local default via
/// `with_default`; every caller is `#[serial(osc_config_env)]`, so no concurrent
/// test in this binary races it.
#[cfg(unix)]
fn capture_logs(f: impl FnOnce()) -> String {
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    #[derive(Clone)]
    struct CaptureLayer(Arc<Mutex<Vec<String>>>);

    struct MessageVisitor(String);
    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                let _ = write!(self.0, "{value:?}");
            }
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = MessageVisitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    let records = Arc::new(Mutex::new(Vec::new()));
    let sub = Registry::default().with(CaptureLayer(records.clone()));
    tracing::subscriber::with_default(sub, f);
    records.lock().unwrap().join("\n")
}

/// Read `read_credentials` under a scoped tracing subscriber, returning the
/// captured warning output so a permission warning can be asserted.
#[cfg(unix)]
fn read_capturing_logs() -> String {
    capture_logs(|| {
        let _ = read();
    })
}

#[cfg(unix)]
#[test]
#[serial(osc_config_env)]
fn loose_permissions_warn() {
    use std::os::unix::fs::PermissionsExt;
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o644)).unwrap();
    let logs = read_capturing_logs();
    assert!(logs.contains("group/world-accessible"), "logs: {logs}");
}

#[cfg(unix)]
#[test]
#[serial(osc_config_env)]
fn tight_permissions_do_not_warn() {
    use std::os::unix::fs::PermissionsExt;
    let guard = EnvGuard::new();
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        &guard,
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o600)).unwrap();
    let logs = read_capturing_logs();
    assert!(!logs.contains("group/world-accessible"), "logs: {logs}");
}

#[test]
fn resolve_sshkey_absolute_path() {
    let (path, fp) = oscrc::resolve_sshkey("/etc/keys/obs").unwrap();
    assert_eq!(path, Some(PathBuf::from("/etc/keys/obs")));
    assert_eq!(fp, None);
}

#[test]
fn resolve_sshkey_fingerprint() {
    let (path, fp) = oscrc::resolve_sshkey("SHA256:abc123").unwrap();
    assert_eq!(path, None);
    assert_eq!(fp, Some("SHA256:abc123".to_owned()));
}

#[test]
fn resolve_sshkey_empty_raises() {
    let err = oscrc::resolve_sshkey("   ").unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
}

// --- oscrc discovery (osc identify_conf parity) --------------------------- //

#[test]
#[serial(osc_config_env)]
fn discovery_prefers_osc_config_env() {
    // $OSC_CONFIG wins over both XDG and ~/.oscrc, even when they exist.
    let guard = EnvGuard::new();
    let override_dir = TempDir::new().unwrap();
    let override_path = override_dir.path().join("custom-oscrc");
    fs::write(&override_path, "").unwrap();

    let xdg = guard.xdg_oscrc();
    fs::create_dir_all(xdg.parent().unwrap()).unwrap();
    fs::write(&xdg, "").unwrap();
    let home = guard.home_oscrc();
    fs::create_dir_all(home.parent().unwrap()).unwrap();
    fs::write(&home, "").unwrap();

    guard.set_osc_config(&override_path);
    assert_eq!(oscrc::default_conffile(), override_path);
}

#[test]
#[serial(osc_config_env)]
fn discovery_prefers_xdg_when_it_exists() {
    // The XDG oscrc is used when present (with no ~/.oscrc).
    let guard = EnvGuard::new();
    let xdg = guard.xdg_oscrc();
    fs::create_dir_all(xdg.parent().unwrap()).unwrap();
    fs::write(&xdg, "").unwrap();
    assert_eq!(oscrc::default_conffile(), xdg);
}

#[test]
#[serial(osc_config_env)]
fn discovery_falls_back_to_home_oscrc() {
    // ~/.oscrc is used when only it exists (no XDG file).
    let guard = EnvGuard::new();
    let home = guard.home_oscrc();
    fs::create_dir_all(home.parent().unwrap()).unwrap();
    fs::write(&home, "").unwrap();
    assert_eq!(oscrc::default_conffile(), home);
}

#[test]
#[serial(osc_config_env)]
fn discovery_default_is_xdg_when_nothing_exists() {
    // With neither file present, the XDG path is returned as the default.
    let guard = EnvGuard::new();
    assert_eq!(oscrc::default_conffile(), guard.xdg_oscrc());
}

#[cfg(unix)]
#[test]
#[serial(osc_config_env)]
fn discovery_warns_when_both_locations_exist() {
    // Both XDG and ~/.oscrc present: XDG wins and a warning is logged.
    let guard = EnvGuard::new();
    let xdg = guard.xdg_oscrc();
    fs::create_dir_all(xdg.parent().unwrap()).unwrap();
    fs::write(&xdg, "").unwrap();
    let home = guard.home_oscrc();
    fs::create_dir_all(home.parent().unwrap()).unwrap();
    fs::write(&home, "").unwrap();

    let mut result = None;
    let logs = capture_logs(|| result = Some(oscrc::default_conffile()));

    assert_eq!(result.unwrap(), xdg);
    assert!(
        logs.contains("multiple oscrc files detected"),
        "logs: {logs}"
    );
}
