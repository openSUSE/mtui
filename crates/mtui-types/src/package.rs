//! Software package version tracking.
//!
//! A [`Package`] names a single RPM and tracks up to four versions relevant to
//! an update workflow: the version [`before`](Package::before) an update, the
//! version [`after`](Package::after) it, the version [`required`](Package::required)
//! by the update metadata, and the [`current`](Package::current) version actually
//! installed on a target.
//!
//! Each version field is stored as a single typed representation, with
//! fallible setters that parse a `&str` rather than silently storing an
//! unparsed string. `before`, `after` and `current` are all [`VersionCheck`]s,
//! not `Option<`[`RPMVersion`]`>`: all three are *measured* on a target, so
//! they must be able to say "checked, not installed" and "never checked" apart
//! (#396, #437 — a host the version query never reached must not read the
//! same as one it confirmed the package absent on). `required` (declared by
//! the update metadata, never measured) stays a plain option.
//!
//! A `Package` hashes and compares **by name only**, so it can live in a
//! name-keyed set regardless of its version fields.

use crate::rpmver::RPMVersion;

/// A software package and the versions relevant to an update.
///
/// Equality and hashing are **by [`name`](Package::name) only**. Two packages
/// with the same name but different versions are considered equal.
#[derive(Debug, Clone)]
pub struct Package {
    /// The package name.
    pub name: String,
    before: VersionCheck,
    after: VersionCheck,
    required: Option<RPMVersion>,
    current: VersionCheck,
}

/// The outcome of checking one of a package's versions on a target.
///
/// `Option<RPMVersion>` cannot carry this: it has no way to say *why* it is
/// empty, and "the check ran and the package was absent" and "no check ever
/// ran" are different facts the export must render differently (#396). Keeping
/// both in one field means the value and the fact that it was measured cannot
/// drift apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum VersionCheck {
    /// No check has run — the version is simply unknown.
    #[default]
    NotChecked,
    /// A check ran and found the package not installed.
    NotInstalled,
    /// A check ran and found this version installed.
    Installed(RPMVersion),
}

impl VersionCheck {
    /// The version found, or `None` when the package was absent or unchecked.
    #[must_use]
    pub fn version(&self) -> Option<&RPMVersion> {
        match self {
            Self::Installed(v) => Some(v),
            Self::NotChecked | Self::NotInstalled => None,
        }
    }

    /// Whether a check ran at all — `false` only for
    /// [`NotChecked`](VersionCheck::NotChecked).
    #[must_use]
    pub fn is_checked(&self) -> bool {
        !matches!(self, Self::NotChecked)
    }
}

impl From<Option<RPMVersion>> for VersionCheck {
    /// Records the result of a check that ran: `None` is *observed absent*,
    /// never *unchecked*. Only [`VersionCheck::default`] produces
    /// [`NotChecked`](VersionCheck::NotChecked).
    fn from(ver: Option<RPMVersion>) -> Self {
        match ver {
            Some(v) => Self::Installed(v),
            None => Self::NotInstalled,
        }
    }
}

impl Package {
    /// Creates a new [`Package`] with the given name and no versions set.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            before: VersionCheck::NotChecked,
            after: VersionCheck::NotChecked,
            required: None,
            current: VersionCheck::NotChecked,
        }
    }

    /// The before-version check, including whether it ran at all.
    #[must_use]
    pub fn before_check(&self) -> &VersionCheck {
        &self.before
    }

    /// The after-side counterpart of [`before_check`](Package::before_check).
    #[must_use]
    pub fn after_check(&self) -> &VersionCheck {
        &self.after
    }

    /// The version of the package before an update, if one was found.
    ///
    /// `None` covers both "checked, not installed" and "never checked" — use
    /// [`before_check`](Package::before_check) when the difference matters.
    #[must_use]
    pub fn before(&self) -> Option<&RPMVersion> {
        self.before.version()
    }

    /// The version of the package after an update, if one was found.
    ///
    /// See [`before`](Package::before) on what `None` does and does not say.
    #[must_use]
    pub fn after(&self) -> Option<&RPMVersion> {
        self.after.version()
    }

    /// The version required by the update metadata, if known.
    #[must_use]
    pub fn required(&self) -> Option<&RPMVersion> {
        self.required.as_ref()
    }

    /// The version currently installed on a target, if known.
    ///
    /// `None` covers both "checked, not installed" and "never checked" — use
    /// [`current_check`](Package::current_check) when the difference matters.
    #[must_use]
    pub fn current(&self) -> Option<&RPMVersion> {
        self.current.version()
    }

    /// The current-version check, including whether it ran at all.
    #[must_use]
    pub fn current_check(&self) -> &VersionCheck {
        &self.current
    }

    /// Sets the [`before`](Package::before) version from an optional string,
    /// recording a completed check either way: `None` (or an empty string)
    /// stores [`NotInstalled`](VersionCheck::NotInstalled), not
    /// [`NotChecked`](VersionCheck::NotChecked).
    ///
    /// # Errors
    /// Returns [`RpmVersionParseError`](crate::error::RpmVersionParseError) only
    /// for a non-empty string that fails to parse. An empty string is treated
    /// as "checked, absent" and never errors.
    pub fn set_before(&mut self, ver: Option<&str>) -> crate::error::Result<()> {
        self.before = parse_opt(ver)?.into();
        Ok(())
    }

    /// Sets the [`after`](Package::after) version from an optional string.
    ///
    /// # Errors
    /// See [`set_before`](Package::set_before).
    pub fn set_after(&mut self, ver: Option<&str>) -> crate::error::Result<()> {
        self.after = parse_opt(ver)?.into();
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

    /// Sets the [`before`](Package::before) version directly, recording a
    /// completed check — `None` is *observed absent*, not *never checked*.
    pub fn set_before_version(&mut self, ver: Option<RPMVersion>) {
        self.before = ver.into();
    }

    /// Sets the [`after`](Package::after) version directly, recording a
    /// completed check — `None` is *observed absent*, not *never checked*.
    pub fn set_after_version(&mut self, ver: Option<RPMVersion>) {
        self.after = ver.into();
    }

    /// Sets the [`before`](Package::before) check outcome directly, including
    /// [`NotChecked`](VersionCheck::NotChecked).
    pub fn set_before_check(&mut self, check: VersionCheck) {
        self.before = check;
    }

    /// Sets the [`after`](Package::after) check outcome directly, including
    /// [`NotChecked`](VersionCheck::NotChecked).
    pub fn set_after_check(&mut self, check: VersionCheck) {
        self.after = check;
    }

    /// Sets the [`current`](Package::current) version directly, recording a
    /// completed check — `None` is *observed absent*, not *never checked*.
    pub fn set_current_version(&mut self, ver: Option<RPMVersion>) {
        self.current = ver.into();
    }

    /// Sets the [`current`](Package::current) check outcome directly,
    /// including [`NotChecked`](VersionCheck::NotChecked) — the version
    /// query never having answered for this package at all (#437).
    pub fn set_current_check(&mut self, check: VersionCheck) {
        self.current = check;
    }
}

/// Parses an optional version string, treating `None`/empty as "no version".
///
/// The before/after setters feed the result through
/// [`VersionCheck::from`], which reads that as *checked, not installed*;
/// [`required`](Package::required) keeps it as a plain absent value, since
/// metadata declares a requirement rather than observing one.
fn parse_opt(ver: Option<&str>) -> crate::error::Result<Option<RPMVersion>> {
    match ver {
        Some(v) if !v.is_empty() => Ok(Some(RPMVersion::parse(v)?)),
        _ => Ok(None),
    }
}

impl std::fmt::Display for Package {
    /// Returns the package name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl PartialEq for Package {
    /// Equal by [`name`](Package::name) only.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Package {}

impl std::hash::Hash for Package {
    /// Hashes by [`name`](Package::name) only.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// #396: setting a `None` version records a check that found nothing —
    /// `NotInstalled` and `NotChecked` must stay distinguishable, and both
    /// must keep reading as `None` through [`Package::before`]/[`Package::after`].
    #[test]
    fn set_version_none_is_not_installed_not_unchecked() {
        let mut p = Package::new("bash");
        assert_eq!(p.before_check(), &VersionCheck::NotChecked);
        assert_eq!(p.after_check(), &VersionCheck::NotChecked);
        p.set_before_version(None);
        assert_eq!(p.before_check(), &VersionCheck::NotInstalled);
        assert_eq!(
            p.after_check(),
            &VersionCheck::NotChecked,
            "setting one side must not mark the other"
        );
        p.set_after_version(None);
        assert_eq!(p.after_check(), &VersionCheck::NotInstalled);
        assert!(p.before().is_none() && p.after().is_none());

        let mut p = Package::new("bash2");
        p.set_after(None).unwrap();
        assert_eq!(p.after_check(), &VersionCheck::NotInstalled);
        assert_eq!(p.before_check(), &VersionCheck::NotChecked);
        p.set_after(Some("2.0-1")).unwrap();
        assert!(p.after_check().is_checked() && p.after().is_some());
        p.set_after_check(VersionCheck::NotChecked);
        assert!(!p.after_check().is_checked() && p.after().is_none());
    }

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

    /// #437: `current` carries the same tri-state as `before`/`after` — a
    /// version query that never answered for a package must not read the
    /// same as one that confirmed it absent.
    #[test]
    fn current_check_distinguishes_unchecked_from_not_installed() {
        let mut p = Package::new("bash");
        assert_eq!(p.current_check(), &VersionCheck::NotChecked);
        p.set_current_check(VersionCheck::NotInstalled);
        assert_eq!(p.current_check(), &VersionCheck::NotInstalled);
        assert!(p.current().is_none());
        p.set_current_version(Some(RPMVersion::parse("3.0-1").unwrap()));
        assert_eq!(
            p.current_check(),
            &VersionCheck::Installed(RPMVersion::parse("3.0-1").unwrap())
        );
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
