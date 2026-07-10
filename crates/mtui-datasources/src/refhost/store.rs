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
//! `host_by_name`) plus the ad-hoc [`query`](Refhosts::query) filter and the
//! [`slot_of`](Refhosts::slot_of) test-target key plus the pool-claim helper
//! [`search_pool_by_query`](Refhosts::search_pool_by_query) that `list_refhosts`
//! and the host-arbitration pool consume. The resolver chain
//! (`PathResolver`/`HttpsResolver`/`RefhostsFactory`) lives in
//! [`super::resolvers`].

use std::path::Path;

use mtui_types::version::VersionField;
use mtui_types::{Addon, Host, Product, Version, load_refhosts};

/// A test-target slot key: `(product, version, arch, sorted addon names)`.
///
/// The full identity an update distinguishes (upstream `slot_of`'s 4-tuple).
pub type Slot = (String, String, String, Vec<String>);

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

    /// Return refhosts matching the filters, de-duplicated by host name.
    ///
    /// Ports upstream `Refhosts.query`. `attributes` (parsed from a
    /// `testplatform`) and the ad-hoc field filters (`name` glob, `arch` list,
    /// `product` substring, `version` loose, `addon` substring list) are
    /// alternatives — when `attributes` is `Some` the field filters are ignored.
    /// With neither, every host is returned. A host matches an `attributes`
    /// query when it satisfies **any** of the alternatives (upstream `any`).
    ///
    /// The loaded `data` is already de-duplicated by name at load time
    /// (`mtui-types::load_refhosts`), so the extra dedup upstream performs here
    /// is a no-op guard kept for byte-parity with the upstream contract.
    #[must_use]
    pub fn query(
        &self,
        attributes: Option<&[Attributes]>,
        name: Option<&str>,
        arch: &[String],
        product: Option<&str>,
        version: Option<&str>,
        addon: &[String],
    ) -> Vec<Host> {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut out: Vec<Host> = Vec::new();
        for host in &self.data {
            if !seen.insert(host.name.as_str()) {
                continue;
            }
            let keep = match attributes {
                Some(attrs) => attrs.iter().any(|a| Self::is_candidate_match(host, a)),
                None => Self::field_match(host, name, arch, product, version, addon),
            };
            if keep {
                out.push(host.clone());
            }
        }
        out
    }

    /// Return `true` iff `host` satisfies every supplied ad-hoc field filter.
    ///
    /// An unset filter (empty/`None`) imposes no constraint. Ports upstream
    /// `_field_match`: `name` is a shell glob, `arch` is membership in a list,
    /// `product` is a case-insensitive substring of the base product name,
    /// `version` is the loose form matched by [`version_str_match`], and each
    /// `addon` term is a case-insensitive substring of some installed addon.
    #[must_use]
    fn field_match(
        host: &Host,
        name: Option<&str>,
        arch: &[String],
        product: Option<&str>,
        version: Option<&str>,
        addon: &[String],
    ) -> bool {
        if let Some(name) = name.filter(|n| !n.is_empty())
            && !name_glob(&host.name, name)
        {
            return false;
        }
        if !arch.is_empty() && !arch.iter().any(|a| a == &host.arch) {
            return false;
        }
        if let Some(product) = product.filter(|p| !p.is_empty())
            && !host
                .product
                .name
                .to_lowercase()
                .contains(&product.to_lowercase())
        {
            return false;
        }
        if let Some(version) = version.filter(|v| !v.is_empty())
            && !Self::version_str_match(host.product.version.as_ref(), version)
        {
            return false;
        }
        if !addon.is_empty() {
            let have: Vec<String> = host.addons.iter().map(|a| a.name.to_lowercase()).collect();
            let all = addon
                .iter()
                .all(|want| have.iter().any(|n| n.contains(&want.to_lowercase())));
            if !all {
                return false;
            }
        }
        true
    }

    /// Loosely match a host version against `15-SP6` / `15.6` / `15`.
    ///
    /// Ports upstream `_version_str_match`: `SP` is optional and
    /// case-insensitive; a bare major matches any minor. A host with no version
    /// never matches a versioned query.
    #[must_use]
    fn version_str_match(hostver: Option<&Version>, want: &str) -> bool {
        let Some(hostver) = hostver else {
            return false;
        };
        let want = want.replace('.', "-").to_lowercase();
        let mut parts = want.splitn(2, '-');
        let major = parts.next().unwrap_or_default();
        if hostver.major.to_string().to_lowercase() != major {
            return false;
        }
        match parts.next() {
            Some(minor) if !minor.is_empty() => {
                let host_minor = hostver
                    .minor
                    .as_ref()
                    .map(|m| m.to_string().to_lowercase())
                    .unwrap_or_default();
                host_minor.replace("sp", "") == minor.replace("sp", "")
            }
            _ => true,
        }
    }

    /// Return the test-target [`Slot`] key for `host`.
    ///
    /// Ports upstream `slot_of`: the full `(product, version, arch, addons)` an
    /// update distinguishes, so only genuine duplicates collapse to one slot.
    /// Addon names are sorted for a stable key.
    #[must_use]
    pub fn slot_of(host: &Host) -> Slot {
        let ver_str = version_display(host.product.version.as_ref());
        let mut addons: Vec<String> = host.addons.iter().map(|a| a.name.clone()).collect();
        addons.sort();
        (
            host.product.name.clone(),
            ver_str,
            host.arch.clone(),
            addons,
        )
    }

    /// Return the test-target [`Slot`] key derived from the **queried**
    /// attributes (upstream `slot_for_query`).
    ///
    /// Unlike [`slot_of`](Self::slot_of) — which keys on every module a host
    /// happens to have installed — this keys on what the testplatform actually
    /// distinguishes: the base product + version it requests, the host's arch
    /// (testplatforms fan out one query per arch), and only the addons the
    /// testplatform explicitly asked for. Hosts that satisfy the same query are
    /// interchangeable for that update and collapse to one slot, so the arbiter
    /// draws a single host per `(product, version, arch, requested addons)`.
    #[must_use]
    pub fn slot_for_query(attribute: &Attributes, host: &Host) -> Slot {
        let (name, ver_str) = match &attribute.product {
            None => (String::new(), String::new()),
            Some(product) => (
                product.name.clone(),
                version_display(product.version.as_ref()),
            ),
        };
        let mut addons: Vec<String> = attribute.addons.iter().map(|a| a.name.clone()).collect();
        addons.sort();
        (name, ver_str, host.arch.clone(), addons)
    }

    /// Return pool candidates `(host, slot)` keyed on the query slot
    /// (upstream `search_pool_by_query`).
    ///
    /// Each host matching any of `attributes` is returned once, tagged with the
    /// [`slot_for_query`](Self::slot_for_query) of the **first** attribute it
    /// matches — the testplatform's requested identity rather than the host's
    /// full installed-module identity — so host-arbitration draws one host per
    /// *requested* test-target slot.
    #[must_use]
    pub fn search_pool_by_query(&self, attributes: &[Attributes]) -> Vec<(Host, Slot)> {
        let mut out: Vec<(Host, Slot)> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for host in &self.data {
            if !seen.insert(host.name.as_str()) {
                continue;
            }
            for attribute in attributes {
                if Self::is_candidate_match(host, attribute) {
                    out.push((host.clone(), Self::slot_for_query(attribute, host)));
                    break;
                }
            }
        }
        out
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

/// Render a [`Version`] as upstream `slot_of` / `_ver_str` does.
///
/// `None` → empty; a missing/empty minor → bare major; else `major-minor`.
fn version_display(ver: Option<&Version>) -> String {
    match ver {
        None => String::new(),
        Some(v) => match &v.minor {
            None => v.major.to_string(),
            Some(VersionField::Text(s)) if s.is_empty() => v.major.to_string(),
            Some(minor) => format!("{}-{minor}", v.major),
        },
    }
}

/// Match `text` against a shell glob supporting `*` and `?` (upstream uses
/// Python's `fnmatch` for the `--name` filter).
///
/// `*` matches any run (including empty), `?` matches exactly one character;
/// every other character is literal. Matching is case-sensitive, mirroring
/// `fnmatch.fnmatch` on a case-sensitive filesystem for the hostnames mtui
/// deals with. Implemented inline to avoid a glob dependency (no-runtime-deps
/// goal).
fn name_glob(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    // Classic two-pointer wildcard match with backtracking on `*`.
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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

    // --- query -------------------------------------------------------------

    fn names(hosts: &[Host]) -> Vec<String> {
        hosts.iter().map(|h| h.name.clone()).collect()
    }

    #[test]
    fn query_no_filters_returns_all_deduped() {
        let got = names(&pool().query(None, None, &[], None, None, &[]));
        assert_eq!(
            got,
            [
                "host-default-x86",
                "host-nbg-x86",
                "host-default-noaddon",
                "host-nbg-only-here",
            ]
        );
    }

    #[test]
    fn query_by_attributes_matches_any() {
        let attrs = Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
        let got = names(&pool().query(Some(&attrs), None, &[], None, None, &[]));
        assert_eq!(got, ["host-default-x86", "host-nbg-x86"]);
    }

    #[test]
    fn query_name_glob_and_arch_filter() {
        // name glob
        let got = names(&pool().query(None, Some("host-nbg-*"), &[], None, None, &[]));
        assert_eq!(got, ["host-nbg-x86", "host-nbg-only-here"]);
        // arch list membership
        let got = names(&pool().query(None, None, &["ppc64le".to_owned()], None, None, &[]));
        assert_eq!(got, ["host-nbg-only-here"]);
    }

    #[test]
    fn query_product_substring_and_version_loose() {
        // product substring is case-insensitive
        let got = names(&pool().query(None, None, &[], Some("SLE"), None, &[]));
        assert_eq!(got.len(), 4);
        // loose version: "15" matches any 15.x minor
        let got = names(&pool().query(None, None, &[], None, Some("15"), &[]));
        assert_eq!(
            got,
            ["host-default-x86", "host-nbg-x86", "host-nbg-only-here"]
        );
        // loose version with SP-optional minor
        let got = names(&pool().query(None, None, &[], None, Some("15-SP5"), &[]));
        assert_eq!(
            got,
            ["host-default-x86", "host-nbg-x86", "host-nbg-only-here"]
        );
        // dotted form
        let got = names(&pool().query(None, None, &[], None, Some("12.sp4"), &[]));
        assert_eq!(got, ["host-default-noaddon"]);
    }

    #[test]
    fn query_addon_substring_requires_all() {
        let got = names(&pool().query(None, None, &[], None, None, &["sdk".to_owned()]));
        assert_eq!(got, ["host-default-x86"]);
        // a term that matches nothing excludes every host
        let got = names(&pool().query(None, None, &[], None, None, &["nope".to_owned()]));
        assert!(got.is_empty());
    }

    #[test]
    fn version_str_match_forms() {
        let v = ver(15, Some(VersionField::Num(6)));
        assert!(Refhosts::version_str_match(Some(&v), "15"));
        assert!(Refhosts::version_str_match(Some(&v), "15-SP6"));
        assert!(Refhosts::version_str_match(Some(&v), "15.6"));
        assert!(!Refhosts::version_str_match(Some(&v), "15-SP5"));
        assert!(!Refhosts::version_str_match(Some(&v), "12"));
        assert!(!Refhosts::version_str_match(None, "15"));
    }

    // --- slot_of -----------------------------------------------------------

    #[test]
    fn slot_of_formats_version_and_sorts_addons() {
        let h = host(
            "h",
            "x86_64",
            sles(15, Some(VersionField::Num(6))),
            vec![
                Addon {
                    name: "sle-module-web".to_owned(),
                    version: None,
                },
                Addon {
                    name: "sdk".to_owned(),
                    version: None,
                },
            ],
        );
        assert_eq!(
            Refhosts::slot_of(&h),
            (
                "sles".to_owned(),
                "15-6".to_owned(),
                "x86_64".to_owned(),
                vec!["sdk".to_owned(), "sle-module-web".to_owned()],
            )
        );
    }

    #[test]
    fn slot_of_bare_major_when_no_minor() {
        let h = host("h", "aarch64", sles(15, None), vec![]);
        assert_eq!(
            Refhosts::slot_of(&h),
            (
                "sles".to_owned(),
                "15".to_owned(),
                "aarch64".to_owned(),
                Vec::new(),
            )
        );
    }

    // --- slot_for_query / search_pool_by_query -----------------------------

    #[test]
    fn slot_for_query_keys_on_requested_not_installed_addons() {
        // A host with an extra installed addon (sdk) queried by a testplatform
        // that requests no addons: the query slot must ignore the sdk.
        let h = host(
            "h",
            "x86_64",
            sles(15, Some(VersionField::Num(5))),
            vec![Addon {
                name: "sdk".to_owned(),
                version: None,
            }],
        );
        let attrs = Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
        assert_eq!(
            Refhosts::slot_for_query(&attrs[0], &h),
            (
                "sles".to_owned(),
                "15-5".to_owned(),
                "x86_64".to_owned(),
                Vec::new(),
            )
        );
    }

    #[test]
    fn slot_for_query_uses_host_arch_and_requested_addons() {
        let h = host("h", "aarch64", sles(15, Some(VersionField::Num(6))), vec![]);
        let attrs = Attributes::from_testplatform(
            "base=sles(major=15,minor=6);arch=[aarch64];addon=sdk(major=15,minor=6)",
        );
        let slot = Refhosts::slot_for_query(&attrs[0], &h);
        assert_eq!(slot.2, "aarch64");
        assert_eq!(slot.3, vec!["sdk".to_owned()]);
    }

    #[test]
    fn search_pool_by_query_collapses_interchangeable_hosts_to_one_slot() {
        // Both x86_64 SLES15-SP5 hosts (one with an sdk addon, one without)
        // satisfy the same addon-less query and must share a single query slot,
        // so the arbiter draws just one of them.
        let attrs = Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
        let pairs = pool().search_pool_by_query(&attrs);
        let hosts: std::collections::BTreeSet<_> =
            pairs.iter().map(|(h, _)| h.name.clone()).collect();
        assert_eq!(
            hosts,
            ["host-default-x86", "host-nbg-x86"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        let slots: std::collections::BTreeSet<_> = pairs.iter().map(|(_, s)| s.clone()).collect();
        assert_eq!(slots.len(), 1, "interchangeable hosts must share one slot");
    }

    #[test]
    fn search_pool_by_query_distinct_arches_are_distinct_slots() {
        // x86_64 and ppc64le SLES15-SP5 hosts land in different slots.
        let attrs =
            Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64,ppc64le]");
        let pairs = pool().search_pool_by_query(&attrs);
        let slots: std::collections::BTreeSet<_> = pairs.iter().map(|(_, s)| s.clone()).collect();
        // x86_64 slot (2 interchangeable hosts) + ppc64le slot = 2 slots.
        assert_eq!(slots.len(), 2);
    }

    #[test]
    fn search_pool_by_query_dedups_host_across_attributes() {
        // A host matching two overlapping attributes appears once (upstream's
        // `seen` set), tagged with the first attribute it matches.
        let mut attrs = Attributes::from_testplatform("base=sles(major=15,minor=5);arch=[x86_64]");
        // A second, equivalent query the same hosts also satisfy.
        attrs.extend(Attributes::from_testplatform(
            "base=sles(major=15,minor=5);arch=[x86_64]",
        ));
        assert_eq!(attrs.len(), 2, "test needs two overlapping attributes");
        let pairs = pool().search_pool_by_query(&attrs);
        let names: Vec<_> = pairs.iter().map(|(h, _)| h.name.clone()).collect();
        let unique: std::collections::HashSet<_> = names.iter().cloned().collect();
        assert_eq!(names.len(), unique.len(), "no host appears twice");
    }

    // --- name_glob ---------------------------------------------------------

    #[test]
    fn name_glob_wildcards() {
        assert!(name_glob("whale-01.qam.suse.cz", "whale-*"));
        assert!(name_glob("whale-01.qam.suse.cz", "*.qam.suse.cz"));
        assert!(name_glob("whale-01", "whale-??"));
        assert!(name_glob("abc", "*"));
        assert!(name_glob("abc", "a?c"));
        assert!(!name_glob("whale-01", "whale-???"));
        assert!(!name_glob("whale-01.qam.suse.cz", "*.suse.de"));
        assert!(name_glob("exact", "exact"));
        assert!(!name_glob("exact", "exac"));
    }
}
