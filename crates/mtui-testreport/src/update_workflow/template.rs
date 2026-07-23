//! A minimal `string.Template`-parity substituter.
//!
//! ## Reference
//!
//! The upstream update-workflow action tables (`mtui/update_workflow/actions/`)
//! store their commands as [Python `string.Template`][pytemplate] objects and
//! call `.substitute(...)` on them. The command strings use every `Template`
//! feature: bare `$name` (`$packages`, `$repa`), braced `${name}`, and the
//! `$$` escape (the update/downgrade shell loops rely on `$$r` / `$$p` to emit
//! a literal `$r` / `$p` for the remote shell). A naive `str::replace` would
//! corrupt those, so this module reproduces `string.Template.substitute`
//! semantics faithfully:
//!
//! * `$$`            → a single literal `$`,
//! * `$name`         → the value of `name` (identifier = `[A-Za-z_][A-Za-z0-9_]*`),
//! * `${name}`       → the value of `name`,
//! * a missing key   → [`TemplateError::MissingKey`] (upstream `substitute`
//!   raises `KeyError`; it is **not** `safe_substitute`),
//! * a malformed `$` (e.g. `$` at end of string, `$1`, `${}`, unterminated
//!   `${`) → [`TemplateError::Invalid`] (upstream raises `ValueError`).
//!
//! Both [`substitute`] and [`safe_substitute`] are ported: the `install`,
//! `uninstall`, and `prepare` call sites use `.substitute` (raise on a missing
//! key / malformed `$`), while `update` and `downgrade` use `.safe_substitute`
//! (leave a missing key or malformed `$` untouched). The distinction matters:
//! the `update`/`downgrade` templates embed shell/awk `$`-tokens — `$2` awk
//! fields and `$$r`-style escapes — that only `safe_substitute` tolerates
//! alongside the real `$repa` / `$package` placeholders (upstream
//! `hostgroup.py` calls `.safe_substitute(repa=..., packages=...)` on them).
//!
//! [pytemplate]: https://docs.python.org/3/library/string.html#string.Template

use std::collections::HashMap;

use thiserror::Error;

/// Errors raised by [`substitute`], mirroring `string.Template.substitute`.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TemplateError {
    /// A `$name` / `${name}` referenced a key not present in the mapping.
    ///
    /// Mirrors the `KeyError` raised by `string.Template.substitute`.
    #[error("template placeholder ${{{0}}} has no value")]
    MissingKey(String),

    /// The template contained a malformed `$` construct (a lone `$`, a
    /// non-identifier after `$`, or an unterminated / empty `${...}`).
    ///
    /// Mirrors the `ValueError` raised by `string.Template.substitute`.
    #[error("invalid placeholder in template at byte offset {0}")]
    Invalid(usize),
}

/// Whether `c` may start a `string.Template` identifier (`[A-Za-z_]`).
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Whether `c` may continue a `string.Template` identifier (`[A-Za-z0-9_]`).
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Substitutes `$name` / `${name}` placeholders in `template` from `mapping`.
///
/// Behaves like Python's `string.Template(template).substitute(mapping)`:
/// `$$` is an escaped literal `$`, identifiers match `[A-Za-z_][A-Za-z0-9_]*`,
/// a missing key is an error ([`TemplateError::MissingKey`]), and a malformed
/// `$` construct is an error ([`TemplateError::Invalid`]).
///
/// # Errors
///
/// Returns [`TemplateError::MissingKey`] when a referenced placeholder is
/// absent from `mapping`, or [`TemplateError::Invalid`] when the template has a
/// malformed `$` construct.
pub(crate) fn substitute(
    template: &str,
    mapping: &HashMap<&str, &str>,
) -> Result<String, TemplateError> {
    render(template, mapping, false)
}

/// Like [`substitute`] but never fails: a missing key or malformed `$` is left
/// in the output verbatim.
///
/// Mirrors Python's `string.Template(template).safe_substitute(mapping)`, which
/// the `update` and `downgrade` command templates rely on so their embedded
/// shell/awk `$`-tokens (`$2` awk fields, an unresolved `$package` at the list
/// stage) survive substitution unharmed.
#[must_use]
pub(crate) fn safe_substitute(template: &str, mapping: &HashMap<&str, &str>) -> String {
    // In lenient mode `render` never returns `Err`.
    render(template, mapping, true).unwrap_or_else(|_| template.to_owned())
}

/// The shared substitution core.
///
/// When `safe` is `false`, a missing key or malformed placeholder returns
/// `Err`; when `safe` is `true`, both are emitted verbatim and substitution
/// continues.
fn render(
    template: &str,
    mapping: &HashMap<&str, &str>,
    safe: bool,
) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let start = i;
        // Fast-path over the run of non-`$` characters.
        if bytes[i] != b'$' {
            // Copy the whole UTF-8 char (templates are ASCII shell, but stay
            // correct for any UTF-8 content).
            let ch = template[i..].chars().next().expect("valid utf-8 boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        // At a `$`. Look at the next byte.
        let next = bytes.get(i + 1).copied();
        match next {
            // `$$` → literal `$`.
            Some(b'$') => {
                out.push('$');
                i += 2;
            }
            // `${name}` → braced identifier.
            Some(b'{') => {
                let name_start = i + 2;
                let brace_end = template[name_start..].find('}');
                let valid = brace_end.is_some_and(|rel_end| {
                    let name = &template[name_start..name_start + rel_end];
                    !name.is_empty() && is_valid_ident(name)
                });
                if !valid {
                    if safe {
                        out.push('$');
                        i += 1;
                        continue;
                    }
                    return Err(TemplateError::Invalid(start));
                }
                let rel_end = brace_end.expect("checked valid");
                let name = &template[name_start..name_start + rel_end];
                match mapping.get(name) {
                    Some(value) => out.push_str(value),
                    None if safe => out.push_str(&template[start..name_start + rel_end + 1]),
                    None => return Err(TemplateError::MissingKey(name.to_owned())),
                }
                i = name_start + rel_end + 1;
            }
            // `$name` → bare identifier.
            Some(c) if is_ident_start(c as char) => {
                let name_start = i + 1;
                let mut end = name_start;
                while end < bytes.len() && is_ident_continue(bytes[end] as char) {
                    end += 1;
                }
                let name = &template[name_start..end];
                match mapping.get(name) {
                    Some(value) => out.push_str(value),
                    None if safe => out.push_str(&template[start..end]),
                    None => return Err(TemplateError::MissingKey(name.to_owned())),
                }
                i = end;
            }
            // Lone `$` (end of string) or `$` before a non-identifier char:
            // upstream `substitute` treats this as an invalid placeholder;
            // `safe_substitute` emits it verbatim.
            _ => {
                if safe {
                    out.push('$');
                    i += 1;
                } else {
                    return Err(TemplateError::Invalid(start));
                }
            }
        }
    }

    Ok(out)
}

/// Whether `name` is a valid `string.Template` identifier.
fn is_valid_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if is_ident_start(c) => chars.all(is_ident_continue),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&'static str, &'static str)]) -> HashMap<&'static str, &'static str> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn bare_identifier_is_substituted() {
        let m = map(&[("packages", "pkg-a pkg-b")]);
        assert_eq!(
            substitute("zypper -n in -y -l $packages", &m).unwrap(),
            "zypper -n in -y -l pkg-a pkg-b"
        );
    }

    #[test]
    fn braced_identifier_is_substituted() {
        let m = map(&[("name", "foo")]);
        assert_eq!(substitute("pre-${name}-post", &m).unwrap(), "pre-foo-post");
    }

    #[test]
    fn double_dollar_escapes_to_literal_dollar() {
        // The update/downgrade loops rely on `$$r` -> `$r` reaching the shell.
        let m = map(&[]);
        assert_eq!(
            substitute("while read r; do zypper -n rr $$r; done", &m).unwrap(),
            "while read r; do zypper -n rr $r; done"
        );
    }

    #[test]
    fn identifier_stops_at_non_identifier_char() {
        // `$repa\>` — the backslash terminates the identifier `repa`.
        let m = map(&[("repa", "SUSE-Patch-1")]);
        assert_eq!(substitute(r"/$repa\>/", &m).unwrap(), r"/SUSE-Patch-1\>/");
    }

    #[test]
    fn awk_field_and_escaped_dollar_coexist() {
        // From zypper_update: `awk ... $$2` must become `$2` (awk field ref),
        // while `$repa` expands.
        let m = map(&[("repa", "R")]);
        assert_eq!(
            substitute(r#"awk -F "|" '/$repa\>/ {{ print $$2; }}'"#, &m).unwrap(),
            r#"awk -F "|" '/R\>/ {{ print $2; }}'"#
        );
    }

    #[test]
    fn missing_key_is_an_error() {
        let m = map(&[]);
        assert_eq!(
            substitute("$missing", &m),
            Err(TemplateError::MissingKey("missing".to_owned()))
        );
        assert_eq!(
            substitute("${missing}", &m),
            Err(TemplateError::MissingKey("missing".to_owned()))
        );
    }

    #[test]
    fn lone_trailing_dollar_is_invalid() {
        let m = map(&[]);
        // The `$` is the last byte, at index 9.
        assert_eq!(substitute("cost is 5$", &m), Err(TemplateError::Invalid(9)));
    }

    #[test]
    fn dollar_before_digit_is_invalid() {
        // `$1` is not a valid Template placeholder (identifiers can't start
        // with a digit) — upstream raises ValueError.
        let m = map(&[]);
        assert_eq!(substitute("$1", &m), Err(TemplateError::Invalid(0)));
    }

    #[test]
    fn empty_or_unterminated_brace_is_invalid() {
        let m = map(&[]);
        assert_eq!(substitute("${}", &m), Err(TemplateError::Invalid(0)));
        assert_eq!(substitute("${abc", &m), Err(TemplateError::Invalid(0)));
        assert_eq!(substitute("${1a}", &m), Err(TemplateError::Invalid(0)));
    }

    #[test]
    fn no_placeholders_is_identity() {
        let m = map(&[]);
        assert_eq!(
            substitute("systemctl reboot", &m).unwrap(),
            "systemctl reboot"
        );
    }

    #[test]
    fn multiple_placeholders_and_repeats() {
        let m = map(&[("package", "kernel"), ("version", "1.2")]);
        assert_eq!(
            substitute(
                "rpm -q $package && zypper in --oldpackage -y $package=$version",
                &m
            )
            .unwrap(),
            "rpm -q kernel && zypper in --oldpackage -y kernel=1.2"
        );
    }

    // --- safe_substitute ----------------------------------------------------

    #[test]
    fn safe_substitute_leaves_missing_keys_verbatim() {
        // The updater template mixes real `$repa` with awk's `$2` field ref.
        // safe_substitute must expand repa and keep `$2` untouched.
        let m = map(&[("repa", ":p=1:2")]);
        assert_eq!(
            safe_substitute(r#"awk -F "|" '/$repa\>/ {{ print $2; }}'"#, &m),
            r#"awk -F "|" '/:p=1:2\>/ {{ print $2; }}'"#
        );
    }

    #[test]
    fn safe_substitute_still_expands_present_keys_and_escapes() {
        let m = map(&[("packages", "p1 p2")]);
        assert_eq!(
            safe_substitute("yum -y update $packages $$HOME", &m),
            "yum -y update p1 p2 $HOME"
        );
    }

    #[test]
    fn safe_substitute_keeps_braced_missing_key() {
        let m = map(&[]);
        assert_eq!(safe_substitute("${missing}-x", &m), "${missing}-x");
    }

    #[test]
    fn safe_substitute_keeps_lone_and_invalid_dollar() {
        let m = map(&[]);
        assert_eq!(safe_substitute("cost 5$", &m), "cost 5$");
        assert_eq!(safe_substitute("$1 field", &m), "$1 field");
        assert_eq!(safe_substitute("${} bad", &m), "${} bad");
    }
}
