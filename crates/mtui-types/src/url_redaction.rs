//! Masking of URL userinfo for logs and diagnostics.
//!
//! Repository and datasource URLs may legitimately embed credentials
//! (`scheme://user:pass@host/…`). The real URL is still what gets executed or
//! requested; only the copy that reaches a log line, error message, prompt or
//! stored command log goes through these helpers.
//!
//! This is not secret detection: only the userinfo of a `scheme://` authority
//! (and, for [`sanitize_url`], a bare `…@…` string) is masked. A password
//! printed without a URL, or carried in a path or query, is not recognised.

/// Return `url` with any userinfo (`user[:password]@`) replaced by `***`, for
/// safe logging/diagnostics.
///
/// Configured datasource URLs may legitimately embed credentials
/// (`scheme://user:pass@host/…`); those must never reach the log stream. Only
/// the authority's userinfo is touched, so scheme, host, port, path and query
/// keep a log line useful. On any unexpected shape it fails closed, stripping an
/// entire `…@` authority prefix rather than risk echoing a credential.
#[must_use]
pub fn sanitize_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // No scheme separator: fail closed on any `@`.
        return match rest_after_at(url) {
            Some(after) => format!("***@{after}"),
            None => url.to_owned(),
        };
    };
    // The authority ends at the first path/query/fragment delimiter.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_userinfo, hostport)) => format!("{scheme}://***@{hostport}{tail}"),
        None => url.to_owned(),
    }
}

/// If `s` contains an `@`, return the slice after the last one; else `None`.
fn rest_after_at(s: &str) -> Option<&str> {
    s.rsplit_once('@').map(|(_, after)| after)
}

/// Return `text` with the userinfo of every embedded `scheme://` URL masked, as
/// [`sanitize_url`] does for a single URL.
///
/// For free-form text — a shell command line, command output, an error message
/// — that may carry any number of URLs. Everything outside the masked
/// userinfo, quoting and punctuation included, is returned byte for byte.
#[must_use]
pub fn sanitize_urls_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        let scheme_start = rest[..sep]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_scheme_char(c))
            .last()
            .map_or(sep, |(i, _)| i);
        let authority_start = sep + "://".len();
        let end = rest[authority_start..]
            .find(ends_authority)
            .map_or(rest.len(), |i| authority_start + i);
        out.push_str(&rest[..scheme_start]);
        out.push_str(&sanitize_url(&rest[scheme_start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// Whether `c` cannot be part of a URL authority embedded in text: the URL's
/// own path/query/fragment delimiters, whitespace and control characters, and
/// the quoting and shell metacharacters a command line wraps a URL in.
fn ends_authority(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '/' | '?'
                | '#'
                | '\''
                | '"'
                | '`'
                | '\\'
                | '<'
                | '>'
                | ';'
                | '|'
                | '$'
                | '('
                | ')'
                | '{'
                | '}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_url_redacts_password() {
        let out = sanitize_url("https://alice:s3cret@host.example/api/v1?q=1");
        assert_eq!(out, "https://***@host.example/api/v1?q=1");
        assert!(!out.contains("s3cret"));
        assert!(!out.contains("alice"));
    }

    #[test]
    fn sanitize_url_redacts_username_only() {
        let out = sanitize_url("https://token@host.example:443/path");
        assert_eq!(out, "https://***@host.example:443/path");
        assert!(!out.contains("token"));
    }

    #[test]
    fn sanitize_url_without_userinfo_is_unchanged() {
        let url = "https://host.example:8080/a/b?c=d#e";
        assert_eq!(sanitize_url(url), url);
    }

    #[test]
    fn sanitize_url_at_in_path_is_not_userinfo() {
        let url = "https://host.example/users/me@example.com";
        assert_eq!(sanitize_url(url), url);
    }

    #[test]
    fn sanitize_url_unparseable_with_at_fails_closed() {
        let out = sanitize_url("alice:s3cret@host.example/x");
        assert_eq!(out, "***@host.example/x");
        assert!(!out.contains("s3cret"));
    }

    #[test]
    fn text_masks_each_shape_of_userinfo() {
        for (input, want) in [
            ("https://u:p@h:443/r?q=1", "https://***@h:443/r?q=1"),
            ("ftp://token@h/r", "ftp://***@h/r"),
            ("https://u:p@[2001:db8::1]/r", "https://***@[2001:db8::1]/r"),
            ("https://u%40x:p%3Ay@h/r", "https://***@h/r"),
            ("nfs://u:p@server/export", "nfs://***@server/export"),
            ("https://u:p@h", "https://***@h"),
            ("https://h/a@b", "https://h/a@b"),
            ("https://h:8080/a/b?c=d#e", "https://h:8080/a/b?c=d#e"),
            ("no urls here, only u@host", "no urls here, only u@host"),
            ("", ""),
        ] {
            assert_eq!(sanitize_urls_in_text(input), want, "input {input:?}");
        }
    }

    #[test]
    fn text_masks_every_url_in_a_command_line() {
        let got = sanitize_urls_in_text("zypper ar 'https://u:p@h/r' x; echo https://a:b@j/q");
        assert_eq!(got, "zypper ar 'https://***@h/r' x; echo https://***@j/q");
    }

    #[test]
    fn text_masks_a_url_nested_in_another_urls_query() {
        let got = sanitize_urls_in_text("https://u:p@h/r?mirror=https://a:b@j/q");
        assert_eq!(got, "https://***@h/r?mirror=https://***@j/q");
    }

    #[test]
    fn text_keeps_surrounding_quotes_and_punctuation() {
        for (input, want) in [
            (
                "Repository 'https://u:p@h/r' is invalid.",
                "Repository 'https://***@h/r' is invalid.",
            ),
            ("(https://u:p@h/r)", "(https://***@h/r)"),
            ("\"https://u:p@h\"", "\"https://***@h\""),
            ("failed https://u:p@h/r.", "failed https://***@h/r."),
            ("url=https://u:p@h/r,x", "url=https://***@h/r,x"),
            ("a\nhttps://u:p@h/r\nb", "a\nhttps://***@h/r\nb"),
            ("ünï https://u:p@h/ü", "ünï https://***@h/ü"),
        ] {
            assert_eq!(sanitize_urls_in_text(input), want, "input {input:?}");
        }
    }

    #[test]
    fn text_masking_is_idempotent() {
        let once = sanitize_urls_in_text("echo https://u:p@h/r https://a@j");
        assert_eq!(sanitize_urls_in_text(&once), once);
    }

    #[test]
    fn text_without_a_scheme_separator_is_untouched() {
        let t = "plain u:p@h/r text";
        assert_eq!(sanitize_urls_in_text(t), t);
    }
}
