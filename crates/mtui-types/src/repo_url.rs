//! Validated repository URL.
//!
//! Repository URLs derived from testreport metadata are placed on remote
//! `zypper ar`/`rr` command lines run **as root**, so raw interpolation is a
//! command-injection vector: [`RepoUrl`] validates at ingestion, rejecting
//! unsupported URI schemes and shell-unsafe characters with a typed
//! [`RepoUrlParseError`] before a URL can reach host execution. The
//! exec-boundary sink shell-quotes every argument too (defense-in-depth).
//!
//! ## Accepted schemes
//!
//! The URI schemes libzypp/zypper accept for a repository, per the openSUSE
//! `zypper modifyrepo -m TYPE` documentation: `http`, `https`, `ftp`, `cd`,
//! `dvd`, `dir`, `file`, `cifs`, `smb`, `nfs`, `hd`, `iso`. Any other (or a
//! missing) scheme is rejected.
//!
//! ## Rejected characters
//!
//! Whitespace, control characters, quotes/backslash, and the shell
//! metacharacters that never legitimately appear in a repository URL
//! (`;` backtick `$ | < > ( ) { }`) are rejected, as is a leading `-` (which a
//! command would read as an option). URL-legitimate characters that *are* shell
//! metacharacters (`? & = # ~ [ ] *`) are permitted — a repository URL's query
//! string uses them (e.g. `iso:///?iso=x.iso`, `hd:///?device=/dev/sr0`) — and
//! the exec-boundary shell-quoting neutralises their shell meaning.

use std::fmt;
use std::str::FromStr;

use crate::error::RepoUrlParseError;
use crate::url_redaction::sanitize_url;

/// URI schemes zypper/libzypp accept for a repository (`zypper modifyrepo -m`).
const ALLOWED_SCHEMES: [&str; 12] = [
    "http", "https", "ftp", "cd", "dvd", "dir", "file", "cifs", "smb", "nfs", "hd", "iso",
];

/// Returns `true` if `ch` may appear in a repository URL (see the [module
/// docs](self) for what is rejected and why).
fn is_url_char(ch: char) -> bool {
    if ch.is_whitespace() || ch.is_control() {
        return false;
    }
    !matches!(
        ch,
        ';' | '|' | '$' | '`' | '<' | '>' | '(' | ')' | '{' | '}' | '"' | '\'' | '\\'
    )
}

/// A validated repository URL safe to place on a remote command line (once
/// shell-quoted at the boundary).
///
/// The URL may carry credentials (`https://user:token@host/…`): [`Display`]
/// yields the exact string to execute, while [`Debug`] and the rejection errors
/// mask the userinfo.
///
/// [`Display`]: fmt::Display
/// [`Debug`]: fmt::Debug
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RepoUrl(String);

impl RepoUrl {
    /// Validates and wraps a repository URL.
    ///
    /// # Errors
    /// Returns a [`RepoUrlParseError`] if the URL is empty, begins with `-`, has
    /// no scheme, has a scheme zypper does not accept, or contains a shell-unsafe
    /// or control character.
    pub fn parse(raw: &str) -> Result<Self, RepoUrlParseError> {
        if raw.is_empty() {
            return Err(RepoUrlParseError::Empty);
        }
        if raw.starts_with('-') {
            return Err(RepoUrlParseError::OptionLike {
                url: sanitize_url(raw),
            });
        }
        if let Some(ch) = raw.chars().find(|&c| !is_url_char(c)) {
            return Err(RepoUrlParseError::IllegalChar {
                ch,
                url: sanitize_url(raw),
            });
        }

        let Some((scheme, _rest)) = raw.split_once("://") else {
            return Err(RepoUrlParseError::MissingScheme {
                url: sanitize_url(raw),
            });
        };
        let scheme_lc = scheme.to_ascii_lowercase();
        if !ALLOWED_SCHEMES.contains(&scheme_lc.as_str()) {
            return Err(RepoUrlParseError::UnsupportedScheme { scheme: scheme_lc });
        }

        Ok(Self(raw.to_owned()))
    }
}

impl fmt::Debug for RepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoUrl")
            .field(&sanitize_url(&self.0))
            .finish()
    }
}

impl fmt::Display for RepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RepoUrl {
    type Err = RepoUrlParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_maintenance_urls() {
        for ok in [
            "http://example.com/repo/",
            "https://dist.suse.de/ibs/SUSE:/Maintenance:/1:/2/images/repo/SLES-15-x86_64/",
            "https://example.com/standard",
            "ftp://ftp.suse.com/pub/repo",
            "nfs://server/export/repo",
            "smb://server/share/repo",
            "cifs://server/share/repo",
            "dir:///srv/repo",
            "file:///var/cache/repo",
            "iso:///path?iso=name.iso",
            "hd:///?device=/dev/sr0",
            "dvd:///",
            "cd:///",
        ] {
            assert!(RepoUrl::parse(ok).is_ok(), "expected {ok:?} to parse");
            assert_eq!(RepoUrl::parse(ok).unwrap().to_string(), ok);
        }
    }

    #[test]
    fn scheme_match_is_case_insensitive() {
        assert!(RepoUrl::parse("HTTPS://example.com/repo").is_ok());
    }

    #[test]
    fn rejects_unsupported_scheme() {
        for bad in [
            "javascript://alert",
            "gopher://x/y",
            "ssh://host/repo",
            "data://x",
        ] {
            assert!(
                matches!(
                    RepoUrl::parse(bad),
                    Err(RepoUrlParseError::UnsupportedScheme { .. })
                ),
                "expected {bad:?} to be rejected as unsupported scheme"
            );
        }
    }

    #[test]
    fn rejects_missing_scheme() {
        for bad in ["//host/path", "example.com/repo", "/srv/repo"] {
            assert!(
                matches!(
                    RepoUrl::parse(bad),
                    Err(RepoUrlParseError::MissingScheme { .. })
                ),
                "expected {bad:?} to be rejected as missing scheme"
            );
        }
    }

    #[test]
    fn rejects_shell_metacharacters() {
        for bad in [
            "https://x/repo;reboot",
            "https://x/repo|sh",
            "https://x/$(id)",
            "https://x/`id`",
            "https://x/repo>out",
            "https://x/repo<in",
            "https://x/(a)",
            "https://x/{a}",
            "https://x/a\"b",
            "https://x/a'b",
            "https://x/a\\b",
        ] {
            assert!(
                matches!(
                    RepoUrl::parse(bad),
                    Err(RepoUrlParseError::IllegalChar { .. })
                ),
                "expected {bad:?} to be rejected as illegal char"
            );
        }
    }

    #[test]
    fn allows_url_legitimate_query_characters() {
        for ok in [
            "iso:///?iso=name.iso&arch=x86_64",
            "hd:///?device=/dev/sr0",
            "https://[2001:db8::1]/repo",
            "https://x/repo#frag",
            "https://x/~user/repo",
        ] {
            assert!(RepoUrl::parse(ok).is_ok(), "expected {ok:?} to parse");
        }
    }

    #[test]
    fn rejects_whitespace_and_newlines() {
        for bad in ["https://x/a b", "https://x/a\tb", "https://x/a\nb"] {
            assert!(
                matches!(
                    RepoUrl::parse(bad),
                    Err(RepoUrlParseError::IllegalChar { .. })
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_option_like_and_empty() {
        assert!(matches!(
            RepoUrl::parse("-oProxyCommand"),
            Err(RepoUrlParseError::OptionLike { .. })
        ));
        assert_eq!(RepoUrl::parse(""), Err(RepoUrlParseError::Empty));
    }

    #[test]
    fn rejected_urls_do_not_echo_userinfo() {
        for bad in [
            "https://audituser:CANARY@h/repo;reboot",
            "https://audituser:CANARY@h/a b",
            "-https://audituser:CANARY@h/r",
            "audituser:CANARY@h/r",
        ] {
            let err = RepoUrl::parse(bad).expect_err("rejected");
            for rendered in [err.to_string(), format!("{err:?}")] {
                assert!(
                    !rendered.contains("CANARY") && !rendered.contains("audituser"),
                    "rejection of a credential-bearing URL leaked its userinfo"
                );
            }
        }
    }

    #[test]
    fn rejected_urls_keep_the_host_for_diagnosis() {
        let err = RepoUrl::parse("https://u:p@h.example/repo;reboot").expect_err("rejected");
        assert_eq!(
            err,
            RepoUrlParseError::IllegalChar {
                ch: ';',
                url: "https://***@h.example/repo;reboot".to_owned(),
            }
        );
    }

    #[test]
    fn valid_credential_url_keeps_raw_display_but_masks_debug() {
        let raw = "https://audituser:CANARY@repo.example/r/";
        let url = RepoUrl::parse(raw).expect("valid");
        assert_eq!(url.to_string(), raw);
        let debug = format!("{url:?}");
        assert!(!debug.contains("CANARY") && !debug.contains("audituser"));
        assert_eq!(debug, "RepoUrl(\"https://***@repo.example/r/\")");
    }

    #[test]
    fn display_and_fromstr_round_trip() {
        let url: RepoUrl = "https://example.com/repo/".parse().unwrap();
        assert_eq!(url.to_string(), "https://example.com/repo/");
    }
}
