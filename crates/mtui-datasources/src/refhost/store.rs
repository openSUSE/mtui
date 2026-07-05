//! The refhosts **search engine**, ported from
//! `mtui/hosts/refhost/store.py::Refhosts`.
//!
//! [`Refhosts`] holds the flattened, de-duplicated list of [`Host`] rows and
//! answers queries expressed as [`Attributes`]. Parsing of the `refhosts.yml`
//! document itself is reused from `mtui-types::load_refhosts` (the pure,
//! I/O-free loader that already handles location-flattening, dedup, and
//! per-row best-effort degradation); this module adds only the file read and
//! the matching logic.
//!
//! # Scope
//! This is the *search* surface (`search`, `is_candidate_match`,
//! `host_by_name`). The resolver chain
//! (`PathResolver`/`HttpsResolver`/`RefhostsFactory`) lives in
//! [`super::resolvers`]. The pool-claim engine (`query`/`slot_of`/`search_pool`)
//! from upstream `store.py` lands in a later Phase-3 task.

use std::path::Path;

use mtui_types::version::VersionField;
use mtui_types::{Addon, Host, Product, Version, load_refhosts};

use super::models::Attributes;
use crate::error::RefhostError;

/// Loads and searches a `refhosts.yml` database for hosts matching an
/// [`Attributes`] query.
#[derive(Debug, Clone)]
pub struct Refhosts {
    data: Vec<Host>,
}

impl Refhosts {
    /// Build a store directly from an already-parsed host list.
    ///
    /// Useful for tests and for resolver code (Phase 3) that has already
    /// obtained the rows via another path.
    #[must_use]
    pub fn from_hosts(data: Vec<Host>) -> Self {
        Self { data }
    }

    /// Load and parse a `refhosts.yml` file into a store.
    ///
    /// The document is flattened (legacy location groups merged), de-duplicated
    /// by host name, and malformed rows are dropped with a `warn` log — all via
    /// `mtui-types::load_refhosts`. A file-read failure or a document-level YAML
    /// parse failure surfaces as [`RefhostError`].
    ///
    /// # Errors
    /// Returns [`RefhostError::Io`] if `path` cannot be read, or
    /// [`RefhostError::Parse`] if the contents are not a valid `refhosts.yml`
    /// document.
    pub fn from_path(path: &Path) -> Result<Self, RefhostError> {
        let text = std::fs::read_to_string(path).map_err(|source| RefhostError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let data = load_refhosts(&text)?;
        Ok(Self { data })
    }

    /// The loaded host rows (flattened + de-duplicated).
    #[must_use]
    pub fn hosts(&self) -> &[Host] {
        &self.data
    }

    /// Return the names of hosts matching **any** of the given queries.
    ///
    /// Mirrors upstream `search`: results are accumulated per query in order
    /// (so a host matching two queries appears twice, as upstream does).
    #[must_use]
    pub fn search(&self, attributes: &[Attributes]) -> Vec<String> {
        let mut results = Vec::new();
        for attribute in attributes {
            for candidate in &self.data {
                if Self::is_candidate_match(candidate, attribute) {
                    results.push(candidate.name.clone());
                }
            }
        }
        results
    }

    /// Return `true` iff `candidate` satisfies every **set** field of `attribute`.
    ///
    /// Unset fields (empty `arch`, `product == None`, empty `addons`) impose no
    /// constraint, so an empty [`Attributes`] matches every host.
    #[must_use]
    pub fn is_candidate_match(candidate: &Host, attribute: &Attributes) -> bool {
        if !attribute.arch.is_empty() && attribute.arch != candidate.arch {
            return false;
        }
        if let Some(query) = &attribute.product
            && !Self::product_satisfied(candidate, query)
        {
            return false;
        }
        if !attribute.addons.is_empty() && !Self::addons_match(&candidate.addons, &attribute.addons)
        {
            return false;
        }
        true
    }

    /// Return the refhosts row whose `name` matches, or `None`.
    #[must_use]
    pub fn host_by_name(&self, name: &str) -> Option<&Host> {
        self.data.iter().find(|c| c.name == name)
    }

    /// A `base=<name>` query is satisfied when the host's base product matches
    /// **or** when the host carries that product as an addon.
    ///
    /// Extension products (`SLES-LTSS`, `sle-ha`, `SLES_SAP`, `SLE_RT`, …) ship
    /// on a `SLES`/`SLED` base and are recorded as addons; a host can only have
    /// one base, so `base=SLES-LTSS` must still resolve to a `SLES` host that
    /// has the `SLES-LTSS` extension installed.
    fn product_satisfied(candidate: &Host, query: &Product) -> bool {
        if Self::product_matches(&candidate.product, query) {
            return true;
        }
        candidate.addons.iter().any(|addon| {
            addon.name == query.name
                && match &query.version {
                    None => true,
                    Some(qv) => Self::version_matches(addon.version.as_ref(), qv),
                }
        })
    }

    /// Return `true` iff `candidate` has the queried name and version.
    fn product_matches(candidate: &Product, query: &Product) -> bool {
        if query.name != candidate.name {
            return false;
        }
        match &query.version {
            None => true,
            Some(qv) => Self::version_matches(candidate.version.as_ref(), qv),
        }
    }

    /// Match a candidate version against a query version.
    ///
    /// `query.minor == Text("")` is the "candidate must NOT have a minor"
    /// sentinel; `query.minor == None` means "ignore minor".
    fn version_matches(candidate: Option<&Version>, query: &Version) -> bool {
        let Some(candidate) = candidate else {
            return false;
        };
        if query.major != candidate.major {
            return false;
        }
        match &query.minor {
            // Sentinel: candidate must have no minor.
            Some(VersionField::Text(s)) if s.is_empty() => candidate.minor.is_none(),
            // Ignore minor.
            None => true,
            // Exact minor match.
            Some(qv) => candidate.minor.as_ref() == Some(qv),
        }
    }

    /// Return `true` iff every queried addon is present on the candidate.
    ///
    /// An addon query with no version matches any version of that addon.
    fn addons_match(candidate_addons: &[Addon], query_addons: &[Addon]) -> bool {
        for query in query_addons {
            let Some(candidate) = candidate_addons.iter().find(|a| a.name == query.name) else {
                return false;
            };
            if let Some(qv) = &query.version
                && !Self::version_matches(candidate.version.as_ref(), qv)
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(major: u64, minor: Option<VersionField>) -> Version {
        Version {
            major: VersionField::Num(major),
            minor,
        }
    }

    fn host(name: &str, arch: &str, product: Product, addons: Vec<Addon>) -> Host {
        Host {
            name: name.to_owned(),
            arch: arch.to_owned(),
            product,
            addons,
        }
    }

    fn sles(major: u64, minor: Option<VersionField>) -> Product {
        Product {
            name: "sles".to_owned(),
            version: Some(ver(major, minor)),
        }
    }

    /// A small in-memory pool mirroring the shape of the golden fixture.
    fn pool() -> Refhosts {
        Refhosts::from_hosts(vec![
            host(
                "host-default-x86",
                "x86_64",
                sles(15, Some(VersionField::Num(5))),
                vec![Addon {
                    name: "sdk".to_owned(),
                    version: Some(ver(15, Some(VersionField::Num(5)))),
                }],
            ),
            host(
                "host-nbg-x86",
                "x86_64",
                sles(15, Some(VersionField::Num(5))),
                vec![],
            ),
            host(
                "host-default-noaddon",
                "x86_64",
                sles(12, Some(VersionField::Text("sp4".to_owned()))),
                vec![],
            ),
            host(
                "host-nbg-only-here",
                "ppc64le",
                sles(15, Some(VersionField::Num(5))),
                vec![],
            ),
        ])
    }

    fn h() -> Host {
        host("h", "x86_64", sles(15, Some(VersionField::Num(5))), vec![])
    }

    // --- search / host_by_name ---

    #[test]
    fn search_finds_hosts() {
        let attrs = Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
        let found: std::collections::BTreeSet<_> = pool().search(&attrs).into_iter().collect();
        assert_eq!(
            found,
            ["host-default-x86", "host-nbg-x86"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[test]
    fn search_finds_host_with_string_minor() {
        let attrs = Attributes::from_testplatform("base=sles(major=12,minor=sp4);arch=[x86_64]");
        assert_eq!(pool().search(&attrs), ["host-default-noaddon"]);
    }

    #[test]
    fn search_empty_when_no_match() {
        let attrs = Attributes::from_testplatform("base=sles(major=99,minor=99);arch=[mips]");
        assert!(pool().search(&attrs).is_empty());
    }

    #[test]
    fn search_addon_filter_excludes_hosts_missing_addon() {
        let attrs = Attributes::from_testplatform(
            "base=sles(major=15,minor=5);arch=[x86_64];addon=sdk(major=15,minor=5)",
        );
        assert_eq!(pool().search(&attrs), ["host-default-x86"]);
    }

    #[test]
    fn host_by_name_finds_and_misses() {
        let p = pool();
        assert_eq!(
            p.host_by_name("host-nbg-only-here").unwrap().arch,
            "ppc64le"
        );
        assert!(p.host_by_name("no-such-host").is_none());
    }

    // --- is_candidate_match branches ---

    #[test]
    fn unset_attribute_matches_everything() {
        assert!(Refhosts::is_candidate_match(&h(), &Attributes::default()));
    }

    #[test]
    fn arch_mismatch_and_match() {
        let attr = Attributes {
            arch: "aarch64".to_owned(),
            ..Default::default()
        };
        assert!(!Refhosts::is_candidate_match(&h(), &attr));
        let attr = Attributes {
            arch: "x86_64".to_owned(),
            ..Default::default()
        };
        assert!(Refhosts::is_candidate_match(&h(), &attr));
    }

    #[test]
    fn empty_minor_sentinel_excludes_candidate_with_minor() {
        let attr = Attributes {
            product: Some(Product {
                name: "sles".to_owned(),
                version: Some(ver(15, Some(VersionField::Text(String::new())))),
            }),
            ..Default::default()
        };
        assert!(!Refhosts::is_candidate_match(&h(), &attr));
    }

    #[test]
    fn empty_minor_sentinel_matches_candidate_without_minor() {
        let attr = Attributes {
            product: Some(Product {
                name: "sles".to_owned(),
                version: Some(ver(15, Some(VersionField::Text(String::new())))),
            }),
            ..Default::default()
        };
        let candidate = host("h", "x86_64", sles(15, None), vec![]);
        assert!(Refhosts::is_candidate_match(&candidate, &attr));
    }

    #[test]
    fn minor_and_major_mismatch_return_false() {
        let attr = Attributes {
            product: Some(sles(15, Some(VersionField::Num(5)))),
            ..Default::default()
        };
        let candidate = host("h", "x86_64", sles(15, Some(VersionField::Num(4))), vec![]);
        assert!(!Refhosts::is_candidate_match(&candidate, &attr));

        let attr = Attributes {
            product: Some(sles(15, None)),
            ..Default::default()
        };
        let candidate = host("h", "x86_64", sles(12, None), vec![]);
        assert!(!Refhosts::is_candidate_match(&candidate, &attr));
    }

    #[test]
    fn addon_missing_and_version_mismatch_return_false() {
        let attr = Attributes {
            addons: vec![Addon {
                name: "sdk".to_owned(),
                version: Some(ver(15, None)),
            }],
            ..Default::default()
        };
        assert!(!Refhosts::is_candidate_match(&h(), &attr)); // no addons on candidate

        let attr = Attributes {
            addons: vec![Addon {
                name: "sdk".to_owned(),
                version: Some(ver(15, Some(VersionField::Num(5)))),
            }],
            ..Default::default()
        };
        let candidate = host(
            "h",
            "x86_64",
            sles(15, Some(VersionField::Num(5))),
            vec![Addon {
                name: "sdk".to_owned(),
                version: Some(ver(15, Some(VersionField::Num(4)))),
            }],
        );
        assert!(!Refhosts::is_candidate_match(&candidate, &attr));
    }

    #[test]
    fn addon_name_only_matches_any_version() {
        let attr = Attributes {
            addons: vec![Addon {
                name: "sdk".to_owned(),
                version: None,
            }],
            ..Default::default()
        };
        let candidate = host(
            "h",
            "x86_64",
            sles(15, Some(VersionField::Num(5))),
            vec![Addon {
                name: "sdk".to_owned(),
                version: Some(ver(99, None)),
            }],
        );
        assert!(Refhosts::is_candidate_match(&candidate, &attr));
    }

    // --- base=<extension> carried as addon ---

    fn ltss_host(base_minor: &str, ltss_minor: &str, with_ltss: bool) -> Host {
        let addons = if with_ltss {
            vec![Addon {
                name: "SLES-LTSS".to_owned(),
                version: Some(ver(15, Some(VersionField::Text(ltss_minor.to_owned())))),
            }]
        } else {
            vec![]
        };
        host(
            "ltss-x86",
            "x86_64",
            Product {
                name: "SLES".to_owned(),
                version: Some(ver(15, Some(VersionField::Text(base_minor.to_owned())))),
            },
            addons,
        )
    }

    fn attr1(tp: &str) -> Attributes {
        Attributes::from_testplatform(tp)
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn base_extension_matches_host_with_addon() {
        let attr = attr1("base=SLES-LTSS(major=15,minor=SP6);arch=[x86_64]");
        assert!(Refhosts::is_candidate_match(
            &ltss_host("SP6", "SP6", true),
            &attr
        ));
    }

    #[test]
    fn base_extension_no_match_when_addon_absent() {
        let attr = attr1("base=SLES-LTSS(major=15,minor=SP6);arch=[x86_64]");
        assert!(!Refhosts::is_candidate_match(
            &ltss_host("SP6", "SP6", false),
            &attr
        ));
    }

    #[test]
    fn base_extension_version_must_match_addon() {
        let attr = attr1("base=SLES-LTSS(major=15,minor=SP6);arch=[x86_64]");
        assert!(!Refhosts::is_candidate_match(
            &ltss_host("SP6", "SP5", true),
            &attr
        ));
    }

    #[test]
    fn base_still_matches_real_base_product() {
        let attr = attr1("base=SLES(major=15,minor=SP6);arch=[x86_64]");
        assert!(Refhosts::is_candidate_match(
            &ltss_host("SP6", "SP6", true),
            &attr
        ));
    }
}
