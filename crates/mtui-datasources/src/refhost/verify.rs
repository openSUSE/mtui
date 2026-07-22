//! Compare a connected host's installed products against `refhosts.yml`.
//!
//! Ported from upstream `mtui/hosts/refhost/verify.py`.
//!
//! When mtui connects a reference host it can know two things about that host's
//! products: what is *actually installed* (parsed from `/etc/products.d` into a
//! [`System`]) and what the `refhosts.yml` metadata *says* should be there (a
//! [`Host`] row). [`compare`] checks the two for drift — a wrong or
//! wrong-version base product, a wrong architecture, missing or extra addons, or
//! a dangling `baseproduct` symlink — so that validating an update on a host
//! that is not the system we think it is gets surfaced. The check is advisory:
//! callers warn and keep the host.
//!
//! # Normalization (grounded on real refhosts across SLE 15/16 and SL-Micro)
//!
//! * Detected product *names* use the same identifiers as `refhosts.yml`
//!   (`SLES`, `SLE_RT`, `SLES-LTSS`, `SL-Micro`, `sle-module-*` …); compared
//!   case-insensitively for safety.
//! * Detected *version strings* come from upstream `parse_product`: `"15-SP4"`
//!   (service packs), `"16.0"` / `"6.1"` (dotted). Both are normalized to a
//!   refhosts [`Version`].
//! * `qa` is dropped from the comparison on the refhosts side because the
//!   detected-side parser intentionally skips `qa.prod` — keeping it would
//!   report a phantom "missing qa" on every host whose metadata lists it.
//!
//! # Divergence from upstream
//!
//! Upstream `Version` collapses `int | str` into one field; the mtui
//! [`Version`] preserves the numeric-vs-textual distinction via
//! [`VersionField`]. [`normalize_version`] therefore yields
//! [`VersionField::Text`] for a service-pack minor (`"SP4"`) and
//! [`VersionField::Num`] for a dotted numeric minor (`"0"`), matching how
//! `refhosts.yml` metadata deserializes so exact-match hosts compare equal.
//! Name case-folding uses ASCII-only [`str::eq_ignore_ascii_case`] (product
//! names are ASCII in practice) rather than Python's full-Unicode `casefold`.

use std::collections::BTreeMap;

use mtui_types::{Addon, Host, System, Version, VersionField};

/// Addon names the detected-side parser never reports as installed (it skips
/// `qa.prod`); excluded from the refhosts side too so they are not reported as
/// missing. Compared case-folded (ASCII).
const IGNORED_ADDONS: &[&str] = &["qa"];

fn is_ignored_addon(name: &str) -> bool {
    IGNORED_ADDONS
        .iter()
        .any(|ignored| ignored.eq_ignore_ascii_case(name))
}

/// Parse a single field value: numeric when it parses as `u64`, else textual.
///
/// Mirrors upstream `_int_or_str`.
fn int_or_str(value: &str) -> VersionField {
    match value.parse::<u64>() {
        Ok(n) => VersionField::Num(n),
        Err(_) => VersionField::Text(value.to_owned()),
    }
}

/// Convert a detected version string to a refhosts [`Version`].
///
/// Handles the formats emitted by upstream `parse_product`:
///
/// * `"15-SP4"` → `Version { major: Num(15), minor: Text("SP4") }`
/// * `"16.0"` / `"6.1"` → `Version { major: Num(16), minor: Num(0) }` /
///   `Version { major: Num(6), minor: Num(1) }`
/// * `"15"` → `Version { major: Num(15), minor: None }`
/// * `""` / whitespace-only → `None`
///
/// A leading numeric segment parses as [`VersionField::Num`]; anything else
/// (e.g. an odd non-numeric major) is kept as [`VersionField::Text`], mirroring
/// upstream `_int_or_str`.
#[must_use]
pub fn normalize_version(version: &str) -> Option<Version> {
    let version = version.trim();
    if version.is_empty() {
        return None;
    }

    // `<major>-SP<minor>` service-pack form (SLE 12/15).
    if let Some((major, sp)) = parse_service_pack(version) {
        return Some(Version {
            major: VersionField::Num(major),
            minor: Some(VersionField::Text(format!("SP{sp}"))),
        });
    }

    // Dotted form (`16.0`, `6.1`): split on the first `.`.
    if let Some((major, minor)) = version.split_once('.') {
        return Some(Version {
            major: int_or_str(major),
            minor: Some(int_or_str(minor)),
        });
    }

    // Major only.
    Some(Version {
        major: int_or_str(version),
        minor: None,
    })
}

/// Match `<digits>-SP<digits>` exactly, returning `(major, sp)` when it does.
///
/// Avoids a `regex` dependency for what is a fixed, anchored grammar.
fn parse_service_pack(version: &str) -> Option<(u64, u64)> {
    let (major, rest) = version.split_once("-SP")?;
    let major: u64 = major.parse().ok()?;
    // The whole remainder must be digits (upstream `re.fullmatch`).
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sp: u64 = rest.parse().ok()?;
    Some((major, sp))
}

/// Render a [`Version`] for warning messages (`""` when `None`).
///
/// Mirrors upstream `_fmt_version`: a numeric minor uses a `.` separator, a
/// textual minor uses `-`, and an absent/empty-sentinel minor renders as major
/// only.
fn fmt_version(version: Option<&Version>) -> String {
    let Some(version) = version else {
        return String::new();
    };
    match &version.minor {
        None => version.major.to_string(),
        Some(VersionField::Text(s)) if s.is_empty() => version.major.to_string(),
        Some(VersionField::Num(n)) => format!("{}.{n}", version.major),
        Some(VersionField::Text(s)) => format!("{}-{s}", version.major),
    }
}

/// Render `"name x.y"` (or just `"name"` when the version renders empty).
///
/// Mirrors upstream `_named`.
fn named(name: &str, version: Option<&Version>) -> String {
    let rendered = fmt_version(version);
    if rendered.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {rendered}")
    }
}

/// Outcome of comparing detected products against refhosts metadata.
///
/// Mirrors upstream `ProductDiff`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductDiff {
    /// `/etc/products.d/baseproduct` is a dangling symlink.
    pub dangling_base: bool,
    /// Human-readable base product mismatch, or `None` when the base matches.
    pub base_mismatch: Option<String>,
    /// Addons in `refhosts.yml` but not installed (`"name x.y"`).
    pub missing_addons: Vec<String>,
    /// Addons installed but not in `refhosts.yml` (`"name x.y"`).
    pub extra_addons: Vec<String>,
    /// Addons present on both sides with a differing version.
    pub mismatched_addons: Vec<String>,
}

impl ProductDiff {
    /// `true` when no drift was detected.
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.dangling_base
            && self.base_mismatch.is_none()
            && self.missing_addons.is_empty()
            && self.extra_addons.is_empty()
            && self.mismatched_addons.is_empty()
    }

    /// One warning line per drift class (empty when [`ok`](Self::ok)).
    ///
    /// Addon lists are sorted for a stable, human-readable message, mirroring
    /// upstream's `", ".join(sorted(...))`.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if self.dangling_base {
            out.push("dangling /etc/products.d/baseproduct symlink".to_owned());
        }
        if let Some(mismatch) = &self.base_mismatch {
            out.push(format!("base product mismatch: {mismatch}"));
        }
        if !self.missing_addons.is_empty() {
            out.push(format!(
                "addons in metadata but not installed: {}",
                sorted_join(&self.missing_addons)
            ));
        }
        if !self.extra_addons.is_empty() {
            out.push(format!(
                "addons installed but not in metadata: {}",
                sorted_join(&self.extra_addons)
            ));
        }
        if !self.mismatched_addons.is_empty() {
            out.push(format!(
                "addons with version mismatch: {}",
                sorted_join(&self.mismatched_addons)
            ));
        }
        out
    }
}

/// Sort a clone of `items` and join with `", "` (upstream `", ".join(sorted)`).
fn sorted_join(items: &[String]) -> String {
    let mut sorted = items.to_vec();
    sorted.sort();
    sorted.join(", ")
}

/// Compare a detected [`System`] against a refhosts [`Host`] row.
///
/// Returns a [`ProductDiff`] describing any base/addon/symlink drift.
///
/// The base name/version check is skipped when the base is a dangling
/// placeholder (the dangling warning already covers it), but the architecture
/// is still checked. Addons are matched case-folded by name, with the
/// always-ignored addons (`qa`) dropped from both sides.
#[must_use]
pub fn compare(system: &System, host: &Host) -> ProductDiff {
    let mut diff = ProductDiff {
        dangling_base: system.dangling_base,
        ..Default::default()
    };

    let base = system.get_base();
    let expected = &host.product;

    let mut problems: Vec<String> = Vec::new();
    if !diff.dangling_base {
        if !base.name.eq_ignore_ascii_case(&expected.name) {
            problems.push(format!(
                "name {:?} != {:?} (metadata)",
                base.name, expected.name
            ));
        }
        let detected_version = normalize_version(&base.version);
        if let Some(expected_version) = &expected.version
            && detected_version.as_ref() != Some(expected_version)
        {
            problems.push(format!(
                "version {:?} != {:?} (metadata)",
                base.version,
                fmt_version(Some(expected_version))
            ));
        }
    }
    if !base.arch.is_empty() && !host.arch.is_empty() && base.arch != host.arch {
        problems.push(format!(
            "arch {:?} != {:?} (metadata)",
            base.arch, host.arch
        ));
    }
    if !problems.is_empty() {
        diff.base_mismatch = Some(problems.join("; "));
    }

    // Addons: case-folded name -> (display name, normalized version), with the
    // always-ignored addons (qa) dropped from both sides.
    let detected: BTreeMap<String, (String, Option<Version>)> = system
        .get_addons()
        .iter()
        .filter(|p| !is_ignored_addon(&p.name))
        .map(|p| {
            (
                p.name.to_ascii_lowercase(),
                (p.name.clone(), normalize_version(&p.version)),
            )
        })
        .collect();
    let metadata: BTreeMap<String, (String, Option<Version>)> = host
        .addons
        .iter()
        .filter(|a| !is_ignored_addon(&a.name))
        .map(|a: &Addon| {
            (
                a.name.to_ascii_lowercase(),
                (a.name.clone(), a.version.clone()),
            )
        })
        .collect();

    for (key, (name, version)) in &metadata {
        if !detected.contains_key(key) {
            diff.missing_addons.push(named(name, version.as_ref()));
        }
    }
    for (key, (name, version)) in &detected {
        if !metadata.contains_key(key) {
            diff.extra_addons.push(named(name, version.as_ref()));
        }
    }
    for (key, (det_name, det_version)) in &detected {
        let Some((_, meta_version)) = metadata.get(key) else {
            continue;
        };
        if let Some(meta_version) = meta_version
            && det_version.as_ref() != Some(meta_version)
        {
            diff.mismatched_addons.push(format!(
                "{det_name} (installed {} != metadata {})",
                fmt_version(det_version.as_ref()),
                fmt_version(Some(meta_version))
            ));
        }
    }

    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_types::{Product, SystemProduct};
    use std::collections::BTreeSet;

    // --- builders mirroring the upstream test helpers ------------------------

    fn detected_product(name: &str, version: &str, arch: &str) -> SystemProduct {
        SystemProduct::new(name, version, arch)
    }

    /// Build a detected `System` from `(name, version, arch)` tuples.
    fn system(base: (&str, &str, &str), addons: &[(&str, &str, &str)], dangling: bool) -> System {
        let addon_set: BTreeSet<SystemProduct> = addons
            .iter()
            .map(|(n, v, a)| detected_product(n, v, a))
            .collect();
        System::new(
            detected_product(base.0, base.1, base.2),
            addon_set,
            dangling,
        )
    }

    fn version(major: u64, minor: Option<VersionField>) -> Version {
        Version {
            major: VersionField::Num(major),
            minor,
        }
    }

    /// Build a refhosts `Host` from `(name, major, minor)` tuples.
    fn host(
        arch: &str,
        product: (&str, u64, Option<VersionField>),
        addons: &[(&str, u64, Option<VersionField>)],
    ) -> Host {
        Host {
            name: "h".to_owned(),
            arch: arch.to_owned(),
            product: Product {
                name: product.0.to_owned(),
                version: Some(version(product.1, product.2)),
            },
            addons: addons
                .iter()
                .map(|(n, maj, min)| Addon {
                    name: (*n).to_owned(),
                    version: Some(version(*maj, min.clone())),
                })
                .collect(),
        }
    }

    fn sp(n: u64) -> Option<VersionField> {
        Some(VersionField::Text(format!("SP{n}")))
    }

    fn num(n: u64) -> Option<VersionField> {
        Some(VersionField::Num(n))
    }

    // --- normalize_version (TestNormalizeVersion) ---------------------------

    #[test]
    fn normalize_dotted_int_minor() {
        // SLE 16 / SL-Micro: "16.0" -> Version(16, 0), "6.1" -> Version(6, 1).
        assert_eq!(normalize_version("16.0"), Some(version(16, num(0))));
        assert_eq!(normalize_version("6.1"), Some(version(6, num(1))));
    }

    #[test]
    fn normalize_service_pack() {
        // SLE 12/15: "15-SP4" -> Version(15, "SP4").
        assert_eq!(normalize_version("15-SP4"), Some(version(15, sp(4))));
    }

    #[test]
    fn normalize_major_only() {
        assert_eq!(normalize_version("15"), Some(version(15, None)));
    }

    #[test]
    fn normalize_empty_is_none() {
        assert_eq!(normalize_version(""), None);
        assert_eq!(normalize_version("   "), None);
    }

    #[test]
    fn normalize_service_pack_equals_metadata_text_minor() {
        // The Num/Text distinction must line up: a detected "15-SP4" must
        // compare equal to metadata whose minor deserializes as Text("SP4").
        let detected = normalize_version("15-SP4").unwrap();
        let metadata = version(15, sp(4));
        assert_eq!(detected, metadata);
    }

    #[test]
    fn normalize_dotted_equals_metadata_num_minor() {
        let detected = normalize_version("16.0").unwrap();
        let metadata = version(16, num(0));
        assert_eq!(detected, metadata);
    }

    // --- compare: base (TestCompareBase) ------------------------------------

    #[test]
    fn sle16_exact_match_no_warnings() {
        let sys = system(("SLES", "16.0", "x86_64"), &[], false);
        let h = host("x86_64", ("SLES", 16, num(0)), &[]);
        let diff = compare(&sys, &h);
        assert!(diff.ok());
        assert_eq!(diff.warnings(), Vec::<String>::new());
    }

    #[test]
    fn slmicro_exact_match_no_warnings() {
        let sys = system(
            ("SL-Micro", "6.1", "x86_64"),
            &[("SL-Micro-Extras", "6.1", "x86_64")],
            false,
        );
        let h = host(
            "x86_64",
            ("SL-Micro", 6, num(1)),
            &[("SL-Micro-Extras", 6, num(1))],
        );
        assert!(compare(&sys, &h).ok());
    }

    #[test]
    fn base_name_mismatch() {
        let sys = system(("SLED", "15-SP4", "x86_64"), &[], false);
        let h = host("x86_64", ("SLES", 15, sp(4)), &[]);
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        let mismatch = diff.base_mismatch.as_deref().unwrap_or("");
        assert!(mismatch.contains("name"));
    }

    #[test]
    fn base_name_casefold_matches() {
        // Names differing only in case are treated as equal.
        let sys = system(("sles", "16.0", "x86_64"), &[], false);
        let h = host("x86_64", ("SLES", 16, num(0)), &[]);
        assert!(compare(&sys, &h).ok());
    }

    #[test]
    fn base_version_mismatch() {
        let sys = system(("SLES", "15-SP4", "x86_64"), &[], false);
        let h = host("x86_64", ("SLES", 15, sp(3)), &[]);
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        assert!(
            diff.base_mismatch
                .as_deref()
                .unwrap_or("")
                .contains("version")
        );
    }

    #[test]
    fn arch_mismatch() {
        let sys = system(("SLES", "16.0", "x86_64"), &[], false);
        let h = host("aarch64", ("SLES", 16, num(0)), &[]);
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        assert!(diff.base_mismatch.as_deref().unwrap_or("").contains("arch"));
    }

    // --- compare: addons (TestCompareAddons) --------------------------------

    #[test]
    fn extra_addon_detected() {
        // bojack-style drift: host has modules absent from metadata.
        let sys = system(
            ("SLE_RT", "15-SP4", "x86_64"),
            &[
                ("SLES-LTSS", "15-SP4", "x86_64"),
                ("sle-module-basesystem", "15-SP4", "x86_64"),
                ("sle-module-development-tools", "15-SP4", "x86_64"),
            ],
            false,
        );
        let h = host(
            "x86_64",
            ("SLE_RT", 15, sp(4)),
            &[
                ("SLES-LTSS", 15, sp(4)),
                ("sle-module-basesystem", 15, sp(4)),
            ],
        );
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        assert_eq!(
            diff.extra_addons,
            vec!["sle-module-development-tools 15-SP4".to_owned()]
        );
        assert_eq!(diff.missing_addons, Vec::<String>::new());
    }

    #[test]
    fn missing_addon_detected() {
        let sys = system(
            ("SLES", "15-SP4", "x86_64"),
            &[("sle-module-basesystem", "15-SP4", "x86_64")],
            false,
        );
        let h = host(
            "x86_64",
            ("SLES", 15, sp(4)),
            &[
                ("sle-module-basesystem", 15, sp(4)),
                ("SLES-LTSS", 15, sp(4)),
            ],
        );
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        assert_eq!(diff.missing_addons, vec!["SLES-LTSS 15-SP4".to_owned()]);
        assert_eq!(diff.extra_addons, Vec::<String>::new());
    }

    #[test]
    fn addon_version_mismatch() {
        let sys = system(
            ("SLES", "15-SP4", "x86_64"),
            &[("sle-module-basesystem", "15-SP3", "x86_64")],
            false,
        );
        let h = host(
            "x86_64",
            ("SLES", 15, sp(4)),
            &[("sle-module-basesystem", 15, sp(4))],
        );
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        assert_eq!(diff.missing_addons, Vec::<String>::new());
        assert_eq!(diff.extra_addons, Vec::<String>::new());
        assert_eq!(diff.mismatched_addons.len(), 1);
        assert!(diff.mismatched_addons[0].contains("sle-module-basesystem"));
    }

    #[test]
    fn qa_excluded_from_both_sides() {
        // The detected side never reports qa, so metadata's qa must not be
        // reported as missing, and a detected qa not as extra.
        let sys = system(
            ("SLES", "15-SP4", "x86_64"),
            &[("sle-module-basesystem", "15-SP4", "x86_64")],
            false,
        );
        let h = host(
            "x86_64",
            ("SLES", 15, sp(4)),
            &[("sle-module-basesystem", 15, sp(4)), ("qa", 15, sp(4))],
        );
        assert!(compare(&sys, &h).ok());
    }

    #[test]
    fn full_sle15_match_no_warnings() {
        // antares-style host whose module set matches metadata exactly.
        let modules = [
            "sle-module-basesystem",
            "sle-module-server-applications",
            "sle-module-desktop-applications",
            "sle-module-development-tools",
            "sle-module-web-scripting",
        ];
        let mut sys_addons: Vec<(&str, &str, &str)> =
            modules.iter().map(|m| (*m, "15-SP4", "aarch64")).collect();
        sys_addons.push(("SLES-LTSS", "15-SP4", "aarch64"));
        let sys = system(("SLES", "15-SP4", "aarch64"), &sys_addons, false);

        let mut meta_addons: Vec<(&str, u64, Option<VersionField>)> =
            modules.iter().map(|m| (*m, 15u64, sp(4))).collect();
        meta_addons.push(("SLES-LTSS", 15, sp(4)));
        let h = host("aarch64", ("SLES", 15, sp(4)), &meta_addons);

        assert!(compare(&sys, &h).ok());
    }

    // --- compare: dangling base (TestCompareDangling) -----------------------

    #[test]
    fn dangling_base_warns_and_skips_base_check() {
        // A dangling baseproduct symlink is reported; the (placeholder) base
        // name/version are not additionally flagged as a mismatch.
        let sys = system(("SLES", "", "x86_64"), &[], true);
        let h = host("x86_64", ("SLES", 16, num(0)), &[]);
        let diff = compare(&sys, &h);
        assert!(!diff.ok());
        assert!(diff.dangling_base);
        assert_eq!(diff.base_mismatch, None);
        assert!(diff.warnings().iter().any(|w| w.contains("dangling")));
    }

    #[test]
    fn dangling_still_checks_arch() {
        // Arch is still compared even with a dangling base.
        let sys = system(("SLES", "", "x86_64"), &[], true);
        let h = host("aarch64", ("SLES", 16, num(0)), &[]);
        let diff = compare(&sys, &h);
        assert!(diff.dangling_base);
        assert!(diff.base_mismatch.as_deref().unwrap_or("").contains("arch"));
    }

    // --- helper coverage ----------------------------------------------------

    #[test]
    fn fmt_version_variants() {
        assert_eq!(fmt_version(None), "");
        assert_eq!(fmt_version(Some(&version(15, None))), "15");
        assert_eq!(fmt_version(Some(&version(16, num(0)))), "16.0");
        assert_eq!(fmt_version(Some(&version(15, sp(4)))), "15-SP4");
        // Empty-string Text sentinel renders as major only.
        assert_eq!(
            fmt_version(Some(&version(11, Some(VersionField::Text(String::new()))))),
            "11"
        );
    }

    #[test]
    fn named_renders_name_and_version() {
        assert_eq!(named("sdk", Some(&version(15, num(5)))), "sdk 15.5");
        assert_eq!(named("sdk", None), "sdk");
    }

    #[test]
    fn ok_and_warnings_empty_when_no_drift() {
        let diff = ProductDiff::default();
        assert!(diff.ok());
        assert!(diff.warnings().is_empty());
    }

    #[test]
    fn warnings_report_every_drift_class() {
        let diff = ProductDiff {
            dangling_base: true,
            base_mismatch: Some("name mismatch".to_owned()),
            missing_addons: vec!["b 1".to_owned(), "a 1".to_owned()],
            extra_addons: vec!["c 2".to_owned()],
            mismatched_addons: vec!["d (installed 1 != metadata 2)".to_owned()],
        };
        let warnings = diff.warnings();
        assert_eq!(warnings.len(), 5);
        assert!(warnings[0].contains("dangling"));
        assert!(warnings[1].contains("base product mismatch"));
        // missing_addons are sorted: "a 1" before "b 1".
        assert!(warnings[2].contains("a 1, b 1"));
        assert!(warnings[3].contains("installed but not in metadata"));
        assert!(warnings[4].contains("version mismatch"));
    }

    #[test]
    fn service_pack_parse_rejects_non_numeric_suffix() {
        // "15-SPfoo" is not a valid service pack; falls through to major-only
        // (no `.`), so the whole string becomes a textual major.
        assert_eq!(
            normalize_version("15-SPfoo"),
            Some(Version {
                major: VersionField::Text("15-SPfoo".to_owned()),
                minor: None,
            })
        );
    }

    #[test]
    fn non_numeric_major_kept_as_text() {
        assert_eq!(
            normalize_version("Leap.15"),
            Some(Version {
                major: VersionField::Text("Leap".to_owned()),
                minor: num(15),
            })
        );
    }
}
