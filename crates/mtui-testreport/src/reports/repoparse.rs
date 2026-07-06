//! Repository-URL derivation helpers (`*repoparse`).
//!
//! Port of the `*repoparse` free functions from upstream
//! `mtui/test_reports/metadata_parsers.py`. Each derives a
//! [`SystemProduct`] → repository-URL mapping — the `update_repos` table a
//! concrete report's `update_repos_parser()` returns and
//! [`RepoManager::run_zypper`](mtui_hosts) consumes.
//!
//! They live next to the report impls (rather than in
//! [`metadata_parsers`](crate::metadata_parsers)) because they *are* the report
//! side of update-repo derivation: `SLTestReport::update_repos_parser` dispatches
//! among [`reporepoparse`], [`slrepoparse`], and [`gitrepoparse`] below.
//!
//! ## Scope
//!
//! The OBS variant (`obsrepoparse`/`_read_project`/`_xmlparse`, which parse a
//! checkout's `project.xml`) is **not** ported here: it is only used by the OBS
//! report, which is out of scope for the SL-report task. It lands with the OBS
//! report impl.
//!
//! All helpers operate on the flat [`SystemProduct`] `(name, version, arch)` —
//! upstream's `Product` `NamedTuple`.

use std::collections::HashMap;

use mtui_types::SystemProduct;

use crate::products::normalize_16;

/// Joins a base URL and a path segment the way upstream `os.path.join` does for
/// the URL cases here: a single `/` separator, without collapsing an existing
/// trailing slash on `base` into a doubled one.
///
/// Upstream builds these with `os.path.join(repository, tail)`; the tails used
/// (`"standard"`, `"images/repo/..."`) never start with `/`, so a plain
/// separator-aware join reproduces the exact strings the tests assert.
fn urljoin(base: &str, tail: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{tail}")
    } else {
        format!("{base}/{tail}")
    }
}

/// Parses a product string such as `"SLES 15 (x86_64, aarch64)"` into one
/// [`SystemProduct`] per architecture.
///
/// Port of upstream `_parse_product`: splits on `" ("`, strips the trailing
/// `")"`, splits the arch list on `", "`, and the base on `" "` — taking the
/// first two whitespace tokens as `(name, version)`.
///
/// # Panics
///
/// Mirrors upstream, which indexes `base[0]`/`base[1]` and splits on `" ("`
/// unconditionally: a string not shaped `"<name> <version> (<archs>)"` is a
/// malformed template and panics rather than silently producing wrong repos.
#[must_use]
pub fn parse_product(product: &str) -> Vec<SystemProduct> {
    let (b, a) = product
        .split_once(" (")
        .expect("product string must contain ' (' before the arch list");
    let archs = a.trim_end_matches(')').split(", ");
    let mut base = b.split(' ');
    let name = base.next().expect("product string must have a name token");
    let version = base
        .next()
        .expect("product string must have a version token");
    archs
        .map(|arch| SystemProduct::new(name, version, arch))
        .collect()
}

/// Derives the update-repo map for SUSE Linux (maintenance `1.1`, still in IBS).
///
/// Port of upstream `slrepoparse`: each product/arch maps to
/// `<repository>/images/repo/<name>-<version>-<arch>/`.
#[must_use]
pub fn slrepoparse(repository: &str, products: &[String]) -> HashMap<SystemProduct, String> {
    products
        .iter()
        .flat_map(|pd| parse_product(pd))
        .map(|x| {
            let tail = format!("images/repo/{}-{}-{}/", x.name, x.version, x.arch);
            let url = urljoin(repository, &tail);
            (x, url)
        })
        .collect()
}

/// Derives the update-repo map for git-backed reports.
///
/// Port of upstream `gitrepoparse`: every product/arch maps to
/// `<repository>/standard`.
#[must_use]
pub fn gitrepoparse(repository: &str, products: &[String]) -> HashMap<SystemProduct, String> {
    products
        .iter()
        .flat_map(|pd| parse_product(pd))
        .map(|x| {
            let url = urljoin(repository, "standard");
            (x, url)
        })
        .collect()
}

/// Derives the update-repo map from an explicit set of repository URLs.
///
/// Port of upstream `reporepoparse`: for each product/arch, matches the repo URL
/// that contains `<name>-<version>-<arch>` and keys it under the
/// [`normalize_16`]-canonicalized product.
#[must_use]
pub fn reporepoparse(
    repositories: &[String],
    products: &[String],
) -> HashMap<SystemProduct, String> {
    let mut out = HashMap::new();
    for pd in products {
        for ps in parse_product(pd) {
            let needle = format!("{}-{}-{}", ps.name, ps.version, ps.arch);
            for repo in repositories {
                if repo.contains(&needle) {
                    out.insert(normalize_16(ps.clone()), repo.clone());
                }
            }
        }
    }
    out
}
