//! Software package version tracking, ported from `mtui/types/package.py`.
//!
//! A [`Package`] names a single RPM and tracks up to four versions relevant to
//! an update workflow: the version [`before`](Package::before) an update, the
//! version [`after`](Package::after) it, the version [`required`](Package::required)
//! by the update metadata, and the [`current`](Package::current) version actually
//! installed on a target.
//!
//! ## Deviations from upstream
//!
//! Upstream stores each version as `str | RPMVersion | None` and uses property
//! setters that coerce a `str` into an [`RPMVersion`] (leaving anything else as
//! `None`). The Rust port keeps a single typed representation —
//! `Option<`[`RPMVersion`]`>` — and exposes fallible setters that parse a
//! `&str`. Passing an empty string (upstream's `""`, which `RPMVersion`
//! rejected) or `None` clears the field, matching upstream's "non-str ⇒ None"
//! behaviour without silently storing an unparsed string.
//!
//! Upstream hashes and compares a `Package` **by name only**
//! (`__hash__ = hash(self.name)`); the port preserves that exactly so a
//! `Package` can live in a name-keyed set regardless of its version fields.

use crate::rpmver::RPMVersion;

/// A software package and the versions relevant to an update.
///
/// Equality and hashing are **by [`name`](Package::name) only**, mirroring
/// upstream. Two packages with the same name but different versions are
/// considered equal.
#[derive(Debug, Clone)]
pub struct Package {
    /// The package name.
    pub name: String,
    before: Option<RPMVersion>,
    after: Option<RPMVersion>,
    required: Option<RPMVersion>,
    current: Option<RPMVersion>,
}

impl Package {
    /// Creates a new [`Package`] with the given name and no versions set.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            before: None,
            after: None,
            required: None,
            current: None,
        }
    }

    /// The version of the package before an update, if known.
    #[must_use]
    pub fn before(&self) -> Option<&RPMVersion> {
        self.before.as_ref()
    }

    /// The version of the package after an update, if known.
    #[must_use]
    pub fn after(&self) -> Option<&RPMVersion> {
        self.after.as_ref()
    }

    /// The version required by the update metadata, if known.
    #[must_use]
    pub fn required(&self) -> Option<&RPMVersion> {
        self.required.as_ref()
    }

    /// The version currently installed on a target, if known.
    #[must_use]
    pub fn current(&self) -> Option<&RPMVersion> {
        self.current.as_ref()
    }

    /// Sets the [`before`](Package::before) version from an optional string.
    ///
    /// `None` (or an empty string) clears the field, mirroring upstream's
    /// "non-`str` ⇒ `None`" setter semantics.
    ///
    /// # Errors
    /// Returns [`RpmVersionParseError`](crate::error::RpmVersionParseError) only
    /// for a non-empty string that fails to parse. An empty string is treated
    /// as "clear" and never errors.
    pub fn set_before(&mut self, ver: Option<&str>) -> crate::error::Result<()> {
        self.before = parse_opt(ver)?;
        Ok(())
    }

    /// Sets the [`after`](Package::after) version from an optional string.
    ///
    /// # Errors
    /// See [`set_before`](Package::set_before).
    pub fn set_after(&mut self, ver: Option<&str>) -> crate::error::Result<()> {
        self.after = parse_opt(ver)?;
        Ok(())
    }

    /// Sets the [`required`](Package::required) version from an optional string.
    ///
    /// # Errors
    /// See [`set_before`](Package::set_before).
    pub fn set_required(&mut self, ver: Option<&str>) -> crate::error::Result<()> {
        self.required = parse_opt(ver)?;
        Ok(())
    }

    /// Sets the [`before`](Package::before) version directly.
    pub fn set_before_version(&mut self, ver: Option<RPMVersion>) {
        self.before = ver;
    }

    /// Sets the [`after`](Package::after) version directly.
    pub fn set_after_version(&mut self, ver: Option<RPMVersion>) {
        self.after = ver;
    }

    /// Sets the [`current`](Package::current) version directly.
    pub fn set_current_version(&mut self, ver: Option<RPMVersion>) {
        self.current = ver;
    }
}

/// Parses an optional version string, treating `None`/empty as "clear".
fn parse_opt(ver: Option<&str>) -> crate::error::Result<Option<RPMVersion>> {
    match ver {
        Some(v) if !v.is_empty() => Ok(Some(RPMVersion::parse(v)?)),
        _ => Ok(None),
    }
}

impl std::fmt::Display for Package {
    /// Returns the package name, mirroring upstream `__str__`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl PartialEq for Package {
    /// Equal by [`name`](Package::name) only, mirroring upstream `__hash__`.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Package {}

impl std::hash::Hash for Package {
    /// Hashes by [`name`](Package::name) only, mirroring upstream `__hash__`.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn new_has_name_and_no_versions() {
        let p = Package::new("bash");
        assert_eq!(p.name, "bash");
        assert!(p.before().is_none());
        assert!(p.after().is_none());
        assert!(p.required().is_none());
        assert!(p.current().is_none());
    }

    #[test]
    fn set_from_str_parses_versions() {
        let mut p = Package::new("bash");
        p.set_before(Some("1.0-1")).unwrap();
        p.set_after(Some("2.0-1")).unwrap();
        p.set_required(Some("2.0-1")).unwrap();
        assert_eq!(p.before().unwrap(), &RPMVersion::parse("1.0-1").unwrap());
        assert_eq!(p.after().unwrap(), &RPMVersion::parse("2.0-1").unwrap());
        assert_eq!(p.required().unwrap(), &RPMVersion::parse("2.0-1").unwrap());
    }

    #[test]
    fn none_and_empty_clear_the_field() {
        let mut p = Package::new("bash");
        p.set_before(Some("1.0-1")).unwrap();
        p.set_before(None).unwrap();
        assert!(p.before().is_none());

        p.set_after(Some("1.0-1")).unwrap();
        p.set_after(Some("")).unwrap();
        assert!(p.after().is_none());
    }

    #[test]
    fn direct_setters_store_version() {
        let mut p = Package::new("bash");
        p.set_current_version(Some(RPMVersion::parse("3.0-1").unwrap()));
        assert_eq!(p.current().unwrap(), &RPMVersion::parse("3.0-1").unwrap());
        p.set_current_version(None);
        assert!(p.current().is_none());
    }

    #[test]
    fn display_is_name() {
        let p = Package::new("bash");
        assert_eq!(p.to_string(), "bash");
    }

    #[test]
    fn equality_and_hash_by_name_only() {
        let mut a = Package::new("bash");
        a.set_before(Some("1.0-1")).unwrap();
        let mut b = Package::new("bash");
        b.set_before(Some("9.9-9")).unwrap();
        // Same name, different versions ⇒ equal.
        assert_eq!(a, b);

        let mut set = HashSet::new();
        set.insert(a);
        // Inserting the same-named package does not grow the set.
        assert!(!set.insert(b));
        assert_eq!(set.len(), 1);

        assert_ne!(Package::new("bash"), Package::new("zsh"));
    }
}
