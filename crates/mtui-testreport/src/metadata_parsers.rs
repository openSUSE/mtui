//! Metadata parsers for testreport sources.
//!
//! Port of the metadata parsers in upstream
//! `mtui/test_reports/metadata_parsers.py`:
//!
//! * [`ReducedMetadataParser`] — line-based parser for the template's `hosts`
//!   field; extracts reference hostnames plus jira/bug ids and their titles.
//!   Registered under the `"hosts"` key in every concrete report's parser table
//!   (SL/PI/OBS), so it is a live part of template loading.
//! * [`JSONParser`] — extracts metadata from the JSON envelope produced by the
//!   build pipeline and populates a [`TestReportBase`].
//! * [`patchinfo_titles`] — best-effort `issue id -> title` map read from a
//!   checkout's `patchinfo.xml`, used to enrich the bare bug/jira ids the JSON
//!   envelope carries.
//!
//! The `*repoparse`/`normalize` helpers are intentionally **not** ported here:
//! the repository-URL derivation and product normalization belong to the
//! products/report tasks, not to metadata parsing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use mtui_types::{PackageSpec, RequestReviewID};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;
use serde::Deserialize;
use tracing::error;

use crate::testreport::TestReportBase;

/// Placeholder description upstream assigns to bare bug/jira ids from the JSON
/// envelope (their human-readable titles are filled later, e.g. from
/// [`patchinfo_titles`]).
const NO_DESCRIPTION: &str = "Description not available";

/// `.* \(reference host: (\S+).*\)` — a reference-host line. The captured host
/// is skipped when it contains `?` (upstream guards `"?" not in match`).
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
/// Port of upstream `ReducedMetadataParser`. Registered under the `"hosts"` key
/// in each concrete report's parser table; the report feeds it the field's lines
/// one at a time. Each line is matched against three patterns, in order, and the
/// first match wins (mirroring upstream's early `return` after each match):
///
/// * a reference-host line adds a hostname (skipping placeholders containing
///   `?`);
/// * a `Jira ID ("title"):` line records the jira id and its title;
/// * a `Bug ID ("title"):` line records the bug id and its title.
///
/// Lines matching none of the patterns are ignored.
pub struct ReducedMetadataParser;

impl ReducedMetadataParser {
    /// Parses a single line and records any hostname / jira / bug it carries
    /// into `results`.
    pub fn parse(results: &mut TestReportBase, line: &str) {
        if let Some(caps) = HOSTNAMES_RE.captures(line) {
            let host = &caps[1];
            if !host.contains('?') {
                results.hostnames.insert(host.to_owned());
            }
            return;
        }

        if let Some(caps) = JIRA_RE.captures(line) {
            results.jira.insert(caps[1].to_owned(), caps[2].to_owned());
            return;
        }

        if let Some(caps) = BUGS_RE.captures(line) {
            results.bugs.insert(caps[1].to_owned(), caps[2].to_owned());
        }
    }
}

/// The JSON metadata envelope produced by the build pipeline.
///
/// Every field is optional so a partial envelope parses without error, mirroring
/// upstream's `data.get(...)` access (absent list/dict keys behave as empty).
/// The field names map to the envelope keys; where a Rust field name would clash
/// with the report's own naming, `#[serde(rename)]` restores the wire key.
#[derive(Debug, Default, Deserialize)]
pub struct MetadataEnvelope {
    /// Jira issue ids.
    #[serde(default)]
    pub jira: Option<Vec<String>>,
    /// Bugzilla bug ids.
    #[serde(default)]
    pub bugs: Option<Vec<String>>,
    /// Request Review ID string (e.g. `SUSE:Maintenance:1:1`).
    #[serde(default)]
    pub rrid: Option<String>,
    /// Packager.
    #[serde(default)]
    pub packager: Option<String>,
    /// Update rating.
    #[serde(default)]
    pub rating: Option<String>,
    /// Update repository string.
    #[serde(default)]
    pub repository: Option<String>,
    /// Update category.
    #[serde(default)]
    pub category: Option<String>,
    /// Test platform strings (envelope key `testplatform`).
    #[serde(default, rename = "testplatform")]
    pub testplatforms: Option<Vec<String>>,
    /// Product strings.
    #[serde(default)]
    pub products: Option<Vec<String>>,
    /// Raw request id (envelope key `id`).
    #[serde(default, rename = "id")]
    pub realid: Option<String>,
    /// Gitea pull-request reference.
    #[serde(default)]
    pub gitea_pr: Option<String>,
    /// Gitea pull-request API URL.
    #[serde(default)]
    pub gitea_pr_api: Option<String>,
    /// Gitea commit hash.
    #[serde(default)]
    pub gitea_commit_hash: Option<String>,
    /// Nested package map: `product -> ["pkg _ version", ...]`.
    #[serde(default)]
    pub packages: Option<HashMap<String, Vec<String>>>,
    /// Update repository URLs.
    #[serde(default)]
    pub repositories: Option<Vec<String>>,
}

/// A parser for the JSON metadata envelope.
///
/// Port of upstream `JSONParser`. Stateless; [`parse`](JSONParser::parse)
/// mutates the supplied [`TestReportBase`] in place, matching upstream's
/// mutate-`results` shape.
pub struct JSONParser;

impl JSONParser {
    /// Parses a raw JSON string into a [`MetadataEnvelope`] and applies it.
    ///
    /// Convenience wrapper over [`parse`](JSONParser::parse) for the common case
    /// of loading straight from a `metadata.json`.
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
    /// Field-for-field port of upstream `JSONParser.parse`:
    ///
    /// * jira/bugs ids are seeded with the [`NO_DESCRIPTION`] placeholder;
    /// * `rrid` is parsed via [`RequestReviewID`] (an absent or malformed value
    ///   leaves the field `None` — upstream constructs it eagerly, but a typed
    ///   `Result` here degrades gracefully rather than panicking on bad input);
    /// * scalar fields map straight through;
    /// * each package entry `"pkg _ version"` is split on whitespace, taking the
    ///   first token as the package name and the third as the version.
    pub fn parse(results: &mut TestReportBase, data: &MetadataEnvelope) {
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

        let mut packages: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (prod, pkgvers) in data.packages.iter().flatten() {
            let mut pkgs = HashMap::new();
            for entry in pkgvers {
                // Upstream: `pkg, _, ver = p.split()` — first token is the
                // package name, third the version, middle token discarded.
                let mut tokens = entry.split_whitespace();
                if let (Some(pkg), Some(_), Some(ver)) =
                    (tokens.next(), tokens.next(), tokens.next())
                {
                    // Package names are interpolated into root remote commands.
                    // Reject anything that is not a valid RPM name at ingestion
                    // (lenient-load: log and skip, never hard-fail the load).
                    if let Err(e) = PackageSpec::parse(pkg) {
                        error!(package = %pkg, error = %e, "skipping invalid package name in metadata");
                        continue;
                    }
                    pkgs.insert(pkg.to_owned(), ver.to_owned());
                }
            }
            packages.insert(prod.clone(), pkgs);
        }
        results.packages = packages;

        results.repositories = data
            .repositories
            .clone()
            .map(|r| r.into_iter().collect())
            .unwrap_or_default();
    }
}

/// Maps `issue id -> title` from a checkout's `patchinfo.xml`.
///
/// Port of upstream `patchinfo_titles`. The JSON metadata envelope carries only
/// bare bug/jira *ids* (their descriptions are the [`NO_DESCRIPTION`]
/// placeholder); the human-readable titles live in the checkout's
/// `patchinfo.xml` as `<issue tracker="bnc" id="123">title</issue>` elements.
///
/// Best-effort: a missing or unparseable `patchinfo.xml` yields an empty map —
/// not every report kind ships one, and a malformed file must never break
/// loading.
#[must_use]
pub fn patchinfo_titles(directory: &Path) -> HashMap<String, String> {
    let pi = directory.join("patchinfo.xml");
    let Ok(content) = std::fs::read_to_string(&pi) else {
        return HashMap::new();
    };
    parse_patchinfo(&content).unwrap_or_default()
}

/// Parses `patchinfo.xml` content into an `id -> title` map.
///
/// Returns `None` on any XML error so the caller can degrade to an empty map,
/// mirroring upstream's `except ET.ParseError: return {}`.
fn parse_patchinfo(content: &str) -> Option<HashMap<String, String>> {
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut titles = HashMap::new();
    let mut buf = Vec::new();
    // The `id` attribute of the currently-open `<issue>` element, if any.
    let mut current_id: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"issue" => {
                current_id = issue_id(&e);
            }
            Ok(Event::Text(e)) => {
                if let Some(id) = &current_id
                    && let Ok(text) = e.decode()
                {
                    let title = text.trim();
                    if !title.is_empty() {
                        titles.insert(id.clone(), title.to_owned());
                    }
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"issue" => {
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
        if attr.key.local_name().as_ref() == b"id" {
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
