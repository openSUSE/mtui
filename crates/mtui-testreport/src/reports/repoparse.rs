//! Repository-URL derivation helpers (`*repoparse`).
//!
//! Each derives a [`SystemProduct`] → repository-URL mapping — the
//! `update_repos` table a concrete report's `update_repos_parser()` returns
//! and [`RepoManager::run_zypper`](mtui_hosts) consumes.
//!
//! They live next to the report impls (rather than in
//! [`metadata_parsers`](crate::metadata_parsers)) because they *are* the report
//! side of update-repo derivation: `SLTestReport::update_repos_parser`
//! dispatches among [`reporepoparse`], [`slrepoparse`] and [`gitrepoparse`];
//! [`obsrepoparse`] parses a checkout's `project.xml` for the OBS report. All
//! operate on the flat [`SystemProduct`] `(name, version, arch)`.
//!
//! **Security:** every derived URL is validated through [`RepoUrl`] before it
//! enters the `update_repos` map, because it later becomes a root
//! `zypper ar`/`rr` argument; an unsupported scheme or shell-unsafe character
//! is dropped and logged. The exec boundary additionally shell-quotes it.

use std::collections::HashMap;
use std::path::Path;

use mtui_types::{RepoUrl, SystemProduct};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use tracing::error;

use crate::products::{normalize, normalize_16};

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
fn validated_url(url: String) -> Option<String> {
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

/// Parses a product string, dropping (and logging at ERROR) a malformed one so
/// a single bad entry never poisons the whole `*repoparse` batch — the same
/// lenient stance as [`validated_url`].
fn parse_products(product: &str) -> Vec<SystemProduct> {
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

/// Reads `<dir>/project.xml` from an OBS/IBS checkout directory.
fn read_project(dir: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(dir.join("project.xml"))
}

/// Parses an OBS `project.xml` into `(product, repo-name)` pairs.
///
/// Selects each `<repository>` with an `update` `<path>` child (XPath
/// `repository/path[@repository='update']/..`) whose `name` does not contain
/// `DEBUG`, pairing its `name` with the [`SystemProduct`] from the
/// `<releasetarget>` child's `project` attribute.
///
/// quick-xml has no XPath, so this buffers each open `<repository>`'s relevant
/// children and emits on the closing tag once the `update` path has been seen.
fn xmlparse(xml: &str) -> Vec<(SystemProduct, String)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();

    // State for the currently-open `<repository>` element.
    let mut repo_name: Option<String> = None;
    let mut has_update_path = false;
    let mut release_project: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "repository" => {
                repo_name = attr(&e, "name");
                has_update_path = false;
                release_project = None;
            }
            // Both are empty elements; the start form is handled defensively.
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if repo_name.is_some() => {
                match e.local_name().as_ref() {
                    "path" if attr(&e, "repository").as_deref() == Some("update") => {
                        has_update_path = true;
                    }
                    "releasetarget" => {
                        release_project = attr(&e, "project");
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == "repository" => {
                if let Some(name) = repo_name.take()
                    && has_update_path
                    && !name.contains("DEBUG")
                    && let Some(project) = &release_project
                    && let Some(product) = product_from_project(project)
                {
                    out.push((product, name));
                }
                has_update_path = false;
                release_project = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    out
}

/// Builds a [`SystemProduct`] from a releasetarget `project` attribute by
/// taking the last three `:`-separated segments as `name`/`version`/`arch`;
/// fewer than three is `None`, not a panic.
fn product_from_project(project: &str) -> Option<SystemProduct> {
    let parts: Vec<&str> = project.split(':').collect();
    let [name, version, arch] = parts[parts.len().checked_sub(3)?..] else {
        return None;
    };
    Some(SystemProduct::new(name, version, arch))
}

/// Extracts an attribute value from an XML start/empty element by local name.
fn attr(e: &quick_xml::events::BytesStart<'_>, key: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == key)
            .then(|| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
            .flatten()
            .map(|v| v.into_owned())
    })
}

/// Derives the update-repo map for an OBS/IBS incident from its checkout.
///
/// Parses `<dir>/project.xml`, [`normalize`]s each parsed product, and keys it
/// to `<repository>/<repo-name>`. A missing or unreadable `project.xml` yields
/// an empty map — loading is best-effort.
#[must_use]
pub fn obsrepoparse(repository: &str, dir: &Path) -> HashMap<SystemProduct, String> {
    let Ok(xml) = read_project(dir) else {
        return HashMap::new();
    };
    xmlparse(&xml)
        .into_iter()
        .filter_map(|(product, name)| {
            validated_url(urljoin(repository, &name)).map(|url| (normalize(product), url))
        })
        .collect()
}
