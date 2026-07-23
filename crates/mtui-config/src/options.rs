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

// -- Parse-time validators (mirror upstream `config.py` `_parse_*`). ----------
//
// Upstream validates a handful of options at parse time and, on failure, logs at
// ERROR and falls back to the option's default (per-option, never hard-failing).
// mtui-rs applies the same per-field fallback in `Config::from_raw`. Rust's typed
// `u64`/`usize` fields already reject non-numeric and negative literals at TOML
// deserialise time, so the positive-int guard reduces to rejecting `0`.

/// Validate an `http(s)` endpoint URL, mirroring upstream `_parse_base_url`.
///
/// Requires an `http` or `https` scheme, a non-empty host, and — when a port is
/// present — a numeric one. This is deliberately lenient (matching Python's
/// `urlsplit`, not full RFC 3986): a bad value like `https://openqa.suse.de:44e3`
/// is rejected, but exotic-yet-usable forms are accepted.
pub(crate) fn validate_base_url(raw: &str) -> bool {
    let token = raw.trim();
    let Some((scheme, rest)) = token.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    // Authority is everything up to the first `/`, `?`, or `#`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip optional `user[:pass]@` userinfo.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    if host_port.is_empty() {
        return false;
    }
    // IPv6 literal: `[..]` optionally followed by `:port`.
    if let Some(after_bracket) = host_port.strip_prefix('[') {
        let Some((host, tail)) = after_bracket.split_once(']') else {
            return false; // unclosed IPv6 bracket
        };
        if host.is_empty() {
            return false;
        }
        return match tail.strip_prefix(':') {
            None if tail.is_empty() => true,
            None => false, // junk after `]` that is not a `:port`
            Some(port) => is_numeric_port(port),
        };
    }
    // Regular host: split off a trailing `:port`, if any.
    match host_port.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && is_numeric_port(port),
        None => true,
    }
}

/// A port is valid when non-empty, all ASCII digits, and parses as `u16`.
fn is_numeric_port(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok()
}

/// Validate `[mtui] install_logs` as a single relative directory name, mirroring
/// upstream `_parse_install_logs`.
///
/// The value is joined per update as `template_dir / <rrid> / install_logs`;
/// an empty, absolute, separator-containing, or `.`/`..` value would crash or
/// silently escape the base path, so it is rejected here.
pub(crate) fn is_relative_dir_name(raw: &str) -> bool {
    let token = raw.trim();
    !token.is_empty()
        && !token.contains('/')
        && !std::path::Path::new(token).is_absolute()
        && token != "."
        && token != ".."
}

// -- Upstream default helpers (used both by serde and by `Config::default`). --

pub(crate) fn default_connection_timeout() -> u64 {
    300
}
pub(crate) fn default_reboot_timeout() -> u64 {
    10
}
pub(crate) fn default_reboot_retries() -> u64 {
    10
}
pub(crate) fn default_max_parallel() -> u64 {
    50
}
pub(crate) fn default_max_oqa_parallel() -> u64 {
    8
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
pub(crate) fn default_qem_dashboard_api() -> String {
    "http://dashboard.qam.suse.de/api".to_owned()
}
pub(crate) fn default_teregen_api() -> String {
    "https://qam.suse.de/api/v1".to_owned()
}
pub(crate) fn default_openqa_instance() -> String {
    "https://openqa.suse.de".to_owned()
}
pub(crate) fn default_openqa_instance_baremetal() -> String {
    "http://openqa.qam.suse.cz".to_owned()
}
/// Default trusted Gitea origin the PR-review token may be sent to.
///
/// A redesign addition with no upstream counterpart (upstream mtui derives the
/// Gitea host solely from attacker-influenceable checked-out metadata). The
/// token is only attached to requests whose origin matches this value, so it
/// defaults to the internal SUSE Gitea (`src.suse.de`) that serves SLFO so the
/// standard workflow keeps working out-of-the-box; point it elsewhere (e.g.
/// `https://src.opensuse.org`) for other instances.
pub(crate) fn default_gitea_url() -> String {
    "https://src.suse.de".to_owned()
}
pub(crate) fn default_openqa_install_distri() -> String {
    "sle".to_owned()
}
/// The Slack integration is **opt-in**: off unless a site turns it on.
///
/// Posting to a chat workspace is an outward-facing side effect, so it should
/// never happen because a default said so. Most mtui users have no Slack
/// integration at all, and for them the feature staying dark — and saying why
/// when invoked — is the correct behaviour. Turning it on is one explicit
/// `enabled = true` alongside the token and channel it needs anyway.
pub(crate) fn default_slack_enabled() -> bool {
    false
}
/// Trusted Slack API origin the [`slack_token`](Config::slack_token) may be
/// sent to, mirroring the `gitea_url` reasoning: the token is only ever
/// attached to requests against this base, so pointing it elsewhere is an
/// explicit, auditable act rather than something a checked-out template can
/// influence. Overridable mainly so tests can aim at a local mock server.
pub(crate) fn default_slack_api_url() -> String {
    "https://slack.com/api".to_owned()
}
/// Seconds between reaction polls while watching a review request. Slack's
/// Web API tier-3 methods allow ~50 requests/minute; two minutes per poll keeps
/// a multi-template watch far inside that even before jitter.
pub(crate) fn default_slack_poll_interval() -> u64 {
    120
}
/// Seconds a `request_review` watch runs before giving up, defaulting to one
/// hour: long enough for a reviewer to notice, short enough that a forgotten
/// foreground watch does not pin a terminal indefinitely.
pub(crate) fn default_slack_watch_timeout() -> u64 {
    3600
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
    // Deliberate deviation from upstream (which hardcodes
    // /usr/share/qam-metadata/refhosts.yml): mtui-rs defaults to a per-user path.
    expanduser(&PathBuf::from("~/.local/share/refdb/refhosts.yml"))
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
pub(crate) fn default_pool_reap_stale() -> bool {
    true
}
pub(crate) fn default_pool_stale_age() -> u64 {
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
pub(crate) fn default_mcp_max_output_bytes() -> usize {
    100_000
}
pub(crate) fn default_mcp_max_input_bytes() -> usize {
    10_000_000
}
pub(crate) fn default_mcp_max_request_bytes() -> usize {
    10_000_000
}
pub(crate) fn default_mcp_session_cap() -> usize {
    32
}
pub(crate) fn default_mcp_session_idle_timeout() -> u64 {
    // 4 hours. Deliberately higher than upstream's 1800s: this value also pins
    // the rmcp streamable-HTTP session keep-alive (`serve_http`), whose own
    // default (300s) tore down idle http sessions mid-conversation.
    14_400
}
pub(crate) fn default_mcp_sweep_parallel() -> usize {
    4
}
pub(crate) fn default_mcp_max_active_jobs() -> usize {
    16
}
pub(crate) fn default_mcp_max_completed_jobs() -> usize {
    128
}
pub(crate) fn default_mcp_profile() -> String {
    "full".to_owned()
}
pub(crate) fn default_obs_api_url() -> String {
    "https://api.suse.de".to_owned()
}
pub(crate) fn default_obs_request_timeout() -> u64 {
    180
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
    pub ssl_verify: Option<SslVerify>,
}

/// `[connection]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ConnectionSection {
    pub connection_timeout: Option<u64>,
    pub reboot_timeout: Option<u64>,
    pub reboot_retries: Option<u64>,
    pub max_parallel: Option<u64>,
    pub max_oqa_parallel: Option<u64>,
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

/// `[qem_dashboard]` table — the QEM Dashboard API base URL.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct QemDashboardSection {
    pub api: Option<String>,
}

/// `[teregen]` table — the TeReGen report/queue API base URL.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TeregenSection {
    pub api: Option<String>,
}

/// `[openqa]` table — openQA instance URLs and the install distri.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct OpenqaSection {
    pub openqa: Option<String>,
    pub baremetal: Option<String>,
    pub distri: Option<String>,
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
    pub url: Option<String>,
}

/// `[slack]` table — the Slack review-request integration.
///
/// Off by default, and gated twice over: `enabled` must be `true`, and
/// `token`/`channel` must both be set. An unconfigured mtui therefore never
/// reaches Slack, and `request_review` refuses with the reason rather than
/// failing somewhere inside the API.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SlackSection {
    pub enabled: Option<bool>,
    pub token: Option<String>,
    pub channel: Option<String>,
    pub api_url: Option<String>,
    pub poll_interval: Option<u64>,
    pub watch_timeout: Option<u64>,
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
    pub pool_reap_stale: Option<bool>,
    pub pool_stale_age: Option<u64>,
    pub pi_autolock: Option<bool>,
    pub wait: Option<u64>,
    pub wait_poll: Option<u64>,
}

/// `[mcp]` table — `mtui-mcp` server behaviour.
///
/// Mirrors upstream `mtui/support/config.py`'s `mcp_*` options (which live under
/// the `[mcp]` INI section). `session_cap` / `session_idle_timeout` configure the
/// http transport's per-client session budget (enforcement is a follow-up —
/// mtui-rs-odq8). `profile` / `tools_allow` / `tools_deny` select the exposed
/// tool surface (see `mtui_mcp::profiles`).
///
/// Note: upstream names the profile key `tool_profile`; here it is `profile`
/// under the already tool-scoped `[mcp]` table (an intentional TOML-idiomatic
/// deviation). The list keys keep their upstream names, but are native TOML
/// arrays of strings rather than upstream's comma-separated INI strings.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct McpSection {
    pub max_output_bytes: Option<usize>,
    pub max_input_bytes: Option<usize>,
    pub max_request_bytes: Option<usize>,
    pub max_active_jobs: Option<usize>,
    pub max_completed_jobs: Option<usize>,
    pub session_cap: Option<usize>,
    pub session_idle_timeout: Option<u64>,
    pub sweep_parallel: Option<usize>,
    pub profile: Option<String>,
    pub tools_allow: Option<Vec<String>>,
    pub tools_deny: Option<Vec<String>>,
}

/// `[obs]` table — the native OBS/IBS QAM review backend.
///
/// Mirrors upstream `mtui/support/config.py`'s `obs_*` options (INI `[obs]`
/// section), added when upstream cut over to the native OBS API backend and
/// dropped the transitional `backend` selector (mtui-rs is native-only). No OBS
/// credentials live here — the oscrc remains the sole credential source (see
/// `mtui_datasources::obs::oscrc`).
///
/// * `api_url` is the OBS API mtui acts against; it must equal a section header
///   in the user's oscrc. The oscrc is located like `osc` itself (`$OSC_CONFIG`
///   → `$XDG_CONFIG_HOME/osc/oscrc` → `~/.oscrc`), so there is no mtui-side path
///   option — set `$OSC_CONFIG` to point at a non-default oscrc.
/// * `request_timeout` is a **coarse** wall-clock budget checked *between* a
///   native operation's HTTP calls (each call is itself bounded by the shared
///   HTTP timeout) — it is not a mid-call hard kill.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ObsSection {
    pub api_url: Option<String>,
    pub request_timeout: Option<u64>,
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
    pub qem_dashboard: QemDashboardSection,
    pub teregen: TeregenSection,
    pub openqa: OpenqaSection,
    pub svn: SvnSection,
    pub target: TargetSection,
    pub gitea: GiteaSection,
    pub slack: SlackSection,
    pub lock: LockSection,
    pub mcp: McpSection,
    pub obs: ObsSection,
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
        take!(mtui, ssl_verify);
        take!(connection, connection_timeout);
        take!(connection, reboot_timeout);
        take!(connection, reboot_retries);
        take!(connection, max_parallel);
        take!(connection, max_oqa_parallel);
        take!(connection, ssh_strict_host_key_checking);
        take!(refhosts, resolvers);
        take!(refhosts, https_uri);
        take!(refhosts, https_expiration);
        take!(refhosts, path);
        take!(url, bugzilla);
        take!(url, testreports);
        take!(url, fancy_reports);
        take!(qem_dashboard, api);
        take!(teregen, api);
        take!(openqa, openqa);
        take!(openqa, baremetal);
        take!(openqa, distri);
        take!(svn, path);
        take!(target, tempdir);
        take!(gitea, token);
        take!(gitea, url);
        take!(slack, enabled);
        take!(slack, token);
        take!(slack, channel);
        take!(slack, api_url);
        take!(slack, poll_interval);
        take!(slack, watch_timeout);
        take!(lock, reap_stale);
        take!(lock, stale_age);
        take!(lock, pool_reap_stale);
        take!(lock, pool_stale_age);
        take!(lock, pi_autolock);
        take!(lock, wait);
        take!(lock, wait_poll);
        take!(mcp, max_output_bytes);
        take!(mcp, max_input_bytes);
        take!(mcp, max_request_bytes);
        take!(mcp, max_active_jobs);
        take!(mcp, max_completed_jobs);
        take!(mcp, session_cap);
        take!(mcp, session_idle_timeout);
        take!(mcp, sweep_parallel);
        take!(mcp, profile);
        take!(mcp, tools_allow);
        take!(mcp, tools_deny);
        take!(obs, api_url);
        take!(obs, request_timeout);
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
    /// TLS verification policy for outbound HTTP.
    pub ssl_verify: SslVerify,

    // [connection]
    /// SSH connect + command timeout, in seconds.
    pub connection_timeout: u64,
    /// Backoff base (seconds) for post-reboot reconnect retries. Sleeps grow
    /// as `2*(reboot_timeout + 5*count)` after the first probe. Upstream
    /// `reconnect(timeout=...)`.
    pub reboot_timeout: u64,
    /// Number of post-reboot reconnect attempts beyond the first probe.
    /// Upstream `reconnect(retry=...)`.
    pub reboot_retries: u64,
    /// Maximum number of hosts to fan out to concurrently (SSH command,
    /// SFTP, lock-probe, and connect batches). Caps peak sockets/tasks/RSS
    /// and remote load on large fleets; serial-host semantics are unaffected.
    /// A non-positive value falls back to the default (upstream is unbounded).
    pub max_parallel: u64,
    /// Maximum number of independent openQA/QAM HTTP requests to run
    /// concurrently in the oqa-search overview (per-version, group×version, and
    /// per-log fan-out). Kept lower than [`max_parallel`] to stay polite toward
    /// the shared openQA/QAM hosts. A non-positive value falls back to the
    /// default.
    pub max_oqa_parallel: u64,
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

    // [qem_dashboard]
    /// QEM Dashboard API base URL (upstream `qem_dashboard_api`).
    pub qem_dashboard_api: String,

    // [teregen]
    /// TeReGen report/queue API base URL (upstream `teregen_api`).
    pub teregen_api: String,

    // [openqa]
    /// openQA instance URL (upstream `openqa_instance`).
    pub openqa_instance: String,
    /// Baremetal openQA instance URL (upstream `openqa_instance_baremetal`).
    pub openqa_instance_baremetal: String,
    /// openQA install `distri` parameter (upstream `openqa_install_distri`).
    pub openqa_install_distri: String,

    // [svn]
    /// SVN base path for test-report checkout.
    pub svn_path: String,

    // [gitea]
    /// API token for the Gitea PR review workflow. Empty by default; the Gitea
    /// connector refuses to build without it.
    pub gitea_token: String,
    /// Trusted Gitea origin the [`gitea_token`](Self::gitea_token) may be sent
    /// to. The Gitea connector attaches the token only to requests whose origin
    /// (scheme/host/port) matches this; metadata-supplied PR URLs pointing
    /// anywhere else are refused. Defaults to `https://src.suse.de`.
    pub gitea_url: String,

    // [slack]
    /// Whether the Slack review-request integration is available at all.
    /// `false` by default — the feature is opt-in, so an unconfigured mtui
    /// never posts anywhere; `request_review` refuses and says why.
    pub slack_enabled: bool,
    /// Bot token for the Slack Web API (scopes: `chat:write`, `reactions:read`,
    /// `channels:history`). Empty by default; `request_review` refuses without
    /// it, so the integration stays inert until deliberately configured.
    pub slack_token: String,
    /// Channel the review request is posted to (an ID such as `C01234567` or a
    /// `#name`). Empty by default; `request_review` refuses without it.
    pub slack_channel: String,
    /// Trusted Slack API origin the [`slack_token`](Self::slack_token) may be
    /// sent to. Defaults to `https://slack.com/api`.
    pub slack_api_url: String,
    /// Seconds between reaction polls while watching a review request.
    pub slack_poll_interval: u64,
    /// Seconds a `request_review` watch runs before giving up.
    pub slack_watch_timeout: u64,

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
    /// On a pool claim attempt, force-remove a pre-existing pool-claim lock older
    /// than [`pool_stale_age`](Self::pool_stale_age) seconds regardless of owner.
    /// The pool-claim analogue of [`lock_reap_stale`](Self::lock_reap_stale) — the
    /// only automatic recovery for a pool claim orphaned by an uncatchable exit
    /// (SIGKILL / panic / power loss), which the RRID-based pool lock otherwise
    /// leaves held until a manual `unlock -f -p`.
    pub pool_reap_stale: bool,
    /// Age (seconds) beyond which a pool-claim lock is considered stale and
    /// reapable. A value of `0` disables pool-claim reaping.
    pub pool_stale_age: u64,
    /// When testing a Product Increment (PI), auto-lock all reference hosts on
    /// `assign` and unlock them at end of testing.
    pub lock_pi_autolock: bool,
    /// Host-arbitration pool-claim queueing budget, in seconds. `0` (the
    /// default) fails fast on a busy host.
    pub lock_wait: u64,
    /// Poll interval (seconds) while waiting for a busy pool lock to free.
    pub lock_wait_poll: u64,

    // [mcp]
    /// Upper bound (bytes) on a single `mtui-mcp` tool result. Output beyond
    /// this is truncated at the tail with a notice so one large command result
    /// (e.g. a fan-out `run`) cannot dwarf the client's context. `0` disables
    /// the cap. Upstream default is 100_000.
    pub mcp_max_output_bytes: usize,
    /// Upper bound (bytes) on how much of an on-disk checkout file a
    /// `testreport_read` MCP tool call will read before stopping. Distinct from
    /// `mcp_max_output_bytes` (the wire-result cap): this bounds the *source*
    /// read so a huge or slow file cannot exhaust memory, while still letting a
    /// caller page through a large log via `offset`/`limit`. Reads past this are
    /// truncated with a notice (never refused). `0` disables the cap. No upstream
    /// equivalent — this is a hardening addition. Default is 10_000_000.
    pub mcp_max_input_bytes: usize,
    /// Upper bound (bytes) on an inbound HTTP request body the `--transport http`
    /// MCP server will buffer before rejecting it (413). Distinct from
    /// `mcp_max_input_bytes` (a testreport *source* read budget): this guards the
    /// http transport itself, so an unauthenticated pre-session request cannot be
    /// buffered until memory exhaustion. `0` disables mtui's limit entirely
    /// (`DefaultBodyLimit::disable()`), removing even axum's implicit 2 MB floor.
    /// No upstream equivalent — this is a hardening addition. Default is
    /// 10_000_000.
    pub mcp_max_request_bytes: usize,
    /// Ceiling on concurrent (running) background jobs a single `mtui-mcp`
    /// session may hold (DoS guard). A `start`/`start_jobs` request that would
    /// exceed this is rejected *before* any worker is spawned. `0` disables the
    /// cap. Default is 16. No upstream equivalent — this is a hardening addition.
    pub mcp_max_active_jobs: usize,
    /// Ceiling on retained *terminal* (done/failed/cancelled) background-job
    /// records per session; the oldest-finished records beyond this are evicted
    /// FIFO so a long-lived session does not accumulate job history unbounded.
    /// Running jobs are never evicted. `0` disables the cap. Default is 128. No
    /// upstream equivalent — this is a hardening addition.
    pub mcp_max_completed_jobs: usize,
    /// Ceiling on concurrent per-client sessions under `--transport http` (DoS
    /// guard). Upstream default is 32. Enforcement is a follow-up
    /// (mtui-rs-odq8); this value is parsed and surfaced now.
    pub mcp_session_cap: usize,
    /// Seconds of inactivity after which an idle http session is swept. `0`
    /// disables the sweeper. Also pins the rmcp streamable-HTTP session
    /// keep-alive (`serve_http`) so the transport does not tear a session down
    /// before this sweeper would. Default is 14400 (4h) — deliberately higher
    /// than upstream's 1800 (which would let rmcp's own 300s keep-alive drop
    /// idle sessions mid-conversation).
    pub mcp_session_idle_timeout: u64,
    /// Max stale sessions the idle sweeper tears down concurrently in one sweep
    /// cycle (each teardown is bounded by the per-session disconnect timeout).
    /// Bounding the fan-out keeps a mass eviction from a host-teardown thundering
    /// herd while making sweep latency ~independent of stale-session count.
    /// Default is 4. No upstream equivalent — this is a hardening addition
    /// (mtui-rs-0mop.10).
    pub mcp_sweep_parallel: usize,
    /// Tool-surface profile the `mtui-mcp` server exposes: `"full"` (default,
    /// every synthesised tool) or `"core"` (the curated everyday subset — see
    /// `mtui_mcp::profiles`). An unknown name falls back to `full` with a
    /// warning. Upstream key is `[mcp] tool_profile`; here it is `[mcp] profile`.
    pub mcp_profile: String,
    /// Extra tool names to keep on top of the profile (only those actually
    /// registered are added). Layered before `mcp_tools_deny`.
    pub mcp_tools_allow: Vec<String>,
    /// Tool names to remove regardless of profile/allow (deny wins last).
    pub mcp_tools_deny: Vec<String>,

    // [obs]
    /// The OBS/IBS API URL the native QAM review backend acts against. Must
    /// equal a section header in the user's oscrc. Upstream default is
    /// `https://api.suse.de`. The oscrc is located like `osc` itself
    /// (`$OSC_CONFIG` → `$XDG_CONFIG_HOME/osc/oscrc` → `~/.oscrc`); there is no
    /// mtui-side path option (set `$OSC_CONFIG` to override).
    pub obs_api_url: String,
    /// Coarse wall-clock budget (seconds) for a whole native OBS operation,
    /// checked *between* its HTTP calls. Upstream default is 180.
    pub obs_request_timeout: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            template_dir: default_template_dir(),
            local_tempdir: default_local_tempdir(),
            session_user: default_session_user(),
            install_logs: default_install_logs(),
            chdir_to_template_dir: false,
            ssl_verify: SslVerify::Enabled,
            connection_timeout: default_connection_timeout(),
            reboot_timeout: default_reboot_timeout(),
            reboot_retries: default_reboot_retries(),
            max_parallel: default_max_parallel(),
            max_oqa_parallel: default_max_oqa_parallel(),
            ssh_strict_host_key_checking: default_ssh_strict_host_key_checking(),
            refhosts_resolvers: default_refhosts_resolvers(),
            refhosts_https_uri: default_refhosts_https_uri(),
            refhosts_https_expiration: default_refhosts_https_expiration(),
            refhosts_path: default_refhosts_path(),
            bugzilla_url: default_bugzilla_url(),
            reports_url: default_reports_url(),
            fancy_reports_url: default_fancy_reports_url(),
            qem_dashboard_api: default_qem_dashboard_api(),
            teregen_api: default_teregen_api(),
            openqa_instance: default_openqa_instance(),
            openqa_instance_baremetal: default_openqa_instance_baremetal(),
            openqa_install_distri: default_openqa_install_distri(),
            svn_path: default_svn_path(),
            gitea_token: String::new(),
            gitea_url: default_gitea_url(),
            slack_enabled: default_slack_enabled(),
            slack_token: String::new(),
            slack_channel: String::new(),
            slack_api_url: default_slack_api_url(),
            slack_poll_interval: default_slack_poll_interval(),
            slack_watch_timeout: default_slack_watch_timeout(),
            target_tempdir: default_target_tempdir(),
            lock_reap_stale: default_lock_reap_stale(),
            lock_stale_age: default_lock_stale_age(),
            pool_reap_stale: default_pool_reap_stale(),
            pool_stale_age: default_pool_stale_age(),
            lock_pi_autolock: default_lock_pi_autolock(),
            lock_wait: default_lock_wait(),
            lock_wait_poll: default_lock_wait_poll(),
            mcp_max_output_bytes: default_mcp_max_output_bytes(),
            mcp_max_input_bytes: default_mcp_max_input_bytes(),
            mcp_max_request_bytes: default_mcp_max_request_bytes(),
            mcp_max_active_jobs: default_mcp_max_active_jobs(),
            mcp_max_completed_jobs: default_mcp_max_completed_jobs(),
            mcp_session_cap: default_mcp_session_cap(),
            mcp_session_idle_timeout: default_mcp_session_idle_timeout(),
            mcp_sweep_parallel: default_mcp_sweep_parallel(),
            mcp_profile: default_mcp_profile(),
            mcp_tools_allow: Vec::new(),
            mcp_tools_deny: Vec::new(),
            obs_api_url: default_obs_api_url(),
            obs_request_timeout: default_obs_request_timeout(),
        }
    }
}

impl Config {
    /// Flatten a merged [`RawConfig`] into a fully-typed `Config`, applying
    /// defaults for absent options and expanding `~` in path options.
    pub(crate) fn from_raw(raw: RawConfig) -> Self {
        let d = Config::default();

        // Per-field parse-time validation with fallback to the default (logged
        // at ERROR), mirroring upstream `config.py`. A present-but-invalid value
        // never invalidates the rest of the file.
        macro_rules! validated_url {
            ($opt:expr, $field:literal, $default:expr) => {
                match $opt {
                    Some(v) if validate_base_url(&v) => v,
                    Some(v) => {
                        tracing::error!(
                            option = $field,
                            value = %v,
                            "invalid endpoint URL (need http(s):// with a host and, if given, a numeric port); using default"
                        );
                        $default
                    }
                    None => $default,
                }
            };
        }
        macro_rules! validated_positive {
            ($opt:expr, $field:literal, $default:expr) => {
                match $opt {
                    Some(0) => {
                        tracing::error!(
                            option = $field,
                            value = 0,
                            "expected a positive integer greater than 0; using default"
                        );
                        $default
                    }
                    Some(v) => v,
                    None => $default,
                }
            };
        }

        Self {
            template_dir: raw
                .mtui
                .template_dir
                .map_or(d.template_dir, |p| expanduser(&p)),
            local_tempdir: raw.mtui.tempdir.map_or(d.local_tempdir, |p| expanduser(&p)),
            session_user: raw.mtui.user.unwrap_or(d.session_user),
            install_logs: match raw.mtui.install_logs {
                Some(p) if is_relative_dir_name(&p.to_string_lossy()) => p,
                Some(p) => {
                    tracing::error!(
                        option = "install_logs",
                        value = %p.display(),
                        "expected a single relative directory name without a path separator (e.g. install_logs); using default"
                    );
                    d.install_logs
                }
                None => d.install_logs,
            },
            chdir_to_template_dir: raw
                .mtui
                .chdir_to_template_dir
                .unwrap_or(d.chdir_to_template_dir),
            ssl_verify: raw.mtui.ssl_verify.unwrap_or(d.ssl_verify),
            connection_timeout: validated_positive!(
                raw.connection.connection_timeout,
                "connection_timeout",
                d.connection_timeout
            ),
            reboot_timeout: validated_positive!(
                raw.connection.reboot_timeout,
                "reboot_timeout",
                d.reboot_timeout
            ),
            reboot_retries: validated_positive!(
                raw.connection.reboot_retries,
                "reboot_retries",
                d.reboot_retries
            ),
            max_parallel: validated_positive!(
                raw.connection.max_parallel,
                "max_parallel",
                d.max_parallel
            ),
            max_oqa_parallel: validated_positive!(
                raw.connection.max_oqa_parallel,
                "max_oqa_parallel",
                d.max_oqa_parallel
            ),
            ssh_strict_host_key_checking: raw
                .connection
                .ssh_strict_host_key_checking
                .unwrap_or(d.ssh_strict_host_key_checking),
            refhosts_resolvers: raw.refhosts.resolvers.unwrap_or(d.refhosts_resolvers),
            refhosts_https_uri: validated_url!(
                raw.refhosts.https_uri,
                "refhosts_https_uri",
                d.refhosts_https_uri
            ),
            refhosts_https_expiration: validated_positive!(
                raw.refhosts.https_expiration,
                "refhosts_https_expiration",
                d.refhosts_https_expiration
            ),
            refhosts_path: raw
                .refhosts
                .path
                .map_or(d.refhosts_path, |p| expanduser(&p)),
            bugzilla_url: raw.url.bugzilla.unwrap_or(d.bugzilla_url),
            reports_url: raw.url.testreports.unwrap_or(d.reports_url),
            fancy_reports_url: raw.url.fancy_reports.unwrap_or(d.fancy_reports_url),
            qem_dashboard_api: validated_url!(
                raw.qem_dashboard.api,
                "qem_dashboard_api",
                d.qem_dashboard_api
            ),
            teregen_api: validated_url!(raw.teregen.api, "teregen_api", d.teregen_api),
            openqa_instance: validated_url!(
                raw.openqa.openqa,
                "openqa_instance",
                d.openqa_instance
            ),
            openqa_instance_baremetal: validated_url!(
                raw.openqa.baremetal,
                "openqa_instance_baremetal",
                d.openqa_instance_baremetal
            ),
            openqa_install_distri: raw.openqa.distri.unwrap_or(d.openqa_install_distri),
            svn_path: raw.svn.path.unwrap_or(d.svn_path),
            gitea_token: raw.gitea.token.unwrap_or(d.gitea_token),
            gitea_url: validated_url!(raw.gitea.url, "gitea_url", d.gitea_url),

            slack_enabled: raw.slack.enabled.unwrap_or(d.slack_enabled),
            slack_token: raw.slack.token.unwrap_or(d.slack_token),
            slack_channel: raw.slack.channel.unwrap_or(d.slack_channel),
            slack_api_url: validated_url!(raw.slack.api_url, "slack_api_url", d.slack_api_url),
            slack_poll_interval: validated_positive!(
                raw.slack.poll_interval,
                "slack_poll_interval",
                d.slack_poll_interval
            ),
            slack_watch_timeout: validated_positive!(
                raw.slack.watch_timeout,
                "slack_watch_timeout",
                d.slack_watch_timeout
            ),
            target_tempdir: raw
                .target
                .tempdir
                .map_or(d.target_tempdir, |p| expanduser(&p)),
            lock_reap_stale: raw.lock.reap_stale.unwrap_or(d.lock_reap_stale),
            lock_stale_age: raw.lock.stale_age.unwrap_or(d.lock_stale_age),
            pool_reap_stale: raw.lock.pool_reap_stale.unwrap_or(d.pool_reap_stale),
            pool_stale_age: raw.lock.pool_stale_age.unwrap_or(d.pool_stale_age),
            lock_pi_autolock: raw.lock.pi_autolock.unwrap_or(d.lock_pi_autolock),
            lock_wait: raw.lock.wait.unwrap_or(d.lock_wait),
            lock_wait_poll: validated_positive!(
                raw.lock.wait_poll,
                "lock_wait_poll",
                d.lock_wait_poll
            ),
            mcp_max_output_bytes: raw.mcp.max_output_bytes.unwrap_or(d.mcp_max_output_bytes),
            mcp_max_input_bytes: raw.mcp.max_input_bytes.unwrap_or(d.mcp_max_input_bytes),
            mcp_max_request_bytes: raw.mcp.max_request_bytes.unwrap_or(d.mcp_max_request_bytes),
            mcp_max_active_jobs: raw.mcp.max_active_jobs.unwrap_or(d.mcp_max_active_jobs),
            mcp_max_completed_jobs: raw
                .mcp
                .max_completed_jobs
                .unwrap_or(d.mcp_max_completed_jobs),
            mcp_session_cap: validated_positive!(
                raw.mcp.session_cap,
                "mcp_session_cap",
                d.mcp_session_cap
            ),
            mcp_session_idle_timeout: validated_positive!(
                raw.mcp.session_idle_timeout,
                "mcp_session_idle_timeout",
                d.mcp_session_idle_timeout
            ),
            mcp_sweep_parallel: validated_positive!(
                raw.mcp.sweep_parallel,
                "mcp_sweep_parallel",
                d.mcp_sweep_parallel
            ),
            mcp_profile: raw.mcp.profile.unwrap_or(d.mcp_profile),
            mcp_tools_allow: raw.mcp.tools_allow.unwrap_or(d.mcp_tools_allow),
            mcp_tools_deny: raw.mcp.tools_deny.unwrap_or(d.mcp_tools_deny),
            obs_api_url: validated_url!(raw.obs.api_url, "obs_api_url", d.obs_api_url),
            obs_request_timeout: validated_positive!(
                raw.obs.request_timeout,
                "obs_request_timeout",
                d.obs_request_timeout
            ),
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
        assert_eq!(c.reboot_timeout, 10);
        assert_eq!(c.reboot_retries, 10);
        assert_eq!(c.max_parallel, 50);
        assert_eq!(c.max_oqa_parallel, 8);
        assert_eq!(c.refhosts_https_expiration, 3600 * 12);
        assert!(!c.chdir_to_template_dir);
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
        if let Some(base) = directories::BaseDirs::new() {
            assert_eq!(
                c.refhosts_path,
                base.home_dir().join(".local/share/refdb/refhosts.yml")
            );
        }
        assert_eq!(c.install_logs, PathBuf::from("install_logs"));
        assert_eq!(c.target_tempdir, PathBuf::from("/tmp"));
        // [lock] defaults mirror upstream config.py exactly.
        assert!(c.lock_reap_stale);
        assert_eq!(c.lock_stale_age, 86400);
        // Pool-claim reaping defaults match the operation lock's.
        assert!(c.pool_reap_stale);
        assert_eq!(c.pool_stale_age, 86400);
        assert!(c.lock_pi_autolock);
        assert_eq!(c.lock_wait, 0);
        assert_eq!(c.lock_wait_poll, 15);
        // openQA / QEM Dashboard / TeReGen defaults mirror upstream config.py.
        assert_eq!(c.qem_dashboard_api, "http://dashboard.qam.suse.de/api");
        assert_eq!(c.teregen_api, "https://qam.suse.de/api/v1");
        assert_eq!(c.openqa_instance, "https://openqa.suse.de");
        assert_eq!(c.openqa_instance_baremetal, "http://openqa.qam.suse.cz");
        assert_eq!(c.openqa_install_distri, "sle");
        // [obs] defaults mirror upstream config.py exactly (native-only backend).
        assert_eq!(c.obs_api_url, "https://api.suse.de");
        assert_eq!(c.obs_request_timeout, 180);
    }

    #[test]
    fn obs_section_parses_and_overrides() {
        let raw: RawConfig = toml::from_str(
            r#"
            [obs]
            api_url = "https://api.opensuse.org"
            request_timeout = 90
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        assert_eq!(c.obs_api_url, "https://api.opensuse.org");
        assert_eq!(c.obs_request_timeout, 90);
    }

    #[test]
    fn obs_section_partial_keeps_defaults() {
        // A partial [obs] table leaves absent keys at their upstream defaults.
        let raw: RawConfig = toml::from_str("[obs]\nrequest_timeout = 90\n").unwrap();
        let c = Config::from_raw(raw);
        assert_eq!(c.obs_request_timeout, 90);
        assert_eq!(c.obs_api_url, "https://api.suse.de");
    }

    #[test]
    fn obs_invalid_url_and_zero_timeout_fall_back() {
        let raw: RawConfig = toml::from_str(
            r#"
            [obs]
            api_url = "not-a-url"
            request_timeout = 0
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        let d = Config::default();
        assert_eq!(c.obs_api_url, d.obs_api_url);
        assert_eq!(c.obs_request_timeout, d.obs_request_timeout);
    }

    #[test]
    fn openqa_teregen_dashboard_sections_override_defaults() {
        let raw: RawConfig = toml::from_str(
            r#"
            [qem_dashboard]
            api = "http://dash.local/api"
            [teregen]
            api = "http://tere.local/api/v1"
            [openqa]
            openqa = "http://oqa.local"
            baremetal = "http://oqa-bm.local"
            distri = "sle-micro"
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        assert_eq!(c.qem_dashboard_api, "http://dash.local/api");
        assert_eq!(c.teregen_api, "http://tere.local/api/v1");
        assert_eq!(c.openqa_instance, "http://oqa.local");
        assert_eq!(c.openqa_instance_baremetal, "http://oqa-bm.local");
        assert_eq!(c.openqa_install_distri, "sle-micro");
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
    fn gitea_url_defaults_to_src_suse_de_and_parses() {
        // Default is the internal SUSE Gitea (the token's only trusted origin).
        assert_eq!(Config::default().gitea_url, "https://src.suse.de");
        // A [gitea] table overrides it (validated as an http(s) URL).
        let raw: RawConfig =
            toml::from_str("[gitea]\nurl = \"https://src.opensuse.org\"\n").unwrap();
        assert_eq!(Config::from_raw(raw).gitea_url, "https://src.opensuse.org");
        // A malformed value falls back to the default (lenient loading).
        let bad: RawConfig = toml::from_str("[gitea]\nurl = \"not a url\"\n").unwrap();
        assert_eq!(Config::from_raw(bad).gitea_url, "https://src.suse.de");
    }

    #[test]
    fn slack_is_off_until_explicitly_enabled() {
        let d = Config::default();
        // Opt-in by default: posting into a chat workspace is an outward-facing
        // side effect that must never follow from an unconfigured install.
        assert!(!d.slack_enabled);
        assert_eq!(d.slack_token, "");
        assert_eq!(d.slack_channel, "");
        assert_eq!(d.slack_api_url, "https://slack.com/api");
        assert_eq!(d.slack_poll_interval, 120);
        assert_eq!(d.slack_watch_timeout, 3600);
    }

    #[test]
    fn slack_section_parses_and_overrides() {
        let raw: RawConfig = toml::from_str(
            "[slack]\nenabled = true\ntoken = \"xoxb-abc\"\nchannel = \"#qam\"\n\
             api_url = \"https://slack.example.com/api\"\npoll_interval = 45\n\
             watch_timeout = 600\n",
        )
        .unwrap();
        let c = Config::from_raw(raw);
        assert!(c.slack_enabled);
        assert_eq!(c.slack_token, "xoxb-abc");
        assert_eq!(c.slack_channel, "#qam");
        assert_eq!(c.slack_api_url, "https://slack.example.com/api");
        assert_eq!(c.slack_poll_interval, 45);
        assert_eq!(c.slack_watch_timeout, 600);
    }

    #[test]
    fn slack_partial_keeps_defaults() {
        // Enabling the integration must not disturb the other options, so a
        // site can switch it on without restating the whole section.
        let raw: RawConfig = toml::from_str("[slack]\nenabled = true\n").unwrap();
        let c = Config::from_raw(raw);
        assert!(c.slack_enabled);
        assert_eq!(c.slack_api_url, "https://slack.com/api");
        assert_eq!(c.slack_poll_interval, 120);
    }

    #[test]
    fn slack_invalid_values_fall_back_to_defaults() {
        // Lenient loading: a bad value is logged and defaulted, never fatal.
        let bad_url: RawConfig = toml::from_str("[slack]\napi_url = \"not a url\"\n").unwrap();
        assert_eq!(
            Config::from_raw(bad_url).slack_api_url,
            "https://slack.com/api"
        );

        // A zero poll interval would busy-loop against a rate-limited API.
        let zero_poll: RawConfig = toml::from_str("[slack]\npoll_interval = 0\n").unwrap();
        assert_eq!(Config::from_raw(zero_poll).slack_poll_interval, 120);

        // A zero timeout would mean a watch that ends before it begins.
        let zero_timeout: RawConfig = toml::from_str("[slack]\nwatch_timeout = 0\n").unwrap();
        assert_eq!(Config::from_raw(zero_timeout).slack_watch_timeout, 3600);
    }

    #[test]
    fn slack_options_survive_the_file_merge() {
        // Every field needs its own `take!` line; a forgotten one silently
        // drops that option when a per-user file overrides /etc.
        let mut base: RawConfig = toml::from_str(
            "[slack]\nenabled = true\ntoken = \"etc\"\nchannel = \"#etc\"\n\
             api_url = \"https://etc.example.com\"\npoll_interval = 10\nwatch_timeout = 20\n",
        )
        .unwrap();
        let user: RawConfig = toml::from_str(
            "[slack]\nenabled = false\ntoken = \"user\"\nchannel = \"#user\"\n\
             api_url = \"https://user.example.com\"\npoll_interval = 30\nwatch_timeout = 40\n",
        )
        .unwrap();
        base.merge(user);
        let c = Config::from_raw(base);
        assert!(!c.slack_enabled);
        assert_eq!(c.slack_token, "user");
        assert_eq!(c.slack_channel, "#user");
        assert_eq!(c.slack_api_url, "https://user.example.com");
        assert_eq!(c.slack_poll_interval, 30);
        assert_eq!(c.slack_watch_timeout, 40);
    }

    #[test]
    fn lock_section_parses_and_overrides() {
        let raw: RawConfig = toml::from_str(
            "[lock]\nreap_stale = false\nstale_age = 3600\npool_reap_stale = false\npool_stale_age = 7200\npi_autolock = false\nwait = 30\nwait_poll = 5\n",
        )
        .unwrap();
        let c = Config::from_raw(raw);
        assert!(!c.lock_reap_stale);
        assert_eq!(c.lock_stale_age, 3600);
        assert!(!c.pool_reap_stale);
        assert_eq!(c.pool_stale_age, 7200);
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
        assert!(c.pool_reap_stale);
        assert_eq!(c.pool_stale_age, 86400);
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
        raw.connection.reboot_timeout = Some(20);
        raw.connection.reboot_retries = Some(5);
        raw.connection.max_parallel = Some(8);
        raw.connection.max_oqa_parallel = Some(3);
        raw.mtui.chdir_to_template_dir = Some(true);
        raw.url.bugzilla = Some("https://bugzilla.example.com".to_owned());
        let c = Config::from_raw(raw);
        // Overridden.
        assert_eq!(c.connection_timeout, 450);
        assert_eq!(c.reboot_timeout, 20);
        assert_eq!(c.reboot_retries, 5);
        assert_eq!(c.max_parallel, 8);
        assert_eq!(c.max_oqa_parallel, 3);
        assert!(c.chdir_to_template_dir);
        assert_eq!(c.bugzilla_url, "https://bugzilla.example.com");
        // Untouched falls back to default.
        assert_eq!(c.reports_url, "https://qam.suse.de/testreports");
    }

    #[test]
    fn merge_later_wins_on_shared_keys() {
        let mut base = RawConfig::default();
        base.connection.connection_timeout = Some(100);
        base.connection.reboot_timeout = Some(10);
        base.url.bugzilla = Some("base".to_owned());
        let mut over = RawConfig::default();
        over.connection.connection_timeout = Some(999);
        over.connection.reboot_timeout = Some(30);
        base.merge(over);
        assert_eq!(base.connection.connection_timeout, Some(999));
        assert_eq!(base.connection.reboot_timeout, Some(30));
        // A key not set in `over` is preserved from base.
        assert_eq!(base.url.bugzilla.as_deref(), Some("base"));
    }

    #[test]
    fn validate_base_url_accepts_usable_forms() {
        for ok in [
            "https://openqa.suse.de",
            "http://dashboard.qam.suse.de/api",
            "https://qam.suse.de/refhosts/refhosts.yml",
            "https://openqa.suse.de:8080",
            "http://user:pass@host.example:443/path",
            "http://[::1]:9000/x",
            "https://[2001:db8::1]",
            " https://openqa.suse.de ", // trimmed
        ] {
            assert!(validate_base_url(ok), "expected valid: {ok:?}");
        }
    }

    #[test]
    fn validate_base_url_rejects_bad_forms() {
        for bad in [
            "https://openqa.suse.de:44e3", // non-numeric port (the upstream typo)
            "ftp://host.example",          // wrong scheme
            "openqa.suse.de",              // no scheme
            "https://",                    // no host
            "https:///path",               // empty authority
            "http://:8080",                // empty host with port
            "http://[::1",                 // unclosed IPv6 bracket
            "https://host:99999",          // port out of u16 range
            "",
        ] {
            assert!(!validate_base_url(bad), "expected invalid: {bad:?}");
        }
    }

    #[test]
    fn is_relative_dir_name_matches_upstream() {
        assert!(is_relative_dir_name("install_logs"));
        assert!(is_relative_dir_name("logs"));
        for bad in ["", "a/b", "/abs", ".", "..", "sub/dir/"] {
            assert!(!is_relative_dir_name(bad), "expected invalid: {bad:?}");
        }
    }

    #[test]
    fn invalid_url_falls_back_to_default_and_keeps_rest_of_file() {
        let raw: RawConfig = toml::from_str(
            r#"
            [openqa]
            openqa = "https://openqa.suse.de:44e3"
            distri = "sle-micro"
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        // Bad URL falls back to the default...
        assert_eq!(c.openqa_instance, "https://openqa.suse.de");
        // ...while a sibling valid option in the same file still applies.
        assert_eq!(c.openqa_install_distri, "sle-micro");
    }

    #[test]
    fn all_url_options_validate() {
        let raw: RawConfig = toml::from_str(
            r#"
            [refhosts]
            https_uri = "nope"
            [qem_dashboard]
            api = "ftp://x"
            [teregen]
            api = "://x"
            [openqa]
            openqa = "http://:1"
            baremetal = "http://host:x"
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        let d = Config::default();
        assert_eq!(c.refhosts_https_uri, d.refhosts_https_uri);
        assert_eq!(c.qem_dashboard_api, d.qem_dashboard_api);
        assert_eq!(c.teregen_api, d.teregen_api);
        assert_eq!(c.openqa_instance, d.openqa_instance);
        assert_eq!(c.openqa_instance_baremetal, d.openqa_instance_baremetal);
    }

    #[test]
    fn zero_positive_int_falls_back_to_default() {
        let raw: RawConfig = toml::from_str(
            r#"
            [connection]
            connection_timeout = 0
            reboot_timeout = 0
            reboot_retries = 0
            max_parallel = 0
            max_oqa_parallel = 0
            [refhosts]
            https_expiration = 0
            [lock]
            wait_poll = 0
            [mcp]
            session_cap = 0
            session_idle_timeout = 0
            sweep_parallel = 0
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        let d = Config::default();
        assert_eq!(c.connection_timeout, d.connection_timeout);
        assert_eq!(c.reboot_timeout, d.reboot_timeout);
        assert_eq!(c.reboot_retries, d.reboot_retries);
        assert_eq!(c.max_parallel, d.max_parallel);
        assert_eq!(c.max_oqa_parallel, d.max_oqa_parallel);
        assert_eq!(c.refhosts_https_expiration, d.refhosts_https_expiration);
        assert_eq!(c.lock_wait_poll, d.lock_wait_poll);
        assert_eq!(c.mcp_session_cap, d.mcp_session_cap);
        assert_eq!(c.mcp_session_idle_timeout, d.mcp_session_idle_timeout);
        assert_eq!(c.mcp_sweep_parallel, d.mcp_sweep_parallel);
    }

    #[test]
    fn zero_legal_int_options_accept_zero() {
        // These upstream options use plain `int`; 0 is meaningful and must NOT
        // be rejected: lock_stale_age (disables reaping), lock_wait (fail fast),
        // mcp_max_output_bytes (disables the cap).
        let raw: RawConfig = toml::from_str(
            r#"
            [lock]
            stale_age = 0
            wait = 0
            [mcp]
            max_output_bytes = 0
            "#,
        )
        .unwrap();
        let c = Config::from_raw(raw);
        assert_eq!(c.lock_stale_age, 0);
        assert_eq!(c.lock_wait, 0);
        assert_eq!(c.mcp_max_output_bytes, 0);
    }

    #[test]
    fn bad_install_logs_falls_back_and_valid_name_accepted() {
        let bad: RawConfig = toml::from_str("[mtui]\ninstall_logs = \"a/b\"\n").unwrap();
        assert_eq!(
            Config::from_raw(bad).install_logs,
            Config::default().install_logs
        );

        let ok: RawConfig = toml::from_str("[mtui]\ninstall_logs = \"my_logs\"\n").unwrap();
        assert_eq!(Config::from_raw(ok).install_logs, PathBuf::from("my_logs"));
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
