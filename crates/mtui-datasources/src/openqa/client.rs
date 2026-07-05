//! The openQA REST API client, reproducing the auth contract of the
//! third-party python `openqa_client` package on top of this crate's shared
//! [`HttpClient`].
//!
//! Upstream mtui wraps `openqa_client.client.OpenQA_Client`, which:
//!
//! 1. reads `key`/`secret` for the server from INI `client.conf` files under
//!    `/etc/openqa` and `~/.config/openqa` (later files override earlier);
//! 2. sends `Accept: json` and, when a key is configured, `X-API-Key`;
//! 3. signs every request with an HMAC-SHA1 over `"{path}{microtime}"` where
//!    `path` is the request path+query with openQA's quirk substitutions
//!    (`%20` → `+`, `~` → `%7E`), emitting `X-API-Microtime` and `X-API-Hash`.
//!
//! This module rebuilds that contract with `reqwest` (via [`HttpClient`]) so
//! mtui-rs needs no python runtime and no third-party client. GET requests do
//! not strictly require signing (openQA allows unauthenticated GETs), but the
//! signature is always attached when a secret is configured, matching upstream.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::error::OpenQAError;
use crate::http::HttpClient;

type HmacSha1 = Hmac<Sha1>;

/// The API credentials resolved for one openQA server.
///
/// Mirrors the `key`/`secret` pair `OpenQA_Client` reads from `client.conf`.
/// Both may be empty, in which case only unauthenticated GET requests are
/// possible (upstream logs this at debug and continues).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiCredentials {
    /// The API key, sent as the `X-API-Key` header.
    pub key: String,
    /// The API secret, used to HMAC-sign each request. Empty means "no signing".
    pub secret: String,
}

impl ApiCredentials {
    /// Whether a secret is present (and thus requests can be signed).
    #[must_use]
    pub fn can_sign(&self) -> bool {
        !self.secret.is_empty()
    }

    /// Resolve credentials for `server` from parsed `client.conf` sections.
    ///
    /// Mirrors upstream's lookup order: try the bare `server` section first,
    /// then the full `baseurl` section, else empty credentials.
    #[must_use]
    pub fn resolve(sections: &ClientConf, server: &str, baseurl: &str) -> Self {
        sections
            .credentials(server)
            .or_else(|| sections.credentials(baseurl))
            .unwrap_or_default()
    }
}

/// Parsed `client.conf` INI: a map of section name → key/value pairs.
///
/// Only the minimal INI shape openQA uses is supported: `[section]` headers and
/// `key = value` lines. Comments (`#`/`;`) and blank lines are ignored. This
/// avoids pulling in a full INI dependency for a two-key file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientConf {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl ClientConf {
    /// The standard `client.conf` search paths, lowest precedence first.
    ///
    /// Matches upstream `("/etc/openqa", "~/.config/openqa")`; later files
    /// override earlier ones for the same section/key.
    #[must_use]
    pub fn default_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("/etc/openqa/client.conf")];
        if let Some(home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf()) {
            paths.push(home.join(".config/openqa/client.conf"));
        }
        paths
    }

    /// Read and merge the [`default_paths`](Self::default_paths).
    ///
    /// Missing files are skipped; an unreadable or malformed file is logged at
    /// `warn` and skipped, so a bad config never hard-fails a lookup (matching
    /// upstream's lenient posture).
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Self::default_paths())
    }

    /// Read and merge the given paths (lowest precedence first).
    #[must_use]
    pub fn load_from(paths: &[PathBuf]) -> Self {
        let mut conf = Self::default();
        for path in paths {
            match std::fs::read_to_string(path) {
                Ok(contents) => conf.merge(&Self::parse(&contents)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!("could not read openQA client.conf {}: {e}", path.display());
                }
            }
        }
        conf
    }

    /// Parse INI text into sections. Never fails: unparseable lines are skipped.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current: Option<String> = None;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let name = name.trim().to_string();
                sections.entry(name.clone()).or_default();
                current = Some(name);
            } else if let Some((k, v)) = line.split_once('=')
                && let Some(section) = &current
            {
                sections
                    .entry(section.clone())
                    .or_default()
                    .insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        Self { sections }
    }

    /// Merge `other` into `self`, with `other`'s values winning on conflict.
    fn merge(&mut self, other: &Self) {
        for (section, kvs) in &other.sections {
            let dst = self.sections.entry(section.clone()).or_default();
            for (k, v) in kvs {
                dst.insert(k.clone(), v.clone());
            }
        }
    }

    /// Look up `key`/`secret` for a section, if both are present.
    #[must_use]
    fn credentials(&self, section: &str) -> Option<ApiCredentials> {
        let kvs = self.sections.get(section)?;
        let key = kvs.get("key")?;
        let secret = kvs.get("secret")?;
        Some(ApiCredentials {
            key: key.clone(),
            secret: secret.clone(),
        })
    }
}

/// Apply openQA's path-encoding quirks before signing.
///
/// Mirrors upstream `path.replace("%20", "+").replace("~", "%7E")`. The input
/// is the already-percent-encoded request path+query.
#[must_use]
fn encode_path_for_signing(path: &str) -> String {
    path.replace("%20", "+").replace('~', "%7E")
}

/// Compute the `X-API-Hash` for a request path and microtime.
///
/// `apisecret` keys an HMAC-SHA1 over the concatenation `"{path}{microtime}"`,
/// hex-encoded — the exact scheme in `openqa_client._add_auth_headers`.
#[must_use]
pub fn compute_api_hash(secret: &str, path: &str, microtime: &str) -> String {
    let mut mac =
        HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(encode_path_for_signing(path).as_bytes());
    mac.update(microtime.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// The current time as a floating-point Unix timestamp string.
///
/// Mirrors python `str(time.time())`, used as `X-API-Microtime`. Returns
/// [`OpenQAError::Clock`] if the system clock predates the Unix epoch.
fn current_microtime() -> Result<String, OpenQAError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| OpenQAError::Clock)?;
    Ok(now.as_secs_f64().to_string())
}

/// A minimal openQA API client: base URL + credentials over an [`HttpClient`].
#[derive(Debug, Clone)]
pub struct OpenQAClient {
    http: HttpClient,
    /// The scheme+host base URL, e.g. `https://openqa.example.com`.
    base_url: String,
    credentials: ApiCredentials,
}

impl OpenQAClient {
    /// Build a client for `base_url` (scheme+host) with the given
    /// [`HttpClient`] and credentials.
    #[must_use]
    pub fn new(http: HttpClient, base_url: impl Into<String>, credentials: ApiCredentials) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            credentials,
        }
    }

    /// The base URL this client targets.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The resolved credentials.
    #[must_use]
    pub fn credentials(&self) -> &ApiCredentials {
        &self.credentials
    }

    /// Build a signed GET request to `/api/v1/{path}` with `params`.
    ///
    /// Reproduces the upstream request shape: `Accept: json`, `X-API-Key` when a
    /// key is set, and the `X-API-Microtime`/`X-API-Hash` pair when a secret is
    /// set. The signature covers the request path+query (`/api/v1/{path}?...`).
    ///
    /// # Errors
    ///
    /// Returns [`OpenQAError::Clock`] if the system clock predates the Unix
    /// epoch.
    pub fn build_get(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<reqwest::RequestBuilder, OpenQAError> {
        let api_path = format!("/api/v1/{path}");

        // Encode the query ourselves (reqwest's `.query()` needs the `query`
        // feature, which pulls in default features we disable). Build the same
        // string we sign so the signed path exactly equals the sent path.
        let query = build_query_string(params);
        let path_url = if query.is_empty() {
            api_path.clone()
        } else {
            format!("{api_path}?{query}")
        };
        let url = format!("{}{path_url}", self.base_url);

        let mut builder = self.http.inner().get(&url).header("Accept", "json");

        if !self.credentials.key.is_empty() {
            builder = builder.header("X-API-Key", self.credentials.key.clone());
        }
        if self.credentials.can_sign() {
            let microtime = current_microtime()?;
            let hash = compute_api_hash(&self.credentials.secret, &path_url, &microtime);
            builder = builder
                .header("X-API-Microtime", microtime)
                .header("X-API-Hash", hash);
        }
        Ok(builder)
    }
}

/// Build the percent-encoded `key=value&...` query string for signing.
///
/// Uses `application/x-www-form-urlencoded` encoding to match reqwest's
/// `.query()` wire form, so the signed path equals the sent path.
fn build_query_string(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                urlencoding::encode(k),
                urlencoding::encode(v).replace("%20", "+")
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_sections_and_keys() {
        let conf = ClientConf::parse(
            "# a comment\n\
             [openqa.example.com]\n\
             key = ABCDEF\n\
             secret = 123456\n\
             \n\
             ; another\n\
             [other]\n\
             key = ZZZ\n\
             secret = YYY\n",
        );
        let creds = conf.credentials("openqa.example.com").unwrap();
        assert_eq!(creds.key, "ABCDEF");
        assert_eq!(creds.secret, "123456");
        let other = conf.credentials("other").unwrap();
        assert_eq!(other.key, "ZZZ");
    }

    #[test]
    fn parse_skips_malformed_lines_without_failing() {
        let conf = ClientConf::parse("garbage-without-section\n[s]\nkey=k\nsecret=s\nno-equals\n");
        let creds = conf.credentials("s").unwrap();
        assert_eq!(creds.key, "k");
        assert_eq!(creds.secret, "s");
    }

    #[test]
    fn credentials_needs_both_key_and_secret() {
        let conf = ClientConf::parse("[s]\nkey=only\n");
        assert!(conf.credentials("s").is_none());
    }

    #[test]
    fn merge_later_paths_override_earlier() {
        let mut base = ClientConf::parse("[s]\nkey=old\nsecret=old\n");
        let over = ClientConf::parse("[s]\nkey=new\nsecret=new\n");
        base.merge(&over);
        let creds = base.credentials("s").unwrap();
        assert_eq!(creds.key, "new");
        assert_eq!(creds.secret, "new");
    }

    #[test]
    fn resolve_prefers_server_then_baseurl_then_empty() {
        let conf = ClientConf::parse(
            "[openqa.example.com]\nkey=SRV\nsecret=srvsec\n\
             [https://openqa.example.com]\nkey=URL\nsecret=urlsec\n",
        );
        // server section wins
        let c = ApiCredentials::resolve(&conf, "openqa.example.com", "https://openqa.example.com");
        assert_eq!(c.key, "SRV");
        // fall back to baseurl section
        let c2 = ApiCredentials::resolve(&conf, "missing", "https://openqa.example.com");
        assert_eq!(c2.key, "URL");
        // empty when neither present
        let c3 = ApiCredentials::resolve(&conf, "missing", "https://also.missing");
        assert_eq!(c3, ApiCredentials::default());
    }

    #[test]
    fn encode_path_applies_openqa_quirks() {
        assert_eq!(
            encode_path_for_signing("/api/v1/jobs?a=x%20y~z"),
            "/api/v1/jobs?a=x+y%7Ez"
        );
    }

    #[test]
    fn compute_api_hash_is_a_stable_known_vector() {
        // Fixed inputs -> a deterministic HMAC-SHA1 hex digest, cross-checked
        // against python: hmac.new(b"secret", b"/api/v1/jobs1000.0",
        // hashlib.sha1).hexdigest(). This locks the signing scheme (key,
        // message ordering, hex encoding) so an accidental change is caught.
        let hash = compute_api_hash("secret", "/api/v1/jobs", "1000.0");
        assert_eq!(hash, "f50340ce52ef5590362f4adf2eb03cf04aef7862");
        assert_eq!(hash.len(), 40); // SHA-1 hex is 40 chars
    }

    #[test]
    fn compute_api_hash_applies_path_quirks_before_signing() {
        // "%20" and "~" in the path are rewritten before hashing, so a path
        // carrying them must hash the same as its rewritten form.
        let raw = compute_api_hash("s", "/api/v1/x?a=b%20c~d", "1.0");
        let pre = compute_api_hash("s", "/api/v1/x?a=b+c%7Ed", "1.0");
        assert_eq!(raw, pre);
    }

    #[test]
    fn can_sign_reflects_secret_presence() {
        assert!(!ApiCredentials::default().can_sign());
        assert!(
            ApiCredentials {
                key: "k".into(),
                secret: "s".into()
            }
            .can_sign()
        );
    }

    #[test]
    fn build_query_string_encodes_params() {
        let q = build_query_string(&[
            ("build", ":smelt:1:bash".to_string()),
            ("latest", "1".to_string()),
        ]);
        assert!(q.contains("build=%3Asmelt%3A1%3Abash"));
        assert!(q.contains("latest=1"));
    }
}
