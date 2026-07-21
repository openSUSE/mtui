//! Shell-safe argument quoting for remote command construction.
//!
//! Values parsed from testreport metadata (package specifiers, repository URLs
//! and aliases) are placed on command lines run **as root** on reference hosts.
//! Joining them raw is a command-injection vector. [`quote_args`] quotes each
//! token so a value carrying shell metacharacters, whitespace, or a leading dash
//! reaches the remote shell as a single literal argument.
//!
//! This is the exec-boundary complement to typed ingestion validation
//! (e.g. [`PackageSpec`](crate::package_spec::PackageSpec)); the two together
//! give defense-in-depth. The helper is deliberately in `mtui-types` so both
//! `mtui-hosts` and `mtui-testreport` (and the repository-URL sinks) can reuse
//! it without a crate cycle.
//!
//! Quoting uses [`shlex`], the same crate the REPL uses for `run` argument
//! joining, so the whole codebase quotes consistently.

/// Quotes each argument and joins them with single spaces.
///
/// Each element is escaped so it survives the remote POSIX shell as exactly one
/// word. A `NUL` byte (which no shell can carry) causes `shlex` to refuse the
/// join; such a token is dropped, which is safe: a `NUL`-bearing package name is
/// never valid and must not reach the command line.
pub fn quote_args<S: AsRef<str>>(args: &[S]) -> String {
    shlex::try_join(args.iter().map(AsRef::as_ref)).unwrap_or_else(|_| {
        // `try_join` only fails on an interior NUL. Fall back to quoting the
        // tokens that *can* be quoted, dropping any NUL-bearing token rather
        // than emitting it unquoted.
        args.iter()
            .filter_map(|a| shlex::try_quote(a.as_ref()).ok().map(|c| c.into_owned()))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_args_join_with_spaces() {
        assert_eq!(quote_args(&["pkg-a", "pkg-b"]), "pkg-a pkg-b");
    }

    #[test]
    fn single_safe_arg_is_unquoted() {
        assert_eq!(quote_args(&["bash"]), "bash");
    }

    #[test]
    fn metacharacters_are_quoted_into_one_word() {
        let out = quote_args(&["foo; rm -rf /"]);
        // The malicious value must be wrapped so the shell never sees a bare `;`
        // command separator; shlex single-quotes it. Re-splitting must yield
        // exactly one literal token.
        assert!(out.starts_with('\''), "not quoted: {out:?}");
        assert_eq!(
            shlex::split(&out).unwrap(),
            vec!["foo; rm -rf /".to_owned()],
            "should re-split to a single literal token: {out:?}"
        );
    }

    #[test]
    fn substitution_is_quoted() {
        let out = quote_args(&["foo$(id)"]);
        assert!(out != "foo$(id)", "substitution left unquoted: {out:?}");
    }

    #[test]
    fn option_like_arg_survives_as_single_token() {
        // shlex leaves a leading-dash token bare (a dash is not shell-special);
        // option-safety is enforced upstream at ingestion by `PackageSpec`. The
        // quoter's contract is only that the value re-splits to one token.
        let out = quote_args(&["--force"]);
        assert_eq!(shlex::split(&out).unwrap(), vec!["--force".to_owned()]);
    }

    #[test]
    fn nul_bearing_token_is_dropped_not_leaked() {
        let out = quote_args(&["ok", "ba\0d"]);
        assert!(!out.contains('\0'), "NUL leaked: {out:?}");
        assert!(out.contains("ok"));
    }

    #[test]
    fn empty_list_is_empty_string() {
        let empty: &[&str] = &[];
        assert_eq!(quote_args(empty), "");
    }
}
