//! Target-host system description, ported from `mtui/types/systems.py` and the
//! flat product tuple in `mtui/types/product.py`.
//!
//! A [`System`] captures the products installed on a reference host: a
//! [`base`](System::get_base) product plus a set of add-on modules/extensions.
//! It is used for pretty-printing and for driving correct update handling.
//!
//! ## The two `Product` types
//!
//! Upstream has *two* distinct `Product` types:
//!
//! * a rich refhost product whose `version` is structured — ported as
//!   [`crate::product::Product`]; and
//! * a flat `NamedTuple` `(name, version: str, arch)` used by `System` — ported
//!   here as [`SystemProduct`] to avoid a name clash with the refhost variant.
//!
//! ## Deviations from upstream
//!
//! Addons are stored in a [`BTreeSet`] (rather than an unordered `set`) so that
//! [`pretty`](System::pretty), [`flatten`](System::flatten), hashing, and
//! equality are all deterministic. `get_release` guards the upstream
//! `version[:2]` slice against versions shorter than two characters instead of
//! risking an out-of-bounds panic.

use std::collections::BTreeSet;

use thiserror::Error;

/// A flat product tuple: `(name, version, arch)`.
///
/// Ported from the upstream `Product` `NamedTuple` in
/// `mtui/types/product.py`; renamed to avoid clashing with the refhost
/// [`Product`](crate::product::Product).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemProduct {
    /// The product name (e.g. `SLES`, `sle-module-basesystem`).
    pub name: String,
    /// The product version (e.g. `15.5`).
    pub version: String,
    /// The product architecture (e.g. `x86_64`).
    pub arch: String,
}

impl SystemProduct {
    /// Creates a new [`SystemProduct`].
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        arch: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            arch: arch.into(),
        }
    }
}

/// Error raised when a system's base product name maps to no known release.
///
/// Mirrors upstream `UnknownSystemError(ValueError)` raised by
/// `System.get_release`.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("unknown system: {name}")]
pub struct UnknownSystemError {
    /// The unrecognised base product name.
    name: String,
}

/// The system information of a target host: a base product plus addons.
#[derive(Debug, Clone)]
pub struct System {
    base: SystemProduct,
    addons: BTreeSet<SystemProduct>,
    /// `true` when `/etc/products.d/baseproduct` is a dangling symlink, so
    /// [`base`](System::get_base) is a best-effort guess from the symlink
    /// target name rather than a parsed product file.
    pub dangling_base: bool,
}

impl System {
    /// Creates a new [`System`] from a base product and optional addons.
    #[must_use]
    pub fn new(base: SystemProduct, addons: BTreeSet<SystemProduct>, dangling_base: bool) -> Self {
        Self {
            base,
            addons,
            dangling_base,
        }
    }

    /// Determines the release identifier for this system.
    ///
    /// Mirrors the upstream `get_release` mapping exactly.
    ///
    /// # Errors
    /// Returns [`UnknownSystemError`] when the base product name maps to no
    /// known release.
    pub fn get_release(&self) -> Result<String, UnknownSystemError> {
        let name = self.base.name.as_str();
        let release = match name {
            "SUSE-Manager-Server" => "15".to_owned(),
            "rhel" => "YUM".to_owned(),
            "SLES" | "SLED" | "SUSE_SLES" | "SLES_SAP" | "SUSE_SLES_SAP" | "SLE_HPC"
            | "SLES_TERADATA" | "SLE_RT" => {
                // Upstream `version[:2]`; guard short versions against a panic.
                self.base.version.chars().take(2).collect()
            }
            "openSUSE" => "15".to_owned(),
            "sle-studioonsite" => "11".to_owned(),
            "SL-Micro" => "slmicro".to_owned(),
            _ => {
                return Err(UnknownSystemError {
                    name: name.to_owned(),
                });
            }
        };
        Ok(release)
    }

    /// Returns the addons of the system.
    #[must_use]
    pub fn get_addons(&self) -> &BTreeSet<SystemProduct> {
        &self.addons
    }

    /// Returns the base product of the system.
    #[must_use]
    pub fn get_base(&self) -> &SystemProduct {
        &self.base
    }

    /// Returns a flattened set of all products (base + addons).
    #[must_use]
    pub fn flatten(&self) -> BTreeSet<SystemProduct> {
        let mut flat = self.addons.clone();
        flat.insert(self.base.clone());
        flat
    }

    /// Returns a pretty-printed, human-readable description of the system.
    ///
    /// Mirrors the upstream `pretty` layout, including the 53-column left-padded
    /// addon name field.
    #[must_use]
    pub fn pretty(&self) -> Vec<String> {
        let mut msg = vec![format!(
            "  Base product: {}-{}-{}",
            self.base.name, self.base.version, self.base.arch
        )];
        if !self.addons.is_empty() {
            msg.push("  Installed Extensions and Modules:".to_owned());
            for x in &self.addons {
                msg.push(format!(
                    "      Addon: {:<53} - version: {}",
                    x.name, x.version
                ));
            }
        }
        msg
    }
}

impl std::fmt::Display for System {
    /// Renders `{name-lowercase}{-modules?}-{version}-{arch}`, mirroring
    /// upstream `__str__`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let modules = if self.addons.is_empty() {
            ""
        } else {
            "-modules"
        };
        write!(
            f,
            "{}{}-{}-{}",
            self.base.name.to_lowercase(),
            modules,
            self.base.version,
            self.base.arch
        )
    }
}

impl PartialEq for System {
    /// Equal by `(base, addons)`, mirroring upstream `__eq__`.
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base && self.addons == other.addons
    }
}

impl Eq for System {}

impl std::hash::Hash for System {
    /// Hashes by `(base, addons)`, mirroring upstream `__hash__`.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.base.hash(state);
        self.addons.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(name: &str, version: &str) -> SystemProduct {
        SystemProduct::new(name, version, "x86_64")
    }

    fn sys(name: &str, version: &str) -> System {
        System::new(base(name, version), BTreeSet::new(), false)
    }

    #[test]
    fn release_sles_family_takes_first_two_chars() {
        for name in [
            "SLES",
            "SLED",
            "SUSE_SLES",
            "SLES_SAP",
            "SUSE_SLES_SAP",
            "SLE_HPC",
            "SLES_TERADATA",
            "SLE_RT",
        ] {
            assert_eq!(sys(name, "15.5").get_release().unwrap(), "15");
        }
    }

    #[test]
    fn release_short_version_does_not_panic() {
        assert_eq!(sys("SLES", "9").get_release().unwrap(), "9");
        assert_eq!(sys("SLES", "").get_release().unwrap(), "");
    }

    #[test]
    fn release_fixed_mappings() {
        assert_eq!(
            sys("SUSE-Manager-Server", "4.3").get_release().unwrap(),
            "15"
        );
        assert_eq!(sys("rhel", "9").get_release().unwrap(), "YUM");
        assert_eq!(sys("openSUSE", "15.5").get_release().unwrap(), "15");
        assert_eq!(sys("sle-studioonsite", "1").get_release().unwrap(), "11");
        assert_eq!(sys("SL-Micro", "6.0").get_release().unwrap(), "slmicro");
    }

    #[test]
    fn release_unknown_errors() {
        let err = sys("gentoo", "1").get_release().unwrap_err();
        assert_eq!(err.name, "gentoo");
        assert_eq!(err.to_string(), "unknown system: gentoo");
    }

    #[test]
    fn display_without_addons() {
        assert_eq!(sys("SLES", "15.5").to_string(), "sles-15.5-x86_64");
    }

    #[test]
    fn display_with_addons() {
        let mut addons = BTreeSet::new();
        addons.insert(base("sle-module-basesystem", "15.5"));
        let s = System::new(base("SLES", "15.5"), addons, false);
        assert_eq!(s.to_string(), "sles-modules-15.5-x86_64");
    }

    #[test]
    fn pretty_without_addons() {
        let s = sys("SLES", "15.5");
        assert_eq!(s.pretty(), vec!["  Base product: SLES-15.5-x86_64"]);
    }

    #[test]
    fn pretty_with_addons() {
        let mut addons = BTreeSet::new();
        addons.insert(base("sle-module-basesystem", "15.5"));
        let s = System::new(base("SLES", "15.5"), addons, false);
        let pretty = s.pretty();
        assert_eq!(pretty[0], "  Base product: SLES-15.5-x86_64");
        assert_eq!(pretty[1], "  Installed Extensions and Modules:");
        // Addon name is left-padded to 53 columns (upstream `{x.name:<53}`).
        assert_eq!(
            pretty[2],
            format!(
                "      Addon: {:<53} - version: 15.5",
                "sle-module-basesystem"
            )
        );
    }

    #[test]
    fn flatten_includes_base_and_addons() {
        let mut addons = BTreeSet::new();
        addons.insert(base("sle-module-basesystem", "15.5"));
        let s = System::new(base("SLES", "15.5"), addons, false);
        let flat = s.flatten();
        assert_eq!(flat.len(), 2);
        assert!(flat.contains(&base("SLES", "15.5")));
        assert!(flat.contains(&base("sle-module-basesystem", "15.5")));
    }

    #[test]
    fn getters_return_base_and_addons() {
        let mut addons = BTreeSet::new();
        addons.insert(base("mod", "1"));
        let s = System::new(base("SLES", "15.5"), addons, true);
        assert_eq!(s.get_base(), &base("SLES", "15.5"));
        assert_eq!(s.get_addons().len(), 1);
        assert!(s.dangling_base);
    }

    #[test]
    fn equality_and_hash_by_base_and_addons() {
        use std::collections::HashSet;
        let a = sys("SLES", "15.5");
        let b = sys("SLES", "15.5");
        assert_eq!(a, b);
        assert_ne!(a, sys("SLED", "15.5"));

        let mut set = HashSet::new();
        set.insert(a);
        assert!(!set.insert(b));
        assert_eq!(set.len(), 1);
    }
}
