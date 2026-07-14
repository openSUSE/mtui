//! Golden tests for the native oscrc credential reader
//! (`mtui_datasources::obs::oscrc`), ported 1:1 from upstream
//! `tests/test_obs_oscrc.py`.
//!
//! Each case writes a throwaway oscrc under a `TempDir` and asserts the resolved
//! [`ObsCredentials`] or the fail-closed [`ObsError::Config`] message substring
//! upstream pins. No network, no real `~/.oscrc`, no interactive prompt.

use std::fs;
use std::path::{Path, PathBuf};

use mtui_datasources::obs::oscrc::{self, ObsCredentials};
use tempfile::TempDir;

const API: &str = "https://api.suse.de";

/// Write an oscrc file (and, when given, a dummy key file it references).
fn write_oscrc(dir: &Path, body: &str, keyfile: Option<&Path>) -> PathBuf {
    if let Some(kf) = keyfile {
        fs::write(kf, "dummy-key").unwrap();
    }
    let path = dir.join("oscrc");
    fs::write(&path, body).unwrap();
    path
}

fn read(conf: &Path) -> Result<ObsCredentials, mtui_datasources::obs::ObsError> {
    oscrc::read_credentials(API, conf.to_str().unwrap())
}

#[test]
fn reads_user_and_sshkey() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_ed25519");
    let conf = write_oscrc(
        dir.path(),
        &format!(
            "[general]\napiurl = {API}\n\n[{API}]\nuser = bob\nsshkey = {}\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read(&conf).unwrap();
    assert_eq!(creds.user, "bob");
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.apiurl, API);
    assert_eq!(creds.source, conf.display().to_string());
}

#[test]
fn password_is_never_read_for_signature_target() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_ed25519");
    let conf = write_oscrc(
        dir.path(),
        &format!(
            "[{API}]\nuser = bob\npass = s3cret\npassx = AAAA==\nsshkey = {}\n",
            key.display()
        ),
        Some(&key),
    );
    let creds = read(&conf).unwrap();
    assert_eq!(creds.user, "bob");
    // The struct structurally cannot carry a password, and its Debug never
    // surfaces one.
    assert!(!format!("{creds:?}").contains("s3cret"));
}

#[test]
fn missing_conffile_raises() {
    let dir = TempDir::new().unwrap();
    let err = read(&dir.path().join("nope")).unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

#[test]
fn missing_section_raises() {
    let dir = TempDir::new().unwrap();
    let conf = write_oscrc(dir.path(), "[https://api.opensuse.org]\nuser = bob\n", None);
    let err = read(&conf).unwrap_err();
    assert!(
        err.to_string().contains("no [https://api.suse.de] section"),
        "{err}"
    );
}

#[test]
fn missing_user_raises() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}]\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    let err = read(&conf).unwrap_err();
    assert!(err.to_string().contains("no 'user'"), "{err}");
}

#[test]
fn missing_sshkey_raises() {
    let dir = TempDir::new().unwrap();
    let conf = write_oscrc(dir.path(), &format!("[{API}]\nuser = bob\n"), None);
    let err = read(&conf).unwrap_err();
    assert!(err.to_string().contains("no 'sshkey'"), "{err}");
}

#[test]
fn unsupported_credentials_manager_raises() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        dir.path(),
        &format!(
            "[{API}]\nuser = bob\nsshkey = {}\n\
             credentials_mgr_class = osc.credentials.KeyringCredentialsManager\n",
            key.display()
        ),
        Some(&key),
    );
    let err = read(&conf).unwrap_err();
    assert!(err.to_string().contains("credentials_mgr_class"), "{err}");
}

#[test]
fn agent_fingerprint_sshkey_is_accepted() {
    let dir = TempDir::new().unwrap();
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = SHA256:abc123\n"),
        None,
    );
    let creds = read(&conf).unwrap();
    assert_eq!(creds.sshkey_fingerprint, Some("SHA256:abc123".to_owned()));
    assert_eq!(creds.sshkey_path, None);
    assert_eq!(creds.user, "bob");
}

#[test]
fn pub_only_key_on_disk_is_accepted() {
    let dir = TempDir::new().unwrap();
    let priv_key = dir.path().join("id_ed25519");
    fs::write(
        dir.path().join("id_ed25519.pub"),
        "ssh-ed25519 AAAA comment\n",
    )
    .unwrap();
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", priv_key.display()),
        None,
    );
    let creds = read(&conf).unwrap();
    assert_eq!(creds.sshkey_path, Some(priv_key));
    assert_eq!(creds.sshkey_fingerprint, None);
}

#[test]
fn missing_key_file_raises() {
    let dir = TempDir::new().unwrap();
    let absent = dir.path().join("absent");
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", absent.display()),
        None,
    );
    let err = read(&conf).unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
fn unparsable_oscrc_raises() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("oscrc");
    fs::write(&conf, "not = ini = at = all\n[unclosed\n").unwrap();
    let err = read(&conf).unwrap_err();
    assert!(err.to_string().contains("could not parse"), "{err}");
}

#[test]
fn parse_error_does_not_leak_secret() {
    let dir = TempDir::new().unwrap();
    let conf = dir.path().join("oscrc");
    // A value before any section is a parse error under a strict reader; its
    // source line (here a password) must not surface in the error message.
    fs::write(&conf, "pass = SUPERSECRET\n[general\n").unwrap();
    let err = read(&conf).unwrap_err();
    assert!(!err.to_string().contains("SUPERSECRET"), "{err}");
}

#[test]
fn sshkey_inherited_from_general() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("id_ed25519");
    fs::write(&key, "dummy-key").unwrap();
    let conf = write_oscrc(
        dir.path(),
        &format!(
            "[general]\nsshkey = {}\n\n[{API}]\nuser = bob\n",
            key.display()
        ),
        None,
    );
    let creds = read(&conf).unwrap();
    assert_eq!(creds.sshkey_path, Some(key));
    assert_eq!(creds.user, "bob");
}

#[test]
fn credentials_manager_inherited_from_general() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        dir.path(),
        &format!(
            "[general]\ncredentials_mgr_class = osc.credentials.KeyringCredentialsManager\n\n\
             [{API}]\nuser = bob\nsshkey = {}\n",
            key.display()
        ),
        Some(&key),
    );
    let err = read(&conf).unwrap_err();
    assert!(err.to_string().contains("credentials_mgr_class"), "{err}");
}

#[test]
fn trailing_slash_section_header_matches() {
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}/]\nuser = bob\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    let creds = read(&conf).unwrap();
    assert_eq!(creds.user, "bob");
}

/// Read `read_credentials` under a scoped tracing subscriber, returning the
/// captured stderr-style output so a warning can be asserted.
#[cfg(unix)]
fn read_capturing_logs(conf: &Path) -> String {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct BufMaker(Arc<Mutex<Vec<u8>>>);
    struct BufWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for BufMaker {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            BufWriter(self.0.clone())
        }
    }

    let buf = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(BufMaker(buf.clone()))
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::with_default(sub, || {
        let _ = read(conf);
    });
    String::from_utf8(buf.lock().unwrap().clone()).unwrap()
}

#[cfg(unix)]
#[test]
fn loose_permissions_warn() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o644)).unwrap();
    let logs = read_capturing_logs(&conf);
    assert!(logs.contains("group/world-accessible"), "logs: {logs}");
}

#[cfg(unix)]
#[test]
fn tight_permissions_do_not_warn() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let key = dir.path().join("k");
    let conf = write_oscrc(
        dir.path(),
        &format!("[{API}]\nuser = bob\nsshkey = {}\n", key.display()),
        Some(&key),
    );
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o600)).unwrap();
    let logs = read_capturing_logs(&conf);
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
