//! Typed configuration options and their defaults.
//!
//! This is the **Phase-1 subset** of upstream `mtui/support/config.py`'s option
//! table, since extended with the `[lock]` section (Phase 2). Options belonging
//! to later phases — `mcp_*` (Phase 7), `openqa_*`/`teregen_*`/`qem_dashboard_*`
//! (Phase 3) — are deliberately omitted; they will be added, additively, as
//! their sections land.
//!
//! Every default here matches the corresponding upstream default value exactly,
//! preserving behavioural parity for the options mtui-rs already understands.
//!
//! ## Shape
//!
//! The on-disk format is **sectioned TOML** (`[mtui]`, `[connection]`,
//! `[refhosts]`, `[url]`, `[svn]`, `[target]`, `[lock]`). `RawConfig` mirrors that
//! structure for serde; [`Config`] is the flattened, fully-typed view the rest
//! of the workspace consumes. Every serde field defaults, so an empty (or
//! partial) TOML document deserialises into all-defaults.

use std::path::PathBuf;

use serde::Deserialize;
use serde::de::{self, Deserializer};

use crate::paths::expanduser;

/// TLS certificate-verification policy for outbound HTTP.
///
/// Accepts three shapes in TOML, all under `[mtui] ssl_verify`:
///
/// * a native boolean — `ssl_verify = true` / `ssl_verify = false`;
/// * a boolean *spelling* string — `"yes"`, `"no"`, `"on"`, `"off"`, `"1"`,
///   `"0"`, `"true"`, `"false"` (case-insensitive), matching upstream
///   `_parse_ssl_verify`;
/// * any other string — treated as a path to a custom CA bundle/certificate,
///   e.g. `ssl_verify = "/my/own/cert.pem"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SslVerify {
    /// Verify certificates against the system trust store (the default).
    #[default]
    Enabled,
    /// Skip certificate verification entirely.
    Disabled,
    /// Verify against the CA bundle / certificate at this path.
    CaBundle(PathBuf),
}

impl SslVerify {
    /// Coerce a raw string value, following upstream's accepted spellings.
    ///
    /// A recognised boolean spelling maps to [`Enabled`](Self::Enabled) /
    /// [`Disabled`](Self::Disabled); anything else is a CA bundle path.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let token = raw.trim();
        match token.to_ascii_lowercase().as_str() {
            "1" | "yes" | "true" | "on" => Self::Enabled,
            "0" | "no" | "false" | "off" => Self::Disabled,
            _ => Self::CaBundle(PathBuf::from(token)),
        }
    }

    /// Map a native boolean to the enabled/disabled variants.
    #[must_use]
    pub fn from_bool(verify: bool) -> Self {
        if verify {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

impl<'de> Deserialize<'de> for SslVerify {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SslVerifyVisitor;

        impl de::Visitor<'_> for SslVerifyVisitor {
            type Value = SslVerify;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a boolean, or a string (boolean spelling or a path to a CA bundle)")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(SslVerify::from_bool(v))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(SslVerify::parse(v))
            }
        }

        deserializer.deserialize_any(SslVerifyVisitor)
    }
}

// -- Upstream default helpers (used both by serde and by `Config::default`). --

pub(crate) fn default_connection_timeout() -> u64 {
    300
}
pub(crate) fn default_svn_path() -> String {
    "svn+ssh://svn@qam.suse.de/testreports".to_owned()
}
pub(crate) fn default_bugzilla_url() -> String {
    "https://bugzilla.suse.com".to_owned()
}
pub(crate) fn default_reports_url() -> String {
    "https://qam.suse.de/testreports".to_owned()
}
pub(crate) fn default_fancy_reports_url() -> String {
    "https://qam.suse.de/reports".to_owned()
}
pub(crate) fn default_refhosts_resolvers() -> String {
    "https,path".to_owned()
}
pub(crate) fn default_refhosts_https_uri() -> String {
    "https://qam.suse.de/refhosts/refhosts.yml".to_owned()
}
pub(crate) fn default_refhosts_https_expiration() -> u64 {
    3600 * 12
}
pub(crate) fn default_refhosts_path() -> PathBuf {
    PathBuf::from("/usr/share/qam-metadata/refhosts.yml")
}
pub(crate) fn default_install_logs() -> PathBuf {
    PathBuf::from("install_logs")
}
pub(crate) fn default_target_tempdir() -> PathBuf {
    PathBuf::from("/tmp")
}
pub(crate) fn default_ssh_strict_host_key_checking() -> String {
    "auto_add".to_owned()
}
pub(crate) fn default_template_dir() -> PathBuf {
    // Upstream: Path(getenv("TEMPLATE_DIR", ".")).
    std::env::var_os("TEMPLATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
pub(crate) fn default_local_tempdir() -> PathBuf {
    // Upstream: Path(getenv("TMPDIR", "/tmp")).
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
pub(crate) fn default_session_user() -> String {
    // Upstream: getpass.getuser(). Fall back to $USER / "unknown".
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}
pub(crate) fn default_lock_reap_stale() -> bool {
    true
}
pub(crate) fn default_lock_stale_age() -> u64 {
    86400
}
pub(crate) fn default_lock_pi_autolock() -> bool {
    true
}
pub(crate) fn default_lock_wait() -> u64 {
    0
}
pub(crate) fn default_lock_wait_poll() -> u64 {
    15
}

// -- Serde section structs (mirror the TOML tables) --------------------------

/// `[mtui]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct MtuiSection {
    pub template_dir: Option<PathBuf>,
    pub tempdir: Option<PathBuf>,
    pub user: Option<String>,
    pub install_logs: Option<PathBuf>,
    pub chdir_to_template_dir: Option<bool>,
    pub use_keyring: Option<bool>,
    pub ssl_verify: Option<SslVerify>,
}

/// `[connection]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ConnectionSection {
    pub connection_timeout: Option<u64>,
    pub ssh_strict_host_key_checking: Option<String>,
}

/// `[refhosts]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RefhostsSection {
    pub resolvers: Option<String>,
    pub https_uri: Option<String>,
    pub https_expiration: Option<u64>,
    pub path: Option<PathBuf>,
}

/// `[url]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct UrlSection {
    pub bugzilla: Option<String>,
    pub testreports: Option<String>,
    pub fancy_reports: Option<String>,
}

/// `[svn]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SvnSection {
    pub path: Option<String>,
}

/// `[target]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TargetSection {
    pub tempdir: Option<PathBuf>,
}

/// `[gitea]` table — credentials for the Gitea PR review workflow.
///
/// Mirrors upstream `mtui/support/config.py`'s `gitea_token` option (INI
/// `[gitea] token`). The Gitea connector refuses to build without it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct GiteaSection {
    pub token: Option<String>,
}

/// `[lock]` table — remote-lock behaviour on target hosts.
///
/// Mirrors upstream `mtui/support/config.py`'s `lock_*` options (which live
/// under the `[lock]` INI section): stale-lock reaping on connect and the
/// host-arbitration pool-claim wait queue.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct LockSection {
    pub reap_stale: Option<bool>,
    pub stale_age: Option<u64>,
    pub pi_autolock: Option<bool>,
    pub wait: Option<u64>,
    pub wait_poll: Option<u64>,
}

/// Raw, deserialised view of a single TOML document.
///
/// Every field is optional so a partial file leaves absent options untouched
/// during the merge; the flattening into [`Config`] applies defaults last.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RawConfig {
    pub mtui: MtuiSection,
    pub connection: ConnectionSection,
    pub refhosts: RefhostsSection,
    pub url: UrlSection,
    pub svn: SvnSection,
    pub target: TargetSection,
    pub gitea: GiteaSection,
    pub lock: LockSection,
}

impl RawConfig {
    /// Merge `other` on top of `self`: any option **set** in `other` wins.
    pub(crate) fn merge(&mut self, other: RawConfig) {
        macro_rules! take {
            ($sec:ident, $field:ident) => {
                if other.$sec.$field.is_some() {
                    self.$sec.$field = other.$sec.$field;
                }
            };
        }
        take!(mtui, template_dir);
        take!(mtui, tempdir);
        take!(mtui, user);
        take!(mtui, install_logs);
        take!(mtui, chdir_to_template_dir);
        take!(mtui, use_keyring);
        take!(mtui, ssl_verify);
        take!(connection, connection_timeout);
        take!(connection, ssh_strict_host_key_checking);
        take!(refhosts, resolvers);
        take!(refhosts, https_uri);
        take!(refhosts, https_expiration);
        take!(refhosts, path);
        take!(url, bugzilla);
        take!(url, testreports);
        take!(url, fancy_reports);
        take!(svn, path);
        take!(target, tempdir);
        take!(gitea, token);
        take!(lock, reap_stale);
        take!(lock, stale_age);
        take!(lock, pi_autolock);
        take!(lock, wait);
        take!(lock, wait_poll);
    }
}

/// Fully-typed configuration consumed by the rest of the workspace.
///
/// Construct via [`Config::load`](crate::Config::load) (file/env/defaults) or
/// [`Config::default`] (all upstream defaults). Path-typed options have `~`
/// expanded to the user's home directory, matching upstream `expanduser`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    // [mtui]
    /// Directory holding checked-out test-report templates.
    pub template_dir: PathBuf,
    /// Local scratch directory.
    pub local_tempdir: PathBuf,
    /// User name attributed to this session (locks, logs).
    pub session_user: String,
    /// Directory where install logs are written.
    pub install_logs: PathBuf,
    /// Whether to `chdir` into the template dir on load.
    pub chdir_to_template_dir: bool,
    /// Whether to read secrets from the system keyring.
    pub use_keyring: bool,
    /// TLS verification policy for outbound HTTP.
    pub ssl_verify: SslVerify,

    // [connection]
    /// SSH connect + command timeout, in seconds.
    pub connection_timeout: u64,
    /// SSH host-key checking policy (`auto_add`, `strict`, `warn`, ...).
    pub ssh_strict_host_key_checking: String,

    // [refhosts]
    /// Comma-separated ordered list of refhosts resolvers.
    pub refhosts_resolvers: String,
    /// HTTPS URI of the refhosts database.
    pub refhosts_https_uri: String,
    /// Seconds before a cached refhosts HTTPS fetch is considered stale.
    pub refhosts_https_expiration: u64,
    /// Local filesystem path to a refhosts database.
    pub refhosts_path: PathBuf,

    // [url]
    /// Bugzilla base URL.
    pub bugzilla_url: String,
    /// Test-reports base URL.
    pub reports_url: String,
    /// "Fancy" reports base URL.
    pub fancy_reports_url: String,

    // [svn]
    /// SVN base path for test-report checkout.
    pub svn_path: String,

    // [gitea]
    /// API token for the Gitea PR review workflow. Empty by default; the Gitea
    /// connector refuses to build without it.
    pub gitea_token: String,

    // [target]
    /// Remote scratch directory on target hosts.
    pub target_tempdir: PathBuf,

    // [lock]
    /// On connect, force-remove a pre-existing remote lock older than
    /// [`lock_stale_age`](Self::lock_stale_age) seconds regardless of owner.
    pub lock_reap_stale: bool,
    /// Age (seconds) beyond which a remote lock is considered stale and reapable.
    /// A non-positive value disables reaping.
    pub lock_stale_age: u64,
    /// When testing a Product Increment (PI), auto-lock all reference hosts on
    /// `assign` and unlock them at end of testing.
    pub lock_pi_autolock: bool,
    /// Host-arbitration pool-claim queueing budget, in seconds. `0` (the
    /// default) fails fast on a busy host.
    pub lock_wait: u64,
    /// Poll interval (seconds) while waiting for a busy pool lock to free.
    pub lock_wait_poll: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            template_dir: default_template_dir(),
            local_tempdir: default_local_tempdir(),
            session_user: default_session_user(),
            install_logs: default_install_logs(),
            chdir_to_template_dir: false,
            use_keyring: false,
            ssl_verify: SslVerify::Enabled,
            connection_timeout: default_connection_timeout(),
            ssh_strict_host_key_checking: default_ssh_strict_host_key_checking(),
            refhosts_resolvers: default_refhosts_resolvers(),
            refhosts_https_uri: default_refhosts_https_uri(),
            refhosts_https_expiration: default_refhosts_https_expiration(),
            refhosts_path: default_refhosts_path(),
            bugzilla_url: default_bugzilla_url(),
            reports_url: default_reports_url(),
            fancy_reports_url: default_fancy_reports_url(),
            svn_path: default_svn_path(),
            gitea_token: String::new(),
            target_tempdir: default_target_tempdir(),
            lock_reap_stale: default_lock_reap_stale(),
            lock_stale_age: default_lock_stale_age(),
            lock_pi_autolock: default_lock_pi_autolock(),
            lock_wait: default_lock_wait(),
            lock_wait_poll: default_lock_wait_poll(),
        }
    }
}

impl Config {
    /// Flatten a merged [`RawConfig`] into a fully-typed `Config`, applying
    /// defaults for absent options and expanding `~` in path options.
    pub(crate) fn from_raw(raw: RawConfig) -> Self {
        let d = Config::default();
        Self {
            template_dir: raw
                .mtui
                .template_dir
                .map_or(d.template_dir, |p| expanduser(&p)),
            local_tempdir: raw.mtui.tempdir.map_or(d.local_tempdir, |p| expanduser(&p)),
            session_user: raw.mtui.user.unwrap_or(d.session_user),
            install_logs: raw
                .mtui
                .install_logs
                .map_or(d.install_logs, |p| expanduser(&p)),
            chdir_to_template_dir: raw
                .mtui
                .chdir_to_template_dir
                .unwrap_or(d.chdir_to_template_dir),
            use_keyring: raw.mtui.use_keyring.unwrap_or(d.use_keyring),
            ssl_verify: raw.mtui.ssl_verify.unwrap_or(d.ssl_verify),
            connection_timeout: raw
                .connection
                .connection_timeout
                .unwrap_or(d.connection_timeout),
            ssh_strict_host_key_checking: raw
                .connection
                .ssh_strict_host_key_checking
                .unwrap_or(d.ssh_strict_host_key_checking),
            refhosts_resolvers: raw.refhosts.resolvers.unwrap_or(d.refhosts_resolvers),
            refhosts_https_uri: raw.refhosts.https_uri.unwrap_or(d.refhosts_https_uri),
            refhosts_https_expiration: raw
                .refhosts
                .https_expiration
                .unwrap_or(d.refhosts_https_expiration),
            refhosts_path: raw
                .refhosts
                .path
                .map_or(d.refhosts_path, |p| expanduser(&p)),
            bugzilla_url: raw.url.bugzilla.unwrap_or(d.bugzilla_url),
            reports_url: raw.url.testreports.unwrap_or(d.reports_url),
            fancy_reports_url: raw.url.fancy_reports.unwrap_or(d.fancy_reports_url),
            svn_path: raw.svn.path.unwrap_or(d.svn_path),
            gitea_token: raw.gitea.token.unwrap_or(d.gitea_token),
            target_tempdir: raw
                .target
                .tempdir
                .map_or(d.target_tempdir, |p| expanduser(&p)),
            lock_reap_stale: raw.lock.reap_stale.unwrap_or(d.lock_reap_stale),
            lock_stale_age: raw.lock.stale_age.unwrap_or(d.lock_stale_age),
            lock_pi_autolock: raw.lock.pi_autolock.unwrap_or(d.lock_pi_autolock),
            lock_wait: raw.lock.wait.unwrap_or(d.lock_wait),
            lock_wait_poll: raw.lock.wait_poll.unwrap_or(d.lock_wait_poll),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_upstream_scalars() {
        let c = Config::default();
        assert_eq!(c.connection_timeout, 300);
        assert_eq!(c.refhosts_https_expiration, 3600 * 12);
        assert!(!c.chdir_to_template_dir);
        assert!(!c.use_keyring);
        assert_eq!(c.ssl_verify, SslVerify::Enabled);
        assert_eq!(c.ssh_strict_host_key_checking, "auto_add");
        assert_eq!(c.refhosts_resolvers, "https,path");
        assert_eq!(c.bugzilla_url, "https://bugzilla.suse.com");
        assert_eq!(c.reports_url, "https://qam.suse.de/testreports");
        assert_eq!(c.fancy_reports_url, "https://qam.suse.de/reports");
        assert_eq!(c.svn_path, "svn+ssh://svn@qam.suse.de/testreports");
        assert_eq!(
            c.refhosts_https_uri,
            "https://qam.suse.de/refhosts/refhosts.yml"
        );
        assert_eq!(
            c.refhosts_path,
            PathBuf::from("/usr/share/qam-metadata/refhosts.yml")
        );
        assert_eq!(c.install_logs, PathBuf::from("install_logs"));
        assert_eq!(c.target_tempdir, PathBuf::from("/tmp"));
        // [lock] defaults mirror upstream config.py exactly.
        assert!(c.lock_reap_stale);
        assert_eq!(c.lock_stale_age, 86400);
        assert!(c.lock_pi_autolock);
        assert_eq!(c.lock_wait, 0);
        assert_eq!(c.lock_wait_poll, 15);
    }

    #[test]
    fn gitea_token_defaults_empty_and_parses() {
        // Default is empty (the connector refuses to build without a token).
        assert_eq!(Config::default().gitea_token, "");
        // A [gitea] table sets it.
        let raw: RawConfig = toml::from_str("[gitea]\ntoken = \"abc123\"\n").unwrap();
        assert_eq!(Config::from_raw(raw).gitea_token, "abc123");
    }

    #[test]
    fn lock_section_parses_and_overrides() {
        let raw: RawConfig = toml::from_str(
            "[lock]\nreap_stale = false\nstale_age = 3600\npi_autolock = false\nwait = 30\nwait_poll = 5\n",
        )
        .unwrap();
        let c = Config::from_raw(raw);
        assert!(!c.lock_reap_stale);
        assert_eq!(c.lock_stale_age, 3600);
        assert!(!c.lock_pi_autolock);
        assert_eq!(c.lock_wait, 30);
        assert_eq!(c.lock_wait_poll, 5);
    }

    #[test]
    fn lock_section_partial_keeps_defaults() {
        // A partial [lock] table leaves absent keys at their upstream defaults.
        let raw: RawConfig = toml::from_str("[lock]\nwait = 45\n").unwrap();
        let c = Config::from_raw(raw);
        assert_eq!(c.lock_wait, 45);
        assert!(c.lock_reap_stale);
        assert_eq!(c.lock_stale_age, 86400);
        assert_eq!(c.lock_wait_poll, 15);
    }

    #[test]
    fn ssl_verify_boolean_spellings() {
        for t in ["1", "yes", "true", "on", "TRUE", "On", " yes "] {
            assert_eq!(SslVerify::parse(t), SslVerify::Enabled, "{t:?}");
        }
        for f in ["0", "no", "false", "off", "FALSE", "Off"] {
            assert_eq!(SslVerify::parse(f), SslVerify::Disabled, "{f:?}");
        }
    }

    #[test]
    fn ssl_verify_path_bundle() {
        assert_eq!(
            SslVerify::parse("/etc/ssl/ca.pem"),
            SslVerify::CaBundle(PathBuf::from("/etc/ssl/ca.pem"))
        );
    }

    #[test]
    fn ssl_verify_from_bool() {
        assert_eq!(SslVerify::from_bool(true), SslVerify::Enabled);
        assert_eq!(SslVerify::from_bool(false), SslVerify::Disabled);
    }

    #[test]
    fn ssl_verify_deserializes_native_bool() {
        // `ssl_verify = false` (native TOML boolean) must disable verification,
        // not silently fall back to the default.
        let raw: RawConfig = toml::from_str("[mtui]\nssl_verify = false\n").unwrap();
        assert_eq!(raw.mtui.ssl_verify, Some(SslVerify::Disabled));

        let raw: RawConfig = toml::from_str("[mtui]\nssl_verify = true\n").unwrap();
        assert_eq!(raw.mtui.ssl_verify, Some(SslVerify::Enabled));
    }

    #[test]
    fn ssl_verify_deserializes_string_forms() {
        // Boolean spelling as a string.
        let raw: RawConfig = toml::from_str("[mtui]\nssl_verify = \"off\"\n").unwrap();
        assert_eq!(raw.mtui.ssl_verify, Some(SslVerify::Disabled));

        // Path to a custom certificate.
        let raw: RawConfig = toml::from_str("[mtui]\nssl_verify = \"/my/own/cert.pem\"\n").unwrap();
        assert_eq!(
            raw.mtui.ssl_verify,
            Some(SslVerify::CaBundle(PathBuf::from("/my/own/cert.pem")))
        );
    }

    #[test]
    fn from_raw_applies_values_and_defaults() {
        let mut raw = RawConfig::default();
        raw.connection.connection_timeout = Some(450);
        raw.mtui.chdir_to_template_dir = Some(true);
        raw.url.bugzilla = Some("https://bugzilla.example.com".to_owned());
        let c = Config::from_raw(raw);
        // Overridden.
        assert_eq!(c.connection_timeout, 450);
        assert!(c.chdir_to_template_dir);
        assert_eq!(c.bugzilla_url, "https://bugzilla.example.com");
        // Untouched falls back to default.
        assert_eq!(c.reports_url, "https://qam.suse.de/testreports");
    }

    #[test]
    fn merge_later_wins_on_shared_keys() {
        let mut base = RawConfig::default();
        base.connection.connection_timeout = Some(100);
        base.url.bugzilla = Some("base".to_owned());
        let mut over = RawConfig::default();
        over.connection.connection_timeout = Some(999);
        base.merge(over);
        assert_eq!(base.connection.connection_timeout, Some(999));
        // A key not set in `over` is preserved from base.
        assert_eq!(base.url.bugzilla.as_deref(), Some("base"));
    }

    #[test]
    fn tilde_expansion_in_path_options() {
        if let Some(base) = directories::BaseDirs::new() {
            let home = base.home_dir().to_path_buf();
            let mut raw = RawConfig::default();
            raw.refhosts.path = Some(PathBuf::from("~/qam/refhosts.yml"));
            let c = Config::from_raw(raw);
            assert_eq!(c.refhosts_path, home.join("qam/refhosts.yml"));
        }
    }
}
