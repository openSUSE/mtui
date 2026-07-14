//! Native reader for OBS/IBS credentials from `~/.oscrc` (no `osc` subprocess).
//!
//! Ported from upstream `mtui/data_sources/obs/oscrc.py`. Parses the user's
//! existing oscrc (genuine INI) and resolves the credentials for one apiurl (the
//! fixed `https://api.suse.de`) into a small [`ObsCredentials`]. mtui
//! authenticates with SSH-signature auth, so this reader deliberately does
//! **not** read `pass`/`passx` for that Signature-only target — pulling a
//! plaintext password into memory for a code path that never fires would be pure
//! exposure. Every failure is a typed, fail-closed [`ObsError::Config`] that
//! names the real oscrc file/section; there is no interactive prompt.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ini::Ini;
use tracing::warn;

use super::errors::ObsError;

/// An `sshkey` value like `SHA256:abc…` names a key held by the ssh agent by
/// fingerprint rather than a file on disk; the native backend resolves it
/// through the agent at signing time. Mirrors upstream `_FINGERPRINT_RE`
/// (`^[A-Z0-9]+:`).
fn is_fingerprint(value: &str) -> bool {
    match value.find(':') {
        // At least one leading char, all of them ASCII-uppercase or digit.
        Some(idx) if idx > 0 => value[..idx]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
        _ => false,
    }
}

/// `credentials_mgr_class` values that route credentials through a mechanism the
/// native SSH-signature backend cannot use. Mirrors upstream `_UNSUPPORTED_MGR`.
const UNSUPPORTED_MGR: [&str; 2] = ["keyring", "transient"];

/// Resolved OBS Signature-auth credentials for one apiurl.
///
/// Exactly one of `sshkey_path` (a private-key file on disk) or
/// `sshkey_fingerprint` (an ssh-agent key's `SHA256:…` fingerprint) identifies
/// the signing key. Carries **no password by construction**: the native backend
/// uses SSH signature auth against api.suse.de, so `pass`/`passx` are never read
/// for that target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsCredentials {
    /// The OBS API URL these credentials authenticate against.
    pub apiurl: String,
    /// The oscrc `user` for this apiurl (does not inherit from `[general]`).
    pub user: String,
    /// The path of the oscrc file these credentials were read from.
    pub source: String,
    /// A private-key file on disk, when `sshkey` named a path.
    pub sshkey_path: Option<PathBuf>,
    /// An ssh-agent key's `SHA256:…` fingerprint, when `sshkey` named one.
    pub sshkey_fingerprint: Option<String>,
}

/// Expand a leading `~` / `~/` in a path against `$HOME` (best effort).
///
/// Mirrors upstream `Path.expanduser`. A path without a leading `~` is returned
/// unchanged; a missing `$HOME` leaves the `~` in place (as upstream does).
fn expanduser(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// The user's home directory, from `$HOME` (Unix) / `%USERPROFILE%` fallback.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Test seam for the default oscrc path (`~/.oscrc`).
///
/// Mirrors upstream's monkeypatched `_default_conffile`: tests set this to a
/// temp file so `read_credentials(api, "")` can be exercised without touching
/// the real `~/.oscrc`.
static DEFAULT_CONFFILE_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// The default oscrc location (`~/.oscrc`), or a test override when set.
pub fn default_conffile() -> PathBuf {
    if let Some(p) = DEFAULT_CONFFILE_OVERRIDE.get() {
        return p.clone();
    }
    expanduser("~/.oscrc")
}

/// Resolve an oscrc `sshkey` value to `(path, fingerprint)`.
///
/// A `SHA256:…` (or other `ALG:…`) value names an ssh-agent key by fingerprint
/// and yields `(None, Some(fingerprint))`. Otherwise it is a private-key file: a
/// bare name (`id_ed25519`) resolves under `~/.ssh/`; a value containing `/` is
/// a literal (`~`-expanded) path, yielding `(Some(path), None)`.
///
/// Errors with [`ObsError::Config`] when the value is empty.
pub fn resolve_sshkey(raw: &str) -> Result<(Option<PathBuf>, Option<String>), ObsError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(ObsError::Config("oscrc 'sshkey' is empty".to_owned()));
    }
    if is_fingerprint(value) {
        return Ok((None, Some(value.to_owned())));
    }
    if value.contains('/') {
        return Ok((Some(expanduser(value)), None));
    }
    Ok((Some(expanduser("~/.ssh").join(value)), None))
}

/// A key is usable if its private file or its `.pub` (agent) counterpart exists.
///
/// A `.pub`-only key on disk is signed via an ssh-agent that holds the private
/// half (matched by public blob at auth time). Mirrors upstream `_key_available`.
fn key_available(key_path: &Path) -> bool {
    if key_path.is_file() {
        return true;
    }
    let mut pubkey = key_path.as_os_str().to_owned();
    pubkey.push(".pub");
    Path::new(&pubkey).is_file()
}

/// Warn (Unix only) when the oscrc file is group/world-accessible.
///
/// Mirrors upstream's `st_mode & (S_IRWXG | S_IRWXO)` check. On non-Unix targets
/// there are no such permission bits, so this is a no-op.
#[cfg(unix)]
fn warn_loose_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        // S_IRWXG (0o070) | S_IRWXO (0o007).
        if meta.permissions().mode() & 0o077 != 0 {
            warn!(
                "oscrc {} is group/world-accessible; tighten it to 0600",
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_loose_permissions(_path: &Path) {}

/// Read SSH-signature credentials for `apiurl` from oscrc.
///
/// `apiurl` is the OBS API URL whose oscrc section to read (its section header
/// must equal this value). `conffile` is an optional oscrc path override; empty
/// uses [`default_conffile`] (`~/.oscrc`).
///
/// Returns the resolved [`ObsCredentials`] (user + signing key). Errors with
/// [`ObsError::Config`] for any fault — missing/unreadable oscrc, missing
/// section/user/sshkey, an unsupported credentials manager, or a missing key
/// file. The message names the real failing file/section. Never prompts, never
/// leaks the offending source line.
pub fn read_credentials(apiurl: &str, conffile: &str) -> Result<ObsCredentials, ObsError> {
    let path = if conffile.is_empty() {
        default_conffile()
    } else {
        expanduser(conffile)
    };

    if !path.is_file() {
        return Err(ObsError::Config(format!(
            "osc config file not found: {}; create an oscrc with a [{apiurl}] \
             section (e.g. run 'osc -A {apiurl} whoami' once)",
            path.display()
        )));
    }

    warn_loose_permissions(&path);

    // Never interpolate the parser error: rust-ini's ParseError message is a
    // fixed "line:col expecting …" string (no source content), but staying with
    // a fixed message keeps the secret-leak guarantee independent of the parser.
    let ini = Ini::load_from_file(&path)
        .map_err(|_| ObsError::Config(format!("could not parse oscrc {}", path.display())))?;

    // osc normalises trailing path slashes when matching apiurl sections
    // (sanitize_apiurl), so [https://api.suse.de/] matches api.suse.de too.
    let wanted = apiurl.trim_end_matches('/');
    let section_name = ini
        .sections()
        .flatten()
        .find(|name| name.trim_end_matches('/') == wanted)
        .map(str::to_owned);
    let Some(section_name) = section_name else {
        return Err(ObsError::Config(format!(
            "oscrc {} has no [{apiurl}] section; the native OBS backend reads \
             credentials from the section whose header equals the apiurl",
            path.display()
        )));
    };

    // `sshkey` (and any credentials manager) inherit from [general] when the
    // host section omits them, matching osc's FromParent resolution; `user`
    // does not inherit (osc requires it per host).
    let inherited = |key: &str| -> String {
        let from_section = ini
            .get_from(Some(section_name.as_str()), key)
            .unwrap_or("")
            .trim();
        if !from_section.is_empty() {
            return from_section.to_owned();
        }
        ini.get_from(Some("general"), key)
            .unwrap_or("")
            .trim()
            .to_owned()
    };

    let mgr = inherited("credentials_mgr_class");
    if !mgr.is_empty() {
        let lower = mgr.to_lowercase();
        if UNSUPPORTED_MGR.iter().any(|bad| lower.contains(bad)) {
            return Err(ObsError::Config(format!(
                "oscrc [{apiurl}] uses credentials_mgr_class={mgr:?}; the native \
                 OBS backend supports only SSH-signature auth (an 'sshkey' entry) \
                 — keyring/transient-password managers are not supported"
            )));
        }
    }

    let user = ini
        .get_from(Some(section_name.as_str()), "user")
        .unwrap_or("")
        .trim();
    if user.is_empty() {
        return Err(ObsError::Config(format!("oscrc [{apiurl}] has no 'user'")));
    }

    let sshkey = inherited("sshkey");
    if sshkey.is_empty() {
        return Err(ObsError::Config(format!(
            "oscrc [{apiurl}] has no 'sshkey' (in the section or [general]); the \
             native OBS backend requires SSH-signature auth (plaintext-password \
             auth is not supported)"
        )));
    }
    // NB: 'pass'/'passx' are intentionally never read for this Signature-only
    // target — see the module docstring.

    let (key_path, fingerprint) = resolve_sshkey(&sshkey)?;
    if let Some(ref kp) = key_path
        && !key_available(kp)
    {
        return Err(ObsError::Config(format!(
            "ssh key {} (from oscrc sshkey={sshkey:?}) does not exist",
            kp.display()
        )));
    }

    Ok(ObsCredentials {
        apiurl: apiurl.to_owned(),
        user: user.to_owned(),
        source: path.display().to_string(),
        sshkey_path: key_path,
        sshkey_fingerprint: fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_sha256_prefix() {
        assert!(is_fingerprint("SHA256:abc123"));
        assert!(is_fingerprint("MD5:aa:bb"));
        assert!(!is_fingerprint("/etc/keys/obs"));
        assert!(!is_fingerprint("id_ed25519"));
        // lowercase alg is not an osc fingerprint locator
        assert!(!is_fingerprint("sha256:abc"));
        // leading ':' is not a fingerprint
        assert!(!is_fingerprint(":abc"));
    }

    #[test]
    fn resolve_sshkey_bare_name_goes_under_ssh_dir() {
        let (path, fp) = resolve_sshkey("id_ed25519").unwrap();
        assert_eq!(path, Some(expanduser("~/.ssh").join("id_ed25519")));
        assert_eq!(fp, None);
    }

    #[test]
    fn resolve_sshkey_absolute_path_is_literal() {
        let (path, fp) = resolve_sshkey("/etc/keys/obs").unwrap();
        assert_eq!(path, Some(PathBuf::from("/etc/keys/obs")));
        assert_eq!(fp, None);
    }

    #[test]
    fn resolve_sshkey_fingerprint() {
        let (path, fp) = resolve_sshkey("SHA256:abc123").unwrap();
        assert_eq!(path, None);
        assert_eq!(fp, Some("SHA256:abc123".to_owned()));
    }

    #[test]
    fn resolve_sshkey_empty_raises() {
        let err = resolve_sshkey("   ").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn default_conffile_used_when_conffile_empty() {
        // Mirrors upstream test_default_conffile_used_when_conffile_empty +
        // test_default_conffile_is_oscrc. The process-global override can be set
        // once, so both assertions live in this single test.
        let dir = tempfile::TempDir::new().unwrap();
        let key = dir.path().join("k");
        std::fs::write(&key, "dummy-key").unwrap();
        let oscrc_path = dir.path().join(".oscrc");
        std::fs::write(
            &oscrc_path,
            format!(
                "[https://api.suse.de]\nuser = bob\nsshkey = {}\n",
                key.display()
            ),
        )
        .unwrap();

        DEFAULT_CONFFILE_OVERRIDE
            .set(oscrc_path.clone())
            .expect("override claimed once per process");

        // The default now points at our temp oscrc (test seam parity with
        // upstream's monkeypatched _default_conffile).
        assert_eq!(default_conffile(), oscrc_path);

        // An empty conffile falls back to that default and reads it.
        let creds = read_credentials("https://api.suse.de", "").unwrap();
        assert_eq!(creds.user, "bob");
    }
}
