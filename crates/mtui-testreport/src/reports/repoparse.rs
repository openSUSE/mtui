//! Repository-URL derivation helpers (`*repoparse`).
//!
//! Each derives a [`SystemProduct`] → repository-URL mapping — the
//! `update_repos` table a concrete report's `update_repos_parser()` returns
//! and [`RepoManager::run_zypper`](mtui_hosts) consumes.
//!
//! They live next to the report impls because they *are* the report side of
//! update-repo derivation: `SLTestReport::update_repos_parser` dispatches among
//! [`reporepoparse`], [`slrepoparse`] and [`gitrepoparse`]. All operate on the
//! flat [`SystemProduct`] `(name, version, arch)`.
//!
//! **Security:** every derived URL is validated through [`RepoUrl`] before it
//! enters the `update_repos` map, because it later becomes a root
//! `zypper ar`/`rr` argument; an unsupported scheme or shell-unsafe character
//! is dropped and logged. The exec boundary additionally shell-quotes it.

use std::collections::HashMap;

use mtui_types::{RepoUrl, SystemProduct};
use tracing::error;

use crate::products::normalize_16;

/// A product string sourced from external metadata (`metadata.json`) was not
/// shaped `"<name> <version> (<archs>)"`.
///
/// [`parse_product`] returns this rather than panicking, so a malformed
/// template degrades to "no repos for that entry" instead of aborting the
/// process under release `panic=abort`.
#[derive(Debug, thiserror::Error)]
#[error("malformed product string {product:?}: {reason}")]
pub struct ProductParseError {
    /// The offending product string.
    product: String,
    /// Why it failed to parse.
    reason: &'static str,
}

/// Validates a derived repository URL before it becomes part of an
/// `update_repos` map (and thus a root `zypper ar`/`rr` argument).
///
/// A URL failing [`RepoUrl`] validation is dropped and logged at ERROR rather
/// than trusted, keeping loading lenient.
pub(crate) fn validated_url(url: String) -> Option<String> {
    match RepoUrl::parse(&url) {
        Ok(_) => Some(url),
        Err(e) => {
            error!(%url, error = %e, "skipping invalid repository URL");
            None
        }
    }
}

/// Joins a base URL and a path segment with exactly one `/` separator.
///
/// The tails used (`"standard"`, `"images/repo/..."`) never start with `/`, so
/// this reproduces the exact strings the tests assert.
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
/// Splits on `" ("`, strips the trailing `")"`, splits the arch list on `", "`,
/// and takes the base's first two whitespace tokens as `(name, version)`.
///
/// # Errors
///
/// Returns [`ProductParseError`] when `product` is not shaped
/// `"<name> <version> (<archs>)"`. Externally-sourced metadata is untrusted, so
/// this is a typed error rather than a panic (fatal under `panic=abort`).
pub fn parse_product(product: &str) -> Result<Vec<SystemProduct>, ProductParseError> {
    let err = |reason| ProductParseError {
        product: product.to_owned(),
        reason,
    };
    let (b, a) = product
        .split_once(" (")
        .ok_or_else(|| err("missing ' (' before the arch list"))?;
    let archs = a.trim_end_matches(')').split(", ");
    let mut base = b.split(' ');
    let name = base.next().ok_or_else(|| err("missing name token"))?;
    let version = base.next().ok_or_else(|| err("missing version token"))?;
    Ok(archs
        .map(|arch| SystemProduct::new(name, version, arch))
        .collect())
}

/// Derives the update-repo map for SUSE Linux (maintenance `1.1`, still in IBS).
///
/// Each product/arch maps to
/// `<repository>/images/repo/<name>-<version>-<arch>/`.
#[must_use]
pub fn slrepoparse(repository: &str, products: &[String]) -> HashMap<SystemProduct, String> {
    products
        .iter()
        .flat_map(|pd| parse_products(pd))
        .filter_map(|x| {
            let tail = format!("images/repo/{}-{}-{}/", x.name, x.version, x.arch);
            validated_url(urljoin(repository, &tail)).map(|url| (x, url))
        })
        .collect()
}

/// Parses a product string, dropping (and logging at ERROR) a malformed one so a
/// single bad entry never poisons the whole `*repoparse` batch.
///
/// This is the lenient wrapper the `*repoparse` helpers use, mirroring
/// [`validated_url`]'s drop-and-log stance for invalid URLs.
pub(crate) fn parse_products(product: &str) -> Vec<SystemProduct> {
    match parse_product(product) {
        Ok(ps) => ps,
        Err(e) => {
            error!(error = %e, "skipping malformed product string");
            Vec::new()
        }
    }
}

/// Derives the update-repo map for git-backed reports.
///
/// Every product/arch maps to `<repository>/standard`.
#[must_use]
pub fn gitrepoparse(repository: &str, products: &[String]) -> HashMap<SystemProduct, String> {
    products
        .iter()
        .flat_map(|pd| parse_products(pd))
        .filter_map(|x| validated_url(urljoin(repository, "standard")).map(|url| (x, url)))
        .collect()
}

/// Derives the update-repo map from an explicit set of repository URLs.
///
/// For each product/arch, matches the repo URL that contains
/// `<name>-<version>-<arch>` and keys it under the
/// [`normalize_16`]-canonicalized product.
#[must_use]
pub fn reporepoparse(
    repositories: &[String],
    products: &[String],
) -> HashMap<SystemProduct, String> {
    let mut out = HashMap::new();
    for pd in products {
        for ps in parse_products(pd) {
            let needle = format!("{}-{}-{}", ps.name, ps.version, ps.arch);
            for repo in repositories {
                if repo.contains(&needle)
                    && let Some(url) = validated_url(repo.clone())
                {
                    out.insert(normalize_16(ps.clone()), url);
                }
            }
        }
    }
    out
}
