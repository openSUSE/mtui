//! Metadata parsers for testreport sources:
//!
//! * [`ReducedMetadataParser`] — line-based parser for the template's `hosts`
//!   field; extracts reference hostnames plus jira/bug ids and their titles.
//! * [`JSONParser`] — extracts metadata from the JSON envelope produced by the
//!   build pipeline and populates a [`TestReportBase`].
//! * [`patchinfo_titles`] — best-effort `issue id -> title` map read from a
//!   checkout's `patchinfo.xml`, used to enrich the bare bug/jira ids the JSON
//!   envelope carries.
//!
//! Repository-URL derivation and product normalization are deliberately *not*
//! here — they are the report side, in [`crate::reports::repoparse`].

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

use mtui_types::{PackageSpec, RequestReviewID, SystemProduct, UpdateSource, parse_rpm_filename};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;
use serde::Deserialize;
use tracing::{debug, error, warn};

use crate::products::{normalize, normalize_16};
use crate::reports::repoparse::parse_products;
use crate::testreport::TestReportBase;

/// Placeholder description for bare bug/jira ids from the JSON envelope
/// (their human-readable titles are filled later, e.g. from
/// [`patchinfo_titles`]).
const NO_DESCRIPTION: &str = "Description not available";

/// `.* \(reference host: (\S+).*\)` — a reference-host line. The captured host
/// is skipped when it contains `?`.
static HOSTNAMES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r".* \(reference host: (\S+).*\)").expect("valid hostnames regex"));

/// `Jira ([A-Z]+-\d+) \("(.*)"\):` — a jira id and its title.
static JIRA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"Jira ([A-Z]+-\d+) \("(.*)"\):"#).expect("valid jira regex"));

/// `Bug (\d+) \("(.*)"\):` — a bug id and its title.
static BUGS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"Bug (\d+) \("(.*)"\):"#).expect("valid bugs regex"));

/// A line-based parser for the template's `hosts` field.
///
/// Registered under the `"hosts"` key in each concrete report's parser table;
/// the report feeds it the field's lines one at a time. Each line is matched
/// first-wins against the reference-host (placeholders containing `?` are
/// skipped), `Jira ID ("title"):` and `Bug ID ("title"):` patterns; a line
/// matching none of them is ignored.
pub struct ReducedMetadataParser;

impl ReducedMetadataParser {
    /// Parses a single line and records any hostname / jira / bug it carries
    /// into `results`.
    pub fn parse(results: &mut TestReportBase, line: &str) {
        // First marker wins, matching the writer's collapse, so read and write
        // agree on which message the gate checks.
        if line.starts_with("Slack Review:") {
            if results.slack_review.is_none()
                && let Some(marker) = crate::testreport::SlackReviewMarker::parse_line(line)
            {
                results.slack_review = Some(marker);
            }
            return;
        }

        // Each guard is a mandatory literal substring of its regex, so a miss
        // here means the pattern cannot match either; a hit still falls
        // through to `captures()` (and on to the next pattern on a guarded
        // non-match), so behaviour is unchanged, just cheaper for the common
        // case of a line matching none of them.
        if line.contains(" (reference host: ")
            && let Some(caps) = HOSTNAMES_RE.captures(line)
        {
            let host = &caps[1];
            if !host.contains('?') {
                results.hostnames.insert(host.to_owned());
            }
            return;
        }

        if line.contains("Jira ")
            && let Some(caps) = JIRA_RE.captures(line)
        {
            results.jira.insert(caps[1].to_owned(), caps[2].to_owned());
            return;
        }

        if line.contains("Bug ")
            && let Some(caps) = BUGS_RE.captures(line)
        {
            results.bugs.insert(caps[1].to_owned(), caps[2].to_owned());
        }
    }
}

/// The JSON metadata envelope produced by the build pipeline.
///
/// Every field is optional so a partial envelope parses without error, an
/// absent key meaning empty; `#[serde(rename)]` restores the wire key wherever
/// the Rust name would clash with the report's own naming.
#[derive(Debug, Default, Deserialize)]
pub struct MetadataEnvelope {
    /// Jira issue ids.
    #[serde(default)]
    jira: Option<Vec<String>>,
    /// Bugzilla bug ids.
    #[serde(default)]
    bugs: Option<Vec<String>>,
    /// Request Review ID string (e.g. `SUSE:Maintenance:1:1`).
    #[serde(default)]
    rrid: Option<String>,
    /// Packager.
    #[serde(default)]
    packager: Option<String>,
    /// Update rating.
    #[serde(default)]
    rating: Option<String>,
    /// Update repository string.
    #[serde(default)]
    repository: Option<String>,
    /// Update category.
    #[serde(default)]
    category: Option<String>,
    /// Test platform strings (envelope key `testplatform`).
    #[serde(default, rename = "testplatform")]
    testplatforms: Option<Vec<String>>,
    /// Product strings.
    #[serde(default)]
    products: Option<Vec<String>>,
    /// Raw request id (envelope key `id`).
    #[serde(default, rename = "id")]
    realid: Option<String>,
    /// Gitea pull-request reference.
    #[serde(default)]
    gitea_pr: Option<String>,
    /// Gitea pull-request API URL.
    #[serde(default)]
    gitea_pr_api: Option<String>,
    /// Gitea commit hash.
    #[serde(default)]
    gitea_commit_hash: Option<String>,
    /// Nested package map: `product -> ["<name> <op> <version>", ...]`, e.g.
    /// `{"standard": ["afterburn = 5.10.0.git73.b97f772-99999_stage.1.1"]}`.
    #[serde(default)]
    packages: Option<HashMap<String, Vec<String>>>,
    /// Update repository URLs.
    #[serde(default)]
    repositories: Option<Vec<String>>,
    /// `"<name>-<version>" -> arch -> ["<name>-<version>-<release>.<arch>.rpm", ...]`:
    /// the binaries this update composes for each product.
    ///
    /// Held untyped and shape-checked in [`index_binaries`]: the block's shape
    /// is not a contract TeReGen has committed to, and a typed field would turn
    /// an unexpected one into a whole-report `MetadataInvalid`.
    #[serde(default)]
    binaries: Option<serde_json::Value>,
}

/// A parser for the JSON metadata envelope; stateless, mutating the supplied
/// [`TestReportBase`] in place.
pub struct JSONParser;

impl JSONParser {
    /// Parses a raw JSON string into a [`MetadataEnvelope`] and applies it.
    ///
    /// Convenience wrapper over `parse` for loading straight from a
    /// `metadata.json`.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`serde_json::Error`] when the input is not valid
    /// JSON matching the envelope shape.
    pub fn parse_str(results: &mut TestReportBase, data: &str) -> Result<(), serde_json::Error> {
        let envelope: MetadataEnvelope = serde_json::from_str(data)?;
        Self::parse(results, &envelope);
        Ok(())
    }

    /// Applies a parsed [`MetadataEnvelope`] to `results`.
    ///
    /// * jira/bugs ids are seeded with the [`NO_DESCRIPTION`] placeholder;
    /// * `rrid` is parsed via [`RequestReviewID`]; absent or malformed leaves
    ///   the field `None` rather than panicking on bad input;
    /// * scalar fields map straight through;
    /// * each package entry `"<name> <op> <version>"` is split on whitespace,
    ///   taking token 1 as the name and token 3 as the version — the operator
    ///   is discarded, so it is not significant.
    fn parse(results: &mut TestReportBase, data: &MetadataEnvelope) {
        for id in data.jira.iter().flatten() {
            results.jira.insert(id.clone(), NO_DESCRIPTION.to_owned());
        }
        for id in data.bugs.iter().flatten() {
            results.bugs.insert(id.clone(), NO_DESCRIPTION.to_owned());
        }

        results.rrid = data
            .rrid
            .as_deref()
            .and_then(|s| RequestReviewID::parse(s).ok());
        results.packager = data.packager.clone().unwrap_or_default();
        results.rating = data.rating.clone();
        results.repository = data.repository.clone().unwrap_or_default();
        results.category = data.category.clone().unwrap_or_default();
        results.testplatforms = data.testplatforms.clone().unwrap_or_default();
        results.products = data.products.clone().unwrap_or_default();
        results.realid = data.realid.clone();
        results.giteapr = data.gitea_pr.clone();
        results.giteaprapi = data.gitea_pr_api.clone();
        results.giteacohash = data.gitea_commit_hash.clone();
        results.update_source = if data
            .gitea_commit_hash
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            UpdateSource::Git
        } else {
            UpdateSource::Obs
        };
        debug!(update_source = ?results.update_source, "resolved update source from metadata");

        let mut packages: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (prod, pkgvers) in data.packages.iter().flatten() {
            let mut pkgs = HashMap::new();
            for entry in pkgvers {
                let mut tokens = entry.split_whitespace();
                match (tokens.next(), tokens.next(), tokens.next()) {
                    (Some(pkg), Some(_), Some(ver)) => {
                        if tokens.next().is_some() {
                            warn!(
                                product = %prod, entry = %entry,
                                "package entry has trailing tokens; using the first as name and third as version"
                            );
                        }
                        // These are interpolated into root remote commands, so
                        // reject non-RPM names at ingestion (log, never fail).
                        if let Err(e) = PackageSpec::parse_name(pkg) {
                            error!(package = %pkg, error = %e, "skipping invalid package name in metadata");
                            continue;
                        }
                        pkgs.insert(pkg.to_owned(), ver.to_owned());
                    }
                    // One- and two-token entries used to vanish with no trace
                    // (#396): the sibling invalid-name arm logs, so must this.
                    _ => error!(
                        product = %prod, entry = %entry,
                        "skipping unparsable package entry in metadata (expected \"<name> <op> <version>\")"
                    ),
                }
            }
            // An empty sub-map makes `base.packages` non-empty while the
            // flattened `get_package_list()` stays empty, defeating every
            // downstream "is anything to install?" guard (#396).
            if pkgs.is_empty() {
                error!(
                    product = %prod,
                    "metadata names this product but no package entry was parsable; dropping the empty entry"
                );
                continue;
            }
            packages.insert(prod.clone(), pkgs);
        }
        if data.packages.iter().flatten().count() > 0 && packages.is_empty() {
            warn!(
                "metadata carries no parsable package versions; prepare/downgrade have nothing \
                 to install and before/after version checks cannot run"
            );
        }
        results.packages = packages;

        results.repositories = data
            .repositories
            .clone()
            .map(|r| r.into_iter().collect())
            .unwrap_or_default();

        results.composed = data
            .binaries
            .as_ref()
            .map(|b| index_binaries(&results.products, b))
            .unwrap_or_default();
    }
}

/// Indexes the envelope's `binaries` block as
/// `SystemProduct -> the package names this update composes for it`.
///
/// Each `binaries` key is a `"<name>-<version>"` product; `products` supplies
/// the parse, so a key naming no declared product is skipped. A product's
/// `noarch` names are unioned across *all* its arch keys before the per-arch
/// sets are built: a `noarch` binary is composed for every arch, but real
/// metadata lists it under only some of them.
///
/// `products` lists every arch a product *exists* on, `binaries` only what this
/// update *built*: an arch in the former and absent from the latter ships
/// nothing, and is indexed as an explicitly empty set so
/// `narrow_to_composed` refuses that host by name rather than falling through
/// to the full list — the `zypper 104` this index exists to prevent. Not even
/// the product's `noarch` names: with no arch key at all there is no repo for
/// this update on that arch to install them from. An arch the metadata never
/// declares stays unknown and keeps falling open, and so does a product whose
/// `binaries` map lists no arch whatsoever — absence is only informative
/// against an enumeration that exists.
///
/// Every entry that parses as an RPM filename is indexed under its arch *key*,
/// whatever arch the filename itself carries: a 32-bit compat binary listed
/// under `x86_64` is installable there, and a source RPM merely names a package
/// the update ships. Over-inclusion degrades to today's behaviour; dropping a
/// name silently shortens a host's list, which is what this index exists to
/// prevent. For the same reason an entry that is *not* an RPM filename — or
/// that yields a name [`PackageSpec::parse_name`] rejects — abandons the whole
/// index rather than poisoning one key: the block cannot be trusted. Abandoning
/// is the fail-*open* direction (an empty index composes everything, exactly as
/// before the index existed), so a hostile `binaries` block can at worst switch
/// the narrowing off, never shorten what a host installs.
///
/// `binaries` is untyped here because its shape is not a committed contract —
/// a block of some other shape yields an empty index and a warning, never a
/// failed report load.
fn index_binaries(
    products: &[String],
    binaries: &serde_json::Value,
) -> HashMap<SystemProduct, BTreeSet<String>> {
    let binaries: HashMap<String, HashMap<String, Vec<String>>> =
        match serde_json::from_value(binaries.clone()) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    error = %e,
                    "metadata's `binaries` block has an unexpected shape; \
                     preparing without a composition"
                );
                return HashMap::new();
            }
        };

    let parsed: Vec<SystemProduct> = products.iter().flat_map(|pd| parse_products(pd)).collect();

    let mut out: HashMap<SystemProduct, BTreeSet<String>> = HashMap::new();
    for (key, arch_map) in &binaries {
        let Some(product) = parsed
            .iter()
            .find(|p| format!("{}-{}", p.name, p.version) == *key)
        else {
            debug!(key = %key, "binaries key matches no product");
            continue;
        };

        let mut noarch: BTreeSet<String> = BTreeSet::new();
        let mut per_arch: Vec<(&String, BTreeSet<String>)> = Vec::new();
        for (arch, entries) in arch_map {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for entry in entries {
                let Some((name, entry_arch)) = parse_rpm_filename(entry) else {
                    warn!(
                        product = %key, arch = %arch, entry = %entry,
                        "unparseable binaries entry; the composition index is abandoned"
                    );
                    return HashMap::new();
                };
                if let Err(e) = PackageSpec::parse_name(name) {
                    warn!(
                        product = %key, arch = %arch, entry = %entry, error = %e,
                        "binaries entry names an invalid package; the composition index is abandoned"
                    );
                    return HashMap::new();
                }
                if entry_arch == "noarch" {
                    noarch.insert(name.to_owned());
                } else {
                    names.insert(name.to_owned());
                }
            }
            per_arch.push((arch, names));
        }

        for (arch, mut names) in per_arch {
            names.extend(noarch.iter().cloned());
            register(
                &mut out,
                SystemProduct::new(&product.name, &product.version, arch),
                &names,
            );
        }

        // Only against a non-empty enumeration: a product for which `binaries`
        // lists no arch at all says nothing about any of them, so it must stay
        // fail-open rather than refuse every host on that product.
        if !arch_map.is_empty() {
            for declared in parsed.iter().filter(|p| {
                p.name == product.name
                    && p.version == product.version
                    && !arch_map.contains_key(&p.arch)
            }) {
                debug!(
                    product = %key, arch = %declared.arch,
                    "products declares this arch but binaries ship nothing for it; composing nothing"
                );
                register(&mut out, declared.clone(), &BTreeSet::new());
            }
        }
    }
    out
}

/// Records `names` for `raw` under one key per normalizer the tree's repo
/// parsers use — raw (`gitrepoparse`/`slrepoparse`), `normalize_16`
/// (`reporepoparse`) and `normalize` (`obsrepoparse`) — because the host reports
/// whichever form its own `/etc/products.d` carries. Where two agree the entry
/// simply merges; where a normalizer's output collides with another product's
/// raw name the sets union, which is over-inclusive and so degrades toward
/// today's behaviour.
///
/// An empty `names` still creates the keys: "composes nothing here" is a
/// statement, distinct from an absent key's "nothing is known here".
fn register(
    out: &mut HashMap<SystemProduct, BTreeSet<String>>,
    raw: SystemProduct,
    names: &BTreeSet<String>,
) {
    for form in BTreeSet::from([normalize_16(raw.clone()), normalize(raw.clone()), raw]) {
        out.entry(form).or_default().extend(names.iter().cloned());
    }
}

/// Maps `issue id -> title` from a checkout's `patchinfo.xml`.
///
/// The JSON envelope carries only bare bug/jira *ids* (descriptions are the
/// `NO_DESCRIPTION` placeholder); the human-readable titles live in
/// `<issue tracker="bnc" id="123">title</issue>` elements. Best-effort: not
/// every report kind ships the file and a malformed one must never break
/// loading, so both yield an empty map.
#[must_use]
pub fn patchinfo_titles(directory: &Path) -> HashMap<String, String> {
    let pi = directory.join("patchinfo.xml");
    let Ok(content) = std::fs::read_to_string(&pi) else {
        return HashMap::new();
    };
    parse_patchinfo(&content).unwrap_or_default()
}

/// Parses `patchinfo.xml` content into an `id -> title` map; `None` on any XML
/// error, so the caller can degrade to an empty map.
fn parse_patchinfo(content: &str) -> Option<HashMap<String, String>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut titles = HashMap::new();
    let mut buf = Vec::new();
    // The `id` attribute of the currently-open `<issue>` element, if any.
    let mut current_id: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == "issue" => {
                current_id = issue_id(&e);
            }
            Ok(Event::Text(e)) => {
                if let Some(id) = &current_id {
                    let title = e.trim();
                    if !title.is_empty() {
                        titles.insert(id.clone(), title.to_owned());
                    }
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == "issue" => {
                current_id = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    Some(titles)
}

/// Extracts the trimmed, non-empty `id` attribute of an `<issue>` element.
fn issue_id(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    e.attributes().flatten().find_map(|attr| {
        if attr.key.local_name().as_ref() == "id" {
            let val = attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()?;
            let trimmed = val.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::options::Config;

    fn base() -> TestReportBase {
        TestReportBase::new(Config::default())
    }

    #[test]
    fn update_source_is_git_when_hash_present() {
        let mut results = base();
        JSONParser::parse_str(&mut results, r#"{"gitea_commit_hash": "deadbeef"}"#).unwrap();
        assert_eq!(results.update_source, UpdateSource::Git);
    }

    #[test]
    fn update_source_is_obs_when_hash_absent() {
        let mut results = base();
        JSONParser::parse_str(&mut results, "{}").unwrap();
        assert_eq!(results.update_source, UpdateSource::Obs);
    }

    #[test]
    fn update_source_is_obs_when_hash_is_blank_or_whitespace() {
        for hash in ["", "   "] {
            let mut results = base();
            JSONParser::parse_str(
                &mut results,
                &format!(r#"{{"gitea_commit_hash": "{hash}"}}"#),
            )
            .unwrap();
            assert_eq!(results.update_source, UpdateSource::Obs, "hash={hash:?}");
        }
    }

    /// The dual-served case: the RRID looks classic (`1.1`) and only the
    /// metadata tells the true story. The executable statement of the
    /// [`UpdateSource`] precedence rule.
    #[test]
    fn dual_served_slfo_1_1_with_hash_resolves_to_git() {
        let mut results = base();
        JSONParser::parse_str(
            &mut results,
            r#"{"rrid": "SUSE:SLFO:1.1:418286", "gitea_commit_hash": "deadbeef"}"#,
        )
        .unwrap();
        assert_eq!(results.update_source, UpdateSource::Git);
    }
}
