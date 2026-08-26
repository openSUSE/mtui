//! Metadata parsers for testreport sources:
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

use mtui_types::{PackageSpec, RequestReviewID, UpdateSource};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use regex::Regex;
use serde::Deserialize;
use tracing::{debug, error, warn};

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
/// Registered under the `"hosts"` key in each concrete report's parser
/// table; the report feeds it the field's lines one at a time. Each line is
/// matched against three patterns, in order, and the first match wins:
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
        // First-wins, like every other arm here: if a template somehow carries
        // several markers, the first is authoritative and the writer collapses
        // the rest, so read and write agree on which message the gate checks.
        if line.starts_with("Slack Review:") {
            if results.slack_review.is_none()
                && let Some(marker) = crate::testreport::SlackReviewMarker::parse_line(line)
            {
                results.slack_review = Some(marker);
            }
            return;
        }

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
/// Every field is optional so a partial envelope parses without error,
/// treating an absent key as empty. The field names map to the envelope
/// keys; where a Rust field name would clash with the report's own naming,
/// `#[serde(rename)]` restores the wire key.
///
/// Unmodelled keys are captured via `extra` and logged at `debug` so a
/// future field addition does not silently vanish (see #445). `deny_unknown_fields`
/// is intentionally not used — metadata gains fields over time and a hard
/// failure would break loading on every new field.
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
    /// Source RPM names (envelope key `SRCRPMs`).
    #[serde(default, rename = "SRCRPMs")]
    srcrpms: Option<Vec<String>>,
    /// Generation timestamp (SLFO only, e.g. `2026-07-21T19:33:33+0200`).
    #[serde(default)]
    generated_at: Option<String>,
    /// Product composer map (SLFO only, `package -> {media, pool_only, shipped}`).
    #[serde(default)]
    product_composer: Option<HashMap<String, serde_json::Value>>,
    /// Composed binary map (SLFO, `product -> arch -> [rpm filename]`).
    #[serde(default)]
    binaries: Option<HashMap<String, HashMap<String, Vec<String>>>>,
    /// Any envelope keys not modelled above, captured for `debug` logging.
    #[serde(default, flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// A parser for the JSON metadata envelope.
///
/// Stateless; `parse` mutates the supplied
/// [`TestReportBase`] in place.
pub struct JSONParser;

impl JSONParser {
    /// Parses a raw JSON string into a [`MetadataEnvelope`] and applies it.
    ///
    /// Convenience wrapper over `parse` for the common case
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
    /// Applies the envelope fields to `results`:
    ///
    /// * jira/bugs ids are seeded with the [`NO_DESCRIPTION`] placeholder;
    /// * `rrid` is parsed via [`RequestReviewID`] (an absent or malformed value
    ///   leaves the field `None`, degrading gracefully rather than panicking
    ///   on bad input);
    /// * scalar fields map straight through;
    /// * each package entry `"<name> <op> <version>"` — real metadata ships
    ///   `"afterburn = 5.10.0.git73.b97f772-99999_stage.1.1"` — is split on
    ///   whitespace, taking the first token as the package name and the third
    ///   as the version. The middle token is discarded, so the operator itself
    ///   is not significant.
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
                // `"<name> <op> <version>"`: first token is the package name,
                // third the version, middle token discarded.
                let mut tokens = entry.split_whitespace();
                match (tokens.next(), tokens.next(), tokens.next()) {
                    (Some(pkg), Some(_), Some(ver)) => {
                        if tokens.next().is_some() {
                            warn!(
                                product = %prod, entry = %entry,
                                "package entry has trailing tokens; using the first as name and third as version"
                            );
                        }
                        // Package names are interpolated into root remote
                        // commands. Reject anything that is not a valid RPM
                        // name at ingestion (lenient-load: log and skip, never
                        // hard-fail the load).
                        if let Err(e) = PackageSpec::parse(pkg) {
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
            // An empty sub-map must not be inserted: it makes `base.packages`
            // non-empty while the flattened `get_package_list()` is empty,
            // defeating every "is anything to install?" guard downstream (#396).
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

        results.srcrpms = data.srcrpms.clone().unwrap_or_default();
        results.generated_at = data.generated_at.clone();
        results.product_composer = data.product_composer.clone().unwrap_or_default();
        results.binaries = data.binaries.clone().unwrap_or_default();
        if !data.extra.is_empty() {
            debug!(
                unmodelled_keys = ?data.extra.keys().collect::<Vec<_>>(),
                "metadata envelope contains unmodelled keys"
            );
        }
    }
}

/// Maps `issue id -> title` from a checkout's `patchinfo.xml`.
///
/// The JSON metadata envelope carries only bare bug/jira *ids* (their
/// descriptions are the `NO_DESCRIPTION` placeholder); the human-readable
/// titles live in the checkout's `patchinfo.xml` as
/// `<issue tracker="bnc" id="123">title</issue>` elements.
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
/// Returns `None` on any XML error so the caller can degrade to an empty
/// map.
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

    /// F8: a `SUSE:SLFO:1.1:*` envelope carrying the hash resolves to `Git` —
    /// the dual-served case, where the RRID looks classic (`1.1`) and only the
    /// metadata tells the true story. This is the executable statement of the
    /// precedence rule; without it, the rule lives only in a doc comment.
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

    #[test]
    fn srcrpms_is_parsed_and_stored() {
        let mut results = base();
        JSONParser::parse_str(&mut results, r#"{"SRCRPMs": ["afterburn", "foo"]}"#).unwrap();
        assert_eq!(results.srcrpms, vec!["afterburn", "foo"]);
        // Absent key stays empty, not None.
        let mut empty = base();
        JSONParser::parse_str(&mut empty, "{}").unwrap();
        assert!(empty.srcrpms.is_empty());
    }

    #[test]
    fn generated_at_product_composer_and_binaries_are_parsed() {
        let mut results = base();
        JSONParser::parse_str(
            &mut results,
            r#"{
                "generated_at": "2026-07-21T19:33:33+0200",
                "product_composer": {"afterburn": {"media": [], "pool_only": false, "shipped": false}},
                "binaries": {"SL-Micro-6.1": {"x86_64": ["afterburn-1.0-1.x86_64.rpm"]}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            results.generated_at.as_deref(),
            Some("2026-07-21T19:33:33+0200")
        );
        assert!(results.product_composer.contains_key("afterburn"));
        assert_eq!(
            results.binaries["SL-Micro-6.1"]["x86_64"],
            vec!["afterburn-1.0-1.x86_64.rpm"]
        );
    }

    #[test]
    fn slfo_fixture_fields_are_not_silently_discarded() {
        // The checked-in SLFO fixture carries SRCRPMs, generated_at and
        // product_composer; before #445 they were silently discarded (no
        // trace, no error). This pins they now survive ingestion.
        let data = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/metadata/slfo_metadata.json"
        ))
        .unwrap();
        let mut results = base();
        JSONParser::parse_str(&mut results, &data).unwrap();
        assert_eq!(results.srcrpms, vec!["afterburn"]);
        assert_eq!(
            results.generated_at.as_deref(),
            Some("2026-07-21T19:33:33+0200")
        );
        assert!(results.product_composer.contains_key("afterburn"));
    }

    #[test]
    fn unknown_metadata_keys_do_not_break_parsing() {
        // Unknown keys are captured via `extra` and logged at debug, not hard-failed.
        // `deny_unknown_fields` would break on every new TeReGen field (#445).
        let mut results = base();
        JSONParser::parse_str(
            &mut results,
            r#"{"SRCRPMs": ["a"], "unknown_future_field": 123, "another": {"x": 1}}"#,
        )
        .unwrap();
        assert_eq!(results.srcrpms, vec!["a"]);
    }
}
