//! Maps a v2 [`ReportDocument`] onto a [`TestReportBase`].
//!
//! Pure and I/O-free: one entry point, [`apply_document`], plus a small pure
//! helper per non-trivial mapping rule; every rule transcribes the document
//! schema's fields rather than inventing new derivations.

use std::collections::{BTreeSet, HashMap, HashSet};

use mtui_types::report_document::{
    DocWorkflow, DocumentKind, Issues, ReportDocument, Target, TestPlatform,
};
use mtui_types::sanitize::{sanitize_external, sanitize_log};
use mtui_types::{RepoUrl, RepoUrlParseError, RequestReviewID, SystemProduct};

use crate::products::{normalize, normalize_16};
use crate::reports::repoparse::log_invalid_url;
use crate::testreport::{HostPackages, SlackReviewMarker, TestReportBase, is_higher_version};

/// Whether `composed`/`repositories` are populated for `doc`.
///
/// **Invariant**: the document ships `install.targets[]` (and therefore
/// binaries and repo URLs) for *classic maintenance* too. Populating them there
/// would engage the `composed` narrowing on a path that exists to prevent
/// `zypper 104`, and for `SlReport` would flip the load-bearing `repositories`
/// short-circuit. Extending the composition to classic maintenance is a
/// separate change; do not switch this on to "complete" the mapping.
#[must_use]
fn should_compose(doc: &ReportDocument) -> bool {
    matches!(doc.workflow, DocWorkflow::Gitea) || matches!(doc.kind, DocumentKind::Pi)
}

/// Refuses a document no report class can drive: every SLFO update is a Gitea
/// pull request, so `kind: slfo` with `workflow: obs` is an error.
pub(crate) fn check_document(doc: &ReportDocument) -> Result<(), String> {
    if doc.kind == DocumentKind::Slfo && doc.workflow == DocWorkflow::Obs {
        return Err(format!(
            "{} is served through the OBS workflow, but SLFO updates are Gitea pull requests only",
            sanitize_external(&doc.id, None).replace(['\n', '\t'], " ")
        ));
    }
    Ok(())
}

/// Applies `doc` onto `base`, filling every field the document maps to.
/// Downstream derivations (`update_repos_parser()`, `packages_for_host`,
/// `register`/`normalize*`, `check_hash`, `composition()`) are untouched.
pub fn apply_document(base: &mut TestReportBase, doc: &ReportDocument) {
    base.rrid = RequestReviewID::parse(&doc.id).ok();
    if base.rrid.is_none() {
        tracing::warn!(id = %sanitize_log(&doc.id, None), "report document: id did not parse as an RRID");
    }
    base.realid = Some(doc.id.clone());

    base.packager = doc.update.packager.clone();
    base.rating = doc.update.rating.clone();
    base.category = doc.update.category.clone().unwrap_or_default();
    base.repository = doc.install.repository.clone();

    base.products = product_lines(&doc.update.products);
    base.testplatforms = testplatform_lines(&doc.install.test_platforms);
    base.packages = packages_map(doc);
    base.host_packages = std::sync::Arc::new(host_packages(doc));

    if should_compose(doc) {
        base.repositories = repositories_set(&doc.install.targets);
        base.composed = composed_index(doc);
    } else {
        base.repositories = HashSet::new();
        base.composed = HashMap::new();
    }

    let (bugs, jira) = issue_maps(&doc.issues);
    base.bugs = bugs;
    base.jira = jira;

    let (giteapr, giteaprapi, giteacohash) = origin_fields(doc);
    base.giteapr = giteapr;
    base.giteaprapi = giteaprapi;
    base.giteacohash = giteacohash;
    base.update_source = doc.workflow.into();

    base.hostnames = refhosts(doc);

    base.slack_review = doc
        .people
        .reviewer
        .slack
        .as_ref()
        .map(|s| SlackReviewMarker {
            channel: s.channel.clone(),
            ts: s.ts.clone(),
        });
    base.reviewer = doc
        .people
        .reviewer
        .name
        .clone()
        .into_inner()
        .unwrap_or_default();
}

/// `update.products[]` -> `"{name} {version} ({archs sorted, ', '-joined})"`
/// — the exact form `95_ExportMetadata.pm:29-34`'s `parse_product` expects.
fn product_lines(products: &[mtui_types::report_document::Product]) -> Vec<String> {
    products
        .iter()
        .map(|p| {
            let mut archs = p.archs.clone();
            archs.sort();
            format!("{} {} ({})", p.name, p.version, archs.join(", "))
        })
        .collect()
}

/// `install.test_platforms[]` ->
/// `base={class}(major={major},minor={minor});arch=[{archs sorted, comma-joined}]`,
/// with an optional `;addon={class}(major=…,minor=…)`.
///
/// The arch list is `tp.archs` verbatim (sorted): that it holds the *addon's*
/// archs when an addon is present, else the base's, is baked into the document
/// by the generator (`95_ExportMetadata.pm:72-75`).
#[must_use]
pub fn testplatform_lines(platforms: &[TestPlatform]) -> Vec<String> {
    platforms
        .iter()
        .map(|tp| {
            let mut archs = tp.archs.clone();
            archs.sort();
            let mut line = format!(
                "base={}(major={},minor={});arch=[{}]",
                tp.base.class,
                tp.base.major,
                tp.base.minor,
                archs.join(",")
            );
            if let Some(addon) = &tp.addon {
                line.push_str(&format!(
                    ";addon={}(major={},minor={})",
                    addon.class, addon.major, addon.minor
                ));
            }
            line
        })
        .collect()
}

/// `install.targets[]` -> `product -> { package name -> version }`.
///
/// Key is `"standard"` for `kind ∈ {slfo, pi}` (mirrors
/// `21_FetchOBSPackages.pm:243,413`'s hardcode), else `target.version`. Value
/// is `binaries[name]` with the final dot-segment (the arch) stripped, as the
/// schema pins `version-release.arch`; this matters for arches outside
/// `RPMVersion::parse`'s own strip list.
fn packages_map(doc: &ReportDocument) -> HashMap<String, HashMap<String, String>> {
    let standard_key = product_agnostic(doc);
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    for target in &doc.install.targets {
        let key = if standard_key {
            "standard".to_owned()
        } else {
            target.version.clone()
        };
        let bucket = out.entry(key).or_default();
        for (name, version_release_arch) in &target.binaries {
            bucket.insert(name.clone(), strip_arch_suffix(version_release_arch));
        }
    }
    out
}

/// Whether every host tracks one package set: SLFO/PI packages are
/// product-agnostic.
fn product_agnostic(doc: &ReportDocument) -> bool {
    matches!(doc.kind, DocumentKind::Slfo | DocumentKind::Pi)
}

/// Each target with its update-repository key and URL check, the one rule
/// [`obs_update_repos`] and [`host_packages`] share so that no host tracks a
/// package it gets no repository for.
fn obs_targets(
    doc: &ReportDocument,
) -> impl Iterator<Item = (SystemProduct, &Target, Result<RepoUrl, RepoUrlParseError>)> {
    doc.install.targets.iter().map(|t| {
        let product = normalize(SystemProduct::new(&t.product, &t.version, &t.arch));
        (product, t, RepoUrl::parse(&t.repository))
    })
}

/// The `"standard"` set for SLFO/PI, else each target's packages under its
/// update-repository key, the higher version winning within a key.
fn host_packages(doc: &ReportDocument) -> HostPackages {
    if product_agnostic(doc) {
        return HostPackages::Shared(packages_map(doc).remove("standard").unwrap_or_default());
    }
    let mut out: HashMap<SystemProduct, HashMap<String, String>> = HashMap::new();
    for (product, target, url) in obs_targets(doc) {
        if url.is_err() {
            continue;
        }
        let bucket = out.entry(product).or_default();
        for (name, version_release_arch) in &target.binaries {
            let ver = strip_arch_suffix(version_release_arch);
            if bucket
                .get(name)
                .is_none_or(|kept| is_higher_version(&ver, kept))
            {
                bucket.insert(name.clone(), ver);
            }
        }
    }
    HostPackages::PerProduct(out)
}

/// Strips the final dot-segment of `version-release.arch`; unchanged if there
/// is no dot (every schema-valid value has one).
fn strip_arch_suffix(version_release_arch: &str) -> String {
    version_release_arch
        .rsplit_once('.')
        .map_or_else(|| version_release_arch.to_owned(), |(v, _)| v.to_owned())
}

/// `install.targets[].repository`, unconditionally — [`apply_document`] gates
/// the call with [`should_compose`].
fn repositories_set(targets: &[Target]) -> HashSet<String> {
    targets.iter().map(|t| t.repository.clone()).collect()
}

/// `install.targets[]` -> `SystemProduct -> update repository URL`: the update
/// repos of the classic OBS workflow. A target whose URL fails `RepoUrl`
/// validation is dropped. Independent of `should_compose`: this feeds
/// `update_repos` only.
#[must_use]
pub fn obs_update_repos(doc: &ReportDocument) -> HashMap<SystemProduct, String> {
    obs_targets(doc)
        .filter_map(|(product, target, url)| match url {
            Ok(_) => Some((product, target.repository.clone())),
            Err(e) => {
                log_invalid_url(&target.repository, &e);
                None
            }
        })
        .collect()
}

/// Records `names` for `raw` under one key per normalizer the repo parsers use
/// — raw (`gitrepoparse`), `normalize_16` (`reporepoparse`) and `normalize`
/// (the OBS update repos) — because the host reports whichever form its own
/// `/etc/products.d` carries. Where a normalizer's output collides with another
/// product's raw name the sets union, which is over-inclusive and so degrades
/// toward not narrowing.
///
/// An empty `names` still creates the keys: "composes nothing here" is
/// distinct from an absent key's "nothing is known here".
pub(crate) fn register(
    out: &mut HashMap<SystemProduct, BTreeSet<String>>,
    raw: SystemProduct,
    names: &BTreeSet<String>,
) {
    for form in BTreeSet::from([normalize_16(raw.clone()), normalize(raw.clone()), raw]) {
        out.entry(form).or_default().extend(names.iter().cloned());
    }
}

/// `install.targets[]` -> `SystemProduct -> the package names this update
/// composes for it`, via the existing three-normalizer [`register`] helper.
///
/// For each target, registers `binaries.keys()` under
/// `(product, version, arch)`. Then, for every `(product, version, arch)`
/// [`update.products[].archs`](mtui_types::report_document::Update) declares
/// with no matching target row, registers an **explicit empty set**, which makes
/// `narrow_to_composed` refuse the host by name instead of falling open.
/// Unconditional — [`apply_document`] gates the call with [`should_compose`].
fn composed_index(doc: &ReportDocument) -> HashMap<SystemProduct, BTreeSet<String>> {
    let mut out: HashMap<SystemProduct, BTreeSet<String>> = HashMap::new();

    for target in &doc.install.targets {
        let names: BTreeSet<String> = target.binaries.keys().cloned().collect();
        register(
            &mut out,
            SystemProduct::new(&target.product, &target.version, &target.arch),
            &names,
        );
    }

    let present: HashSet<(&str, &str, &str)> = doc
        .install
        .targets
        .iter()
        .map(|t| (t.product.as_str(), t.version.as_str(), t.arch.as_str()))
        .collect();
    for product in &doc.update.products {
        for arch in &product.archs {
            if !present.contains(&(
                product.name.as_str(),
                product.version.as_str(),
                arch.as_str(),
            )) {
                register(
                    &mut out,
                    SystemProduct::new(&product.name, &product.version, arch),
                    &BTreeSet::new(),
                );
            }
        }
    }
    out
}

/// `issues` -> `(bugs, jira)`, splitting each key on its tracker: `bsc#` /
/// `bnc#` / `boo#` go to `bugs`, `jsc#` to `jira`; the value is `issue.title`.
fn issue_maps(issues: &Issues) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut bugs = HashMap::new();
    let mut jira = HashMap::new();
    for (key, issue) in issues {
        let Some((tracker, id)) = key.as_str().split_once('#') else {
            continue;
        };
        match tracker {
            "bsc" | "bnc" | "boo" => {
                bugs.insert(id.to_owned(), issue.title.clone());
            }
            "jsc" => {
                jira.insert(id.to_owned(), issue.title.clone());
            }
            _ => {}
        }
    }
    (bugs, jira)
}

/// `update.origin.{pull_request,api,commit}`, only when `workflow == gitea`.
fn origin_fields(doc: &ReportDocument) -> (Option<String>, Option<String>, Option<String>) {
    if matches!(doc.workflow, DocWorkflow::Gitea) {
        (
            doc.update.origin.pull_request.clone(),
            doc.update.origin.api.clone(),
            doc.update.origin.commit.clone(),
        )
    } else {
        (None, None, None)
    }
}

/// `testing.install.checks[].refhost`; no live report carries
/// `testing.install.checks[]` yet.
fn refhosts(doc: &ReportDocument) -> HashSet<String> {
    doc.testing
        .install
        .as_ref()
        .map(|install| install.checks.iter().map(|c| c.refhost.clone()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::Config;
    use mtui_types::report_document::ReportDocument;

    fn doc(raw: &str) -> ReportDocument {
        raw.parse().expect("fixture parses as a ReportDocument")
    }

    fn base() -> TestReportBase {
        TestReportBase::new(Config::default())
    }

    const MAINTENANCE_OBS: &str = mtui_types::fixtures::MAINTENANCE_OBS;
    const MAINTENANCE_ADDON: &str = mtui_types::fixtures::MAINTENANCE_ADDON;
    const SLFO_GITEA: &str = mtui_types::fixtures::SLFO_GITEA;
    const MAXIMAL: &str = mtui_types::fixtures::MAXIMAL;
    const PI: &str = mtui_types::fixtures::PI;

    // --- product_lines ---

    #[test]
    fn product_lines_sorts_archs_and_joins_with_comma_space() {
        let d = doc(MAINTENANCE_OBS);
        assert_eq!(
            product_lines(&d.update.products),
            vec!["SLE-Product-SLES 15-SP6-LTSS (s390x)".to_owned()]
        );

        let d = doc(SLFO_GITEA);
        assert_eq!(
            product_lines(&d.update.products),
            vec!["SLES-SAP 16.0 (ppc64le, x86_64)".to_owned()]
        );
    }

    // --- testplatform_lines: the addon-archs quirk ---

    #[test]
    fn testplatform_lines_without_addon() {
        let d = doc(MAINTENANCE_OBS);
        assert_eq!(
            testplatform_lines(&d.install.test_platforms),
            vec!["base=SLES-LTSS(major=15,minor=SP6);arch=[s390x]".to_owned()]
        );
    }

    #[test]
    fn testplatform_lines_with_addon_appends_it_after_arch() {
        let d = doc(MAINTENANCE_ADDON);
        assert_eq!(
            testplatform_lines(&d.install.test_platforms),
            vec![
                "base=SLES(major=15,minor=SP7);arch=[x86_64];addon=sle-module-live-patching(major=15,minor=SP7)"
                    .to_owned()
            ]
        );
    }

    /// Mutation: using the *base's* archs when an addon is present would
    /// diverge here since `maximal.json`'s single platform declares both
    /// `x86_64` and `aarch64` on the (already addon-selected) `archs` field —
    /// swapping the source would still print `[aarch64,x86_64]` today only
    /// because this fixture's base and addon share the same arch list, so the
    /// real guard against that mutation lives in the equivalence test (step
    /// 9), not here. This test instead pins the sorted, comma-joined form.
    #[test]
    fn testplatform_lines_sorts_archs() {
        let d = doc(MAXIMAL);
        assert_eq!(
            testplatform_lines(&d.install.test_platforms),
            vec![
                "base=SLES(major=15,minor=SP6);arch=[aarch64,x86_64];addon=sle-module-example(major=15,minor=SP6)"
                    .to_owned()
            ]
        );
    }

    // --- packages_map: "standard" vs target.version, and the arch strip ---

    #[test]
    fn packages_map_keys_classic_maintenance_by_target_version() {
        let d = doc(MAINTENANCE_OBS);
        let map = packages_map(&d);
        assert_eq!(map.keys().collect::<Vec<_>>(), vec!["15-SP6-LTSS"]);
        let pkgs = &map["15-SP6-LTSS"];
        assert_eq!(pkgs["smc-tools"], "1.8.8-150600.3.9.1");
        assert_eq!(pkgs["smc-tools-completion"], "1.8.8-150600.3.9.1");
    }

    #[test]
    fn packages_map_keys_slfo_and_pi_as_standard() {
        for raw in [SLFO_GITEA, PI] {
            let d = doc(raw);
            let map = packages_map(&d);
            assert_eq!(map.keys().collect::<Vec<_>>(), vec!["standard"], "{raw}");
        }
    }

    /// A regression that keys SLFO by `target.version` instead of
    /// `"standard"` must be observed red by this exact assertion.
    #[test]
    fn packages_map_slfo_is_not_keyed_by_version() {
        let d = doc(SLFO_GITEA);
        let map = packages_map(&d);
        assert!(!map.contains_key("16.0"));
    }

    /// A regression that drops the arch strip must be observed red by this
    /// exact assertion.
    #[test]
    fn packages_map_strips_the_arch_suffix() {
        let d = doc(MAXIMAL);
        let map = packages_map(&d);
        assert_eq!(map["15-SP6"]["examplepkg"], "1.2.3-1.1");
        assert_ne!(map["15-SP6"]["examplepkg"], "1.2.3-1.1.x86_64");
    }

    // --- host_packages: keyed like obs_update_repos ---

    /// The per-product map `host_packages` builds for a maintenance `d`.
    fn packages_by_product(d: &ReportDocument) -> HashMap<SystemProduct, HashMap<String, String>> {
        match host_packages(d) {
            HostPackages::PerProduct(map) => map,
            shared @ HostPackages::Shared(_) => panic!("expected per-product: {shared:?}"),
        }
    }

    #[test]
    fn host_packages_keys_by_the_normalized_target_product() {
        let map = packages_by_product(&doc(MAINTENANCE_OBS));
        let key = SystemProduct::new("SLES-LTSS", "15-SP6", "s390x");
        assert_eq!(map.keys().collect::<Vec<_>>(), vec![&key]);
        assert_eq!(
            map[&key],
            HashMap::from([
                ("smc-tools".to_owned(), "1.8.8-150600.3.9.1".to_owned()),
                (
                    "smc-tools-completion".to_owned(),
                    "1.8.8-150600.3.9.1".to_owned()
                ),
            ])
        );
    }

    #[test]
    fn host_packages_matches_the_update_repo_keys() {
        let d = doc(MAINTENANCE_ADDON);
        let by_product: HashSet<_> = packages_by_product(&d).into_keys().collect();
        let repos: HashSet<_> = obs_update_repos(&d).into_keys().collect();
        assert!(!repos.is_empty());
        assert_eq!(by_product, repos);
    }

    /// Two targets of one normalized product naming one package keep the
    /// higher version, whichever comes first.
    #[test]
    fn host_packages_keeps_the_higher_version_within_a_product() {
        let key = SystemProduct::new("SLES-LTSS", "15-SP6", "s390x");
        for versions in [["3.6.1", "3.9.1"], ["3.9.1", "3.6.1"]] {
            let mut d = doc(MAINTENANCE_OBS);
            let template = d.install.targets[0].clone();
            d.install.targets = versions
                .iter()
                .map(|v| {
                    let mut t = template.clone();
                    t.binaries =
                        [("smc-tools".to_owned(), format!("1.8.8-150600.{v}.s390x"))].into();
                    t
                })
                .collect();
            let map = packages_by_product(&d);
            assert_eq!(map[&key]["smc-tools"], "1.8.8-150600.3.9.1", "{versions:?}");
        }
    }

    #[test]
    fn host_packages_shares_the_standard_set_for_slfo_and_pi() {
        for raw in [SLFO_GITEA, PI] {
            let d = doc(raw);
            let standard = packages_map(&d).remove("standard").expect("standard");
            assert!(!standard.is_empty(), "{raw}");
            assert_eq!(host_packages(&d), HostPackages::Shared(standard), "{raw}");
        }
    }

    #[test]
    fn strip_arch_suffix_leaves_a_dotless_string_unchanged() {
        assert_eq!(strip_arch_suffix("no-dots-here"), "no-dots-here");
    }

    // --- composed_index / repositories_set / should_compose ---

    #[test]
    fn should_compose_is_true_for_gitea_or_pi_only() {
        assert!(!should_compose(&doc(MAINTENANCE_OBS))); // obs, maintenance
        assert!(should_compose(&doc(SLFO_GITEA))); // gitea
        assert!(should_compose(&doc(PI))); // obs, but kind: pi
    }

    #[test]
    fn obs_update_repos_normalizes_the_product_and_keeps_the_url() {
        let d = doc(MAINTENANCE_ADDON);
        let target = &d.install.targets[0];
        let want = normalize(SystemProduct::new(
            &target.product,
            &target.version,
            &target.arch,
        ));
        assert_ne!(want.name, target.product, "fixture must exercise normalize");
        let repos = obs_update_repos(&d);
        assert_eq!(repos[&want], target.repository);
    }

    /// A target whose URL `obs_update_repos` drops has no key either, so the
    /// two maps stay keyed alike.
    #[test]
    fn host_packages_keys_only_the_targets_obs_update_repos_keeps() {
        let mut d = doc(MAINTENANCE_ADDON);
        let mut bad = d.install.targets[0].clone();
        bad.arch = "aarch64".to_owned();
        bad.repository = "ftp://not allowed/$(x)".to_owned();
        d.install.targets.push(bad);
        let by_product = packages_by_product(&d);
        let keys: BTreeSet<_> = by_product.keys().collect();
        assert_eq!(keys, obs_update_repos(&d).keys().collect::<BTreeSet<_>>());
        assert!(keys.iter().all(|p| p.arch == "x86_64"), "{by_product:?}");
    }

    /// Not even a host carrying the targets' product, which no repository reaches.
    #[test]
    fn a_maintenance_document_whose_every_target_url_is_invalid_seeds_no_host() {
        let mut d = doc(MAINTENANCE_ADDON);
        for t in &mut d.install.targets {
            t.repository = "ftp://not allowed/$(x)".to_owned();
        }
        assert!(obs_update_repos(&d).is_empty());
        let host = mtui_types::System::new(
            SystemProduct::new("SLES", "15-SP7", "x86_64"),
            BTreeSet::from([SystemProduct::new(
                "sle-module-live-patching",
                "15-SP7",
                "x86_64",
            )]),
            false,
        );
        let seeded = crate::testreport::packages_for_host(&host_packages(&d), &host);
        assert!(seeded.is_empty(), "{seeded:?}");
    }

    #[test]
    fn obs_update_repos_drops_only_a_target_with_an_invalid_url() {
        let mut d = doc(MAINTENANCE_ADDON);
        let mut bad = d.install.targets[0].clone();
        bad.arch = "aarch64".to_owned();
        bad.repository = "ftp://not allowed/$(x)".to_owned();
        d.install.targets.push(bad);
        let repos = obs_update_repos(&d);
        assert_eq!(repos.len(), 1);
        assert!(repos.keys().all(|p| p.arch == "x86_64"));
    }

    #[test]
    fn composed_index_registers_target_binaries() {
        let d = doc(PI);
        let idx = composed_index(&d);
        let key = SystemProduct::new("SLE-Product-SLES", "16.0", "x86_64");
        assert_eq!(idx[&key], BTreeSet::from(["examplepkg".to_owned()]));
    }

    /// The declared-but-missing-arch empty set: `maximal.json` declares
    /// `aarch64` in `update.products[].archs` but ships no target row for it.
    #[test]
    fn composed_index_registers_an_empty_set_for_a_declared_but_missing_arch() {
        let d = doc(MAXIMAL);
        let idx = composed_index(&d);
        let present = SystemProduct::new("SLE-Product-SLES", "15-SP6", "x86_64");
        let missing = SystemProduct::new("SLE-Product-SLES", "15-SP6", "aarch64");
        assert_eq!(
            idx[&present],
            BTreeSet::from(["examplepkg".to_owned(), "examplepkg-devel".to_owned()])
        );
        assert_eq!(idx[&missing], BTreeSet::new());
    }

    #[test]
    fn repositories_set_collects_target_repositories() {
        let d = doc(SLFO_GITEA);
        let repos = repositories_set(&d.install.targets);
        assert_eq!(repos.len(), 2);
        assert!(repos.iter().all(|r| r.contains("SLES-SAP-16.0")));
    }

    // --- issue_maps ---

    #[test]
    fn issue_maps_splits_by_tracker_prefix() {
        let d = doc(MAINTENANCE_OBS);
        let (bugs, jira) = issue_maps(&d.issues);
        assert_eq!(
            bugs.get("1276308"),
            Some(&"SLES 15 SP7 - Add patches from smc-tools 1.8.8".to_owned())
        );
        assert!(jira.is_empty());
    }

    // --- origin_fields ---

    #[test]
    fn origin_fields_populated_only_for_gitea() {
        let d = doc(SLFO_GITEA);
        assert_eq!(
            origin_fields(&d),
            (
                Some("https://src.suse.de/products/SLFO/pulls/7819".to_owned()),
                Some("https://src.suse.de/api/v1/repos/products/SLFO/pulls/7819".to_owned()),
                Some("50d90dc5965410c49890c92506a2812bf939abc31cf5d05503c2f321c203a576".to_owned()),
            )
        );

        let d = doc(MAINTENANCE_OBS);
        assert_eq!(origin_fields(&d), (None, None, None));
    }

    // --- refhosts ---

    #[test]
    fn refhosts_reads_install_checks() {
        let d = doc(MAXIMAL);
        let hosts = refhosts(&d);
        assert_eq!(
            hosts,
            HashSet::from([
                "host-x86-1.suse.de".to_owned(),
                "host-aarch64-1.suse.de".to_owned()
            ])
        );
    }

    #[test]
    fn refhosts_empty_when_testing_install_absent() {
        let d = doc(MAINTENANCE_OBS);
        assert!(refhosts(&d).is_empty());
    }

    // --- apply_document: the whole pipeline on one representative document ---

    #[test]
    fn apply_document_fills_the_maximal_fixture() {
        let d = doc(MAXIMAL);
        let mut b = base();
        apply_document(&mut b, &d);

        assert_eq!(b.rrid.unwrap().to_string(), "SUSE:Maintenance:99999:999999");
        assert_eq!(b.realid.as_deref(), Some("SUSE:Maintenance:99999:999999"));
        assert_eq!(b.packager, "someone@suse.com");
        assert_eq!(b.rating.as_deref(), Some("important"));
        assert_eq!(b.category, "security");
        assert_eq!(
            b.repository,
            "http://download.suse.de/ibs/SUSE:/Maintenance:/99999/"
        );
        assert_eq!(b.reviewer, "Carol Reviewer");
        assert_eq!(
            b.slack_review,
            Some(SlackReviewMarker {
                channel: "C0123456789".to_owned(),
                ts: "1735732800.123456".to_owned(),
            })
        );
        // maintenance + obs: composed/repositories stay empty.
        assert!(b.composed.is_empty());
        assert!(b.repositories.is_empty());
        assert_eq!(b.giteapr, None);
        assert_eq!(b.update_source, mtui_types::UpdateSource::Obs);
    }

    #[test]
    fn apply_document_id_that_fails_rrid_parse_warns_and_leaves_rrid_none() {
        // The schema's id pattern (`^[A-Za-z0-9:._-]+$`) allows this string,
        // so it still parses as a document; only `RequestReviewID::parse`
        // rejects it.
        let raw = MAXIMAL.replace(
            "\"id\": \"SUSE:Maintenance:99999:999999\"",
            "\"id\": \"not-an-rrid-at-all-1234\"",
        );
        let d = doc(&raw);
        let mut b = base();
        apply_document(&mut b, &d);
        assert_eq!(b.rrid, None);
        assert_eq!(b.realid.as_deref(), Some("not-an-rrid-at-all-1234"));
    }

    #[test]
    fn check_document_refusal_sanitizes_the_id() {
        let mut d = doc(SLFO_GITEA);
        d.workflow = DocWorkflow::Obs;
        d.id = "bad \u{9b}2J\u{202e}\nid".to_owned();
        assert_eq!(
            check_document(&d),
            Err("bad  id is served through the OBS workflow, but SLFO updates are Gitea pull requests only".to_owned())
        );
    }
}
