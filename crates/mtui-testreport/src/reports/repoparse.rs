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
//! The OBS variant ([`obsrepoparse`] + its private `read_project`/`xmlparse`
//! helpers) parses a checkout's `project.xml`; it is used by the OBS report.
//!
//! All helpers operate on the flat [`SystemProduct`] `(name, version, arch)` —
//! upstream's `Product` `NamedTuple`.

use std::collections::HashMap;
use std::path::Path;

use mtui_types::SystemProduct;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::products::{normalize, normalize_16};

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

/// Reads the `project.xml` file from an OBS/IBS checkout directory.
///
/// Port of upstream `_read_project`: reads `<dir>/project.xml` to a string.
fn read_project(dir: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(dir.join("project.xml"))
}

/// Parses an OBS `project.xml` into `(product, repo-name)` pairs.
///
/// Port of upstream `_xmlparse`, whose XPath
/// `repository/path[@repository='update']/..` selects each `<repository>` that
/// has an `update` `<path>` child, excluding any whose `name` contains `DEBUG`.
/// For each such repository it yields the [`SystemProduct`] built from the
/// `<releasetarget>` child's `project` attribute (split on `:`, last three
/// segments → `name`/`version`/`arch`) paired with the repository's `name`.
///
/// quick-xml has no XPath, so this buffers each open `<repository>` element's
/// relevant children (`path`, `releasetarget`) and emits the pair on the
/// closing tag once the `update` path has been seen — mirroring the
/// event-driven style used by `parse_patchinfo`.
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
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"repository" => {
                repo_name = attr(&e, b"name");
                has_update_path = false;
                release_project = None;
            }
            // `<path repository="update"/>` and `<releasetarget project="..."/>`
            // are empty elements; handle both empty and (defensively) start form.
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if repo_name.is_some() => {
                match e.local_name().as_ref() {
                    b"path" if attr(&e, b"repository").as_deref() == Some("update") => {
                        has_update_path = true;
                    }
                    b"releasetarget" => {
                        release_project = attr(&e, b"project");
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"repository" => {
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
/// taking the last three `:`-separated segments as `name`/`version`/`arch`.
///
/// Mirrors upstream `project.split(":")[-3:]`; returns `None` when fewer than
/// three segments are present rather than panicking.
fn product_from_project(project: &str) -> Option<SystemProduct> {
    let parts: Vec<&str> = project.split(':').collect();
    let [name, version, arch] = parts[parts.len().checked_sub(3)?..] else {
        return None;
    };
    Some(SystemProduct::new(name, version, arch))
}

/// Extracts an attribute value from an XML start/empty element by local name.
fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (a.key.local_name().as_ref() == key)
            .then(|| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
            .flatten()
            .map(|v| v.into_owned())
    })
}

/// Derives the update-repo map for an OBS/IBS incident from its checkout.
///
/// Port of upstream `obsrepoparse`: parses `<dir>/project.xml`, [`normalize`]s
/// each parsed product, and keys it to `<repository>/<repo-name>`.
///
/// A missing or unreadable `project.xml` yields an empty map (upstream would
/// raise; here loading is best-effort — the caller has no repos to act on).
#[must_use]
pub fn obsrepoparse(repository: &str, dir: &Path) -> HashMap<SystemProduct, String> {
    let Ok(xml) = read_project(dir) else {
        return HashMap::new();
    };
    xmlparse(&xml)
        .into_iter()
        .map(|(product, name)| (normalize(product), urljoin(repository, &name)))
        .collect()
}
