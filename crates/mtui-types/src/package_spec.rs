//! Validated package name / `name=version` specifier.
//!
//! Package specifiers parsed from testreport metadata are joined into remote
//! commands executed **as root** on reference hosts. Upstream mtui interpolates
//! them raw (`" ".join(packages)`), which is a command-injection vector. As an
//! improvement over upstream, [`PackageSpec`] validates a specifier at ingestion
//! and rejects anything that is not a plausible RPM package form with a typed
//! [`PackageSpecParseError`], so malformed input never reaches host execution.
//! The exec-boundary sinks additionally shell-quote every argument
//! (defense-in-depth).
//!
//! ## Grammar
//!
//! * **name** — one or more of `[A-Za-z0-9._+-]`, not starting with `-` (a
//!   leading dash would be parsed as a command option).
//! * **spec** — a bare name, or `name=version` (the form the downgrade workflow
//!   builds), where **version** is one or more of `[A-Za-z0-9.:_+~^-]` (`~`/`^`
//!   are legal in RPM versions; `:` appears in epoch/`name=ver` specifiers).
//!
//! Whitespace, quotes, newlines, `$`/backtick substitutions, `;|&<>()` and every
//! other shell metacharacter are rejected.

use std::fmt;
use std::str::FromStr;

use crate::error::PackageSpecParseError;

/// A validated package name or `name=version` specifier safe to place on a
/// remote command line (once shell-quoted at the boundary).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackageSpec(String);

/// Returns `true` if `ch` is allowed in an RPM package name.
fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-')
}

/// Returns `true` if `ch` is allowed in the version half of a `name=version`
/// spec.
fn is_version_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '_' | '+' | '~' | '^' | '-')
}

impl PackageSpec {
    /// Validates and wraps a package name or `name=version` spec.
    ///
    /// # Errors
    /// Returns a [`PackageSpecParseError`] if the input is empty, begins with a
    /// `-`, or contains a character outside the name/version allow-lists.
    pub fn parse(raw: &str) -> Result<Self, PackageSpecParseError> {
        let (name, version) = match raw.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (raw, None),
        };

        if name.is_empty() {
            return Err(PackageSpecParseError::Empty);
        }
        if name.starts_with('-') {
            return Err(PackageSpecParseError::OptionLike {
                name: name.to_owned(),
            });
        }
        if let Some(ch) = name.chars().find(|&c| !is_name_char(c)) {
            return Err(PackageSpecParseError::IllegalChar {
                ch,
                name: name.to_owned(),
            });
        }

        if let Some(version) = version
            && (version.is_empty() || version.chars().any(|c| !is_version_char(c)))
        {
            return Err(PackageSpecParseError::BadVersion {
                version: version.to_owned(),
                spec: raw.to_owned(),
            });
        }

        Ok(Self(raw.to_owned()))
    }
}

impl fmt::Display for PackageSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PackageSpec {
    type Err = PackageSpecParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_rpm_names() {
        for ok in [
            "bash",
            "coreutils",
            "libfoo-1_2",
            "gtk3+",
            "foo.x86_64",
            "kernel-default",
            "python3-setuptools",
            "branding-upstream",
        ] {
            assert!(PackageSpec::parse(ok).is_ok(), "expected {ok:?} to parse");
            assert_eq!(PackageSpec::parse(ok).unwrap().to_string(), ok);
        }
    }

    #[test]
    fn accepts_name_equals_version_specs() {
        for ok in [
            "kernel-default=5.14.21-150500",
            "foo=1.0",
            "bar=1:2.3~beta^1",
            "baz=1.2.3-4.x86_64",
        ] {
            assert!(PackageSpec::parse(ok).is_ok(), "expected {ok:?} to parse");
        }
    }

    #[test]
    fn rejects_shell_metacharacters() {
        for bad in [
            "foo; rm -rf /",
            "foo|bar",
            "foo&bar",
            "foo$(id)",
            "foo`id`",
            "foo>out",
            "foo<in",
            "foo(bar)",
            "foo{bar}",
            "foo*",
            "foo?",
            "foo!bar",
            "foo#bar",
            "foo\\bar",
            "foo/bar",
            "foo@bar",
        ] {
            assert!(
                matches!(
                    PackageSpec::parse(bad),
                    Err(PackageSpecParseError::IllegalChar { .. })
                ),
                "expected {bad:?} to be rejected as illegal char"
            );
        }
    }

    #[test]
    fn rejects_whitespace_quotes_and_newlines() {
        for bad in ["foo bar", "foo\tbar", "foo\nbar", "foo\"bar", "foo'bar"] {
            assert!(
                matches!(
                    PackageSpec::parse(bad),
                    Err(PackageSpecParseError::IllegalChar { .. })
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_option_like_names() {
        for bad in ["-rf", "--force", "-x"] {
            assert!(
                matches!(
                    PackageSpec::parse(bad),
                    Err(PackageSpecParseError::OptionLike { .. })
                ),
                "expected {bad:?} to be rejected as option-like"
            );
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(PackageSpec::parse(""), Err(PackageSpecParseError::Empty));
        assert_eq!(
            PackageSpec::parse("=1.0"),
            Err(PackageSpecParseError::Empty)
        );
    }

    #[test]
    fn rejects_bad_version() {
        assert!(matches!(
            PackageSpec::parse("foo="),
            Err(PackageSpecParseError::BadVersion { .. })
        ));
        assert!(matches!(
            PackageSpec::parse("foo=1.0;rm"),
            Err(PackageSpecParseError::BadVersion { .. })
        ));
        assert!(matches!(
            PackageSpec::parse("foo=1 2"),
            Err(PackageSpecParseError::BadVersion { .. })
        ));
    }

    #[test]
    fn display_and_fromstr_round_trip() {
        let spec: PackageSpec = "kernel-default=5.14.21-150500".parse().unwrap();
        assert_eq!(spec.to_string(), "kernel-default=5.14.21-150500");
    }
}
