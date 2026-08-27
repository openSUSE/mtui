//! Public entry points and supporting helpers for the openQA / QAM Dashboard
//! search.
//!
//! The three high-level entry points are [`single_incidents`],
//! [`aggregated_updates`], and [`build_checks`]. Each returns a list of typed
//! result rows (defined in [`super::results`]) that the command layer renders.
//!
//! The openQA job-group list is fetched per call rather than memoised: that is
//! a per-process performance detail, not a behavioural contract. The build-check
//! directory-index scan uses the `scraper` HTML parser, with a golden test
//! pinning the extracted `.log` set so parser drift is caught.

use std::collections::{BTreeSet, HashMap};

use futures::stream::{self, StreamExt};
use reqwest::Method;
use ruoqa::consts::JobResult as OqaJobResult;
use scraper::{Html, Selector};

use crate::error::OqaSearchError;
use crate::http::{HttpClient, MAX_API_BODY, sanitize_url};

use super::heuristics::{
    AGGREGATED_EXCLUDED_VERSIONS, AGGREGATED_GROUPS_TERMS, AGGREGATED_NAME_MAP, EXCLUDED_GROUPS,
    MICRO_TEMPLATE_IDENTIFIER, PYTHON_FLAVOR_RE, SINGLE_INCIDENTS_TERMS, TESTSUITE_NUMBERS_PATTERN,
    TESTSUITE_SUMMARY_KEYWORDS, TESTSUITE_SUMMARY_PATTERNS, TESTSUITE_VISUAL_SEPARATORS,
    TESTSUITE_WORDS_BLOCKLIST, oqa_query_string,
};
use super::results::{BuildCheckResult, GroupResult, JobResult, VersionResult};

// --- JSON fetch helpers over the shared HttpClient ---

/// GET `url` and parse the body as JSON.
///
/// Any transport, non-2xx, or JSON-parse failure collapses onto
/// [`OqaSearchError::Http`] so callers can convert into a user-facing message
/// (or fold it into a note / empty result, as the entry points do).
async fn get_json(http: &HttpClient, url: &str) -> Result<serde_json::Value, OqaSearchError> {
    let bytes = http.get_bytes_capped(url, MAX_API_BODY).await?;
    serde_json::from_slice(&bytes).map_err(|e| OqaSearchError::Http(e.to_string()))
}

/// GET `url` and return the body as UTF-8 text.
async fn fetch_url_content(http: &HttpClient, url: &str) -> Result<String, OqaSearchError> {
    let bytes = http.get_bytes_capped(url, MAX_API_BODY).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// GET `path` (relative to `oqa`'s base URL) and return the parsed JSON body.
///
/// `ruoqa::Client::request` parses the response and enforces the client's
/// `max_response_bytes` cap, so unlike [`get_json`] there is no separate
/// byte-fetch step.
async fn oqa_request(oqa: &ruoqa::Client, path: &str) -> Result<serde_json::Value, OqaSearchError> {
    Ok(oqa.request(Method::GET, path, None).await?)
}

// --- Incident info (Dashboard) ---

/// Get the incident build name and the affected SLE versions.
///
/// Returns `(build, versions)`. `versions` is `None` when no openQA builds exist
/// for the incident yet.
///
/// # Errors
///
/// Returns [`OqaSearchError::Http`] if the Dashboard cannot be reached.
pub async fn get_incident_info(
    http: &HttpClient,
    url_dashboard_qam: &str,
    incident_id: &str,
) -> Result<(String, Option<Vec<String>>), OqaSearchError> {
    let url = format!("{url_dashboard_qam}/api/incident_settings/{incident_id}");
    let incident_settings = get_json(http, &url).await?;

    let settings = incident_settings.as_array();
    let Some(settings) = settings.filter(|s| !s.is_empty()) else {
        return Ok((
            fallback_build(http, url_dashboard_qam, incident_id).await?,
            None,
        ));
    };

    let build = settings
        .first()
        .and_then(|s| s.get("settings"))
        .and_then(|s| s.get("BUILD"))
        .and_then(|b| b.as_str());
    let Some(build) = build else {
        return Ok((
            fallback_build(http, url_dashboard_qam, incident_id).await?,
            None,
        ));
    };
    let build = build.to_string();

    let mut raw_versions: BTreeSet<String> = BTreeSet::new();
    for entry in settings {
        let entry_settings = entry.get("settings");
        let distri = entry_settings
            .and_then(|s| s.get("DISTRI"))
            .and_then(|d| d.as_str());
        if distri != Some("sle") {
            continue;
        }
        let version = entry.get("version").and_then(|v| v.as_str()).unwrap_or("");
        let flavor = entry.get("flavor").and_then(|f| f.as_str()).unwrap_or("");
        let label = if flavor.contains("TERADATA") {
            format!("{version}-TERADATA")
        } else {
            version.to_string()
        };
        raw_versions.insert(label);
    }

    let versions: Vec<String> = raw_versions.into_iter().collect();
    Ok((
        build,
        if versions.is_empty() {
            None
        } else {
            Some(versions)
        },
    ))
}

/// Synthesise a build name from `/api/incidents/<id>` when no builds exist yet.
async fn fallback_build(
    http: &HttpClient,
    url_dashboard_qam: &str,
    incident_id: &str,
) -> Result<String, OqaSearchError> {
    let url = format!("{url_dashboard_qam}/api/incidents/{incident_id}");
    let incident_info = get_json(http, &url).await?;
    let package = incident_info
        .get("packages")
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.as_str())
        .unwrap_or("");
    Ok(format!(":{incident_id}:{package}"))
}

/// List the individual openQA jobs for an incident `build`.
///
/// `obsoleted` jobs (superseded by a later retrigger) are dropped unless
/// `include_obsoleted` is set. Returns the jobs sorted by result then arch then
/// scenario. Empty when `build` is empty or no jobs exist.
///
/// # Errors
///
/// Returns [`OqaSearchError::Http`] if openQA cannot be reached.
pub async fn incident_jobs(
    oqa: &ruoqa::Client,
    build: &str,
    include_obsoleted: bool,
) -> Result<Vec<JobResult>, OqaSearchError> {
    if build.is_empty() {
        return Ok(vec![]);
    }
    let encoded = urlencoding::encode(build);
    let data = oqa_request(oqa, &format!("/api/v1/jobs?build={encoded}")).await?;
    let jobs = data.get("jobs").and_then(|j| j.as_array());
    let Some(jobs) = jobs else {
        return Ok(vec![]);
    };

    let openqa_trimmed = oqa.base_url().as_str().trim_end_matches('/').to_owned();
    let mut rows: Vec<JobResult> = Vec::new();
    for job in jobs {
        let result = job.get("result").and_then(|r| r.as_str()).unwrap_or("");
        let is_obsoleted = job
            .get("result")
            .cloned()
            .and_then(|v| serde_json::from_value::<OqaJobResult>(v).ok())
            == Some(OqaJobResult::Obsoleted);
        if is_obsoleted && !include_obsoleted {
            continue;
        }
        let settings = job.get("settings");
        let test = job
            .get("test")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .or_else(|| job.get("name").and_then(|n| n.as_str()))
            .unwrap_or("")
            .to_string();
        let arch = settings
            .and_then(|s| s.get("ARCH"))
            .and_then(|a| a.as_str())
            .filter(|a| !a.is_empty())
            .or_else(|| job.get("arch").and_then(|a| a.as_str()))
            .unwrap_or("")
            .to_string();
        let id = job
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let state = job.get("state").and_then(|s| s.as_str()).unwrap_or("");
        rows.push(JobResult {
            job_id: id,
            test,
            arch,
            result: result.to_string(),
            state: state.to_string(),
            group: job
                .get("group")
                .and_then(|g| g.as_str())
                .unwrap_or("")
                .to_string(),
            url: format!("{openqa_trimmed}/t{id}"),
        });
    }
    rows.sort_by(|a, b| {
        (a.result.as_str(), a.arch.as_str(), a.test.as_str()).cmp(&(
            b.result.as_str(),
            b.arch.as_str(),
            b.test.as_str(),
        ))
    });
    Ok(rows)
}

// --- openQA job-group enumeration ---

/// Fetch the full openQA job-group list for a host.
async fn fetch_openqa_groups(
    oqa: &ruoqa::Client,
) -> Result<Vec<serde_json::Value>, OqaSearchError> {
    let data = oqa_request(oqa, "/api/v1/job_groups").await?;
    Ok(data.as_array().cloned().unwrap_or_default())
}

/// A job-group has a usable template that is not a SLE-Micro one.
fn is_valid_template(group: &serde_json::Value) -> bool {
    let template = group.get("template").and_then(|t| t.as_str());
    matches!(template, Some(t) if !t.is_empty() && !t.contains(MICRO_TEMPLATE_IDENTIFIER))
}

/// A job-group name matches one of `match_terms` and none of `excluded_terms`.
fn is_name_matching(
    group: &serde_json::Value,
    match_terms: &[&str],
    excluded_terms: &[&str],
) -> bool {
    let name = group.get("name").and_then(|n| n.as_str()).unwrap_or("");
    match_terms.iter().any(|t| name.contains(t)) && !excluded_terms.iter().any(|t| name.contains(t))
}

/// Extract a normalised SLE version label from a job-group name.
fn extract_version(name: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    static SPACE_FORM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)\s*SP\s*(\d+)(?:\s+TERADATA)?").unwrap());
    static HYPHEN_FORM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)-SP\d+(?:-TERADATA)?").unwrap());
    static DOT_FORM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+\.\d+)").unwrap());

    if let Some(caps) = SPACE_FORM.captures(name) {
        let base = format!("{}-SP{}", &caps[1], &caps[2]);
        return if name.contains("TERADATA") {
            format!("{base}-TERADATA")
        } else {
            base
        };
    }
    if let Some(m) = HYPHEN_FORM.find(name) {
        return m.as_str().to_string();
    }
    if let Some(caps) = DOT_FORM.captures(name) {
        return caps[1].to_string();
    }
    String::new()
}

/// Map an aggregated job-group name to its short key.
fn extract_aggregated_name(name: &str) -> String {
    for (label, key) in AGGREGATED_NAME_MAP {
        if name.contains(label) {
            return (*key).to_string();
        }
    }
    name.split_whitespace()
        .next()
        .map(str::to_lowercase)
        .unwrap_or_default()
}

/// Filter openQA job groups, keyed by the extractor's output.
fn filter_openqa_groups(
    groups: &[serde_json::Value],
    match_text: &[&str],
    excluded_terms: &[&str],
    name_extractor: impl Fn(&str) -> String,
) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    for group in groups {
        if !(is_name_matching(group, match_text, excluded_terms) && is_valid_template(group)) {
            continue;
        }
        let name = group.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let id = group
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        out.insert(name_extractor(name), id);
    }
    out
}

/// Single-Incidents / Core job groups keyed by SLE version.
fn incident_groups(groups: &[serde_json::Value]) -> HashMap<String, i64> {
    filter_openqa_groups(groups, SINGLE_INCIDENTS_TERMS, EXCLUDED_GROUPS, |n| {
        extract_version(n)
    })
}

/// Aggregated-Updates job groups keyed by short name (core, sap, ...).
fn aggregated_groups(groups: &[serde_json::Value]) -> HashMap<String, i64> {
    filter_openqa_groups(groups, AGGREGATED_GROUPS_TERMS, EXCLUDED_GROUPS, |n| {
        extract_aggregated_name(n)
    })
}

/// Resolve a SLE version or aggregated-group name to its openQA group id.
/// Returns `None` when the key is unknown.
fn get_group_id(groups: &[serde_json::Value], key: &str) -> Option<i64> {
    incident_groups(groups)
        .get(key)
        .copied()
        .or_else(|| aggregated_groups(groups).get(key).copied())
}

// --- openQA job lookups ---

/// Browser-facing openQA overview URL.
fn openqa_print_url(base_url: &str, version: &str, build: &str, group_id: i64) -> String {
    format!(
        "{base_url}/tests/overview?distri=sle&version={version}&build={build}&groupid={group_id}"
    )
}

/// API path (relative to the client's base URL) for filtered openQA jobs in a
/// build. Returns `None` for an unknown state.
fn openqa_build_path(state: &str, version: &str, build: &str, group_id: i64) -> Option<String> {
    let suffix = oqa_query_string(state)?;
    Some(format!(
        "/api/v1/jobs/overview?distri=sle&version={version}&build={build}&groupid={group_id}{suffix}"
    ))
}

/// Set of incident IDs being tested by an openQA job.
async fn openqa_job_issues(
    oqa: &ruoqa::Client,
    job_id: i64,
) -> Result<BTreeSet<i64>, OqaSearchError> {
    let response = oqa_request(oqa, &format!("/api/v1/jobs/{job_id}")).await?;
    let settings = response
        .get("job")
        .and_then(|j| j.get("settings"))
        .and_then(|s| s.as_object());
    let mut issues = BTreeSet::new();
    if let Some(settings) = settings {
        for (k, v) in settings {
            if k.to_uppercase().contains("_TEST_ISSUES") {
                let raw = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                for part in raw.split(',') {
                    let part = part.trim();
                    if let Ok(n) = part.parse::<i64>() {
                        issues.insert(n);
                    }
                }
            }
        }
    }
    Ok(issues)
}

/// The current UTC date, as a `chrono::NaiveDate`.
///
/// The workspace pins `chrono` without the `clock` feature to keep the
/// single-static-binary contract lean, so "today" comes from
/// [`std::time::SystemTime`] in UTC rather than local time. The two differ only
/// in the UTC-offset window around midnight, where the day-walk starts one day
/// over — which the N-day window absorbs.
fn current_utc_date() -> chrono::NaiveDate {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.date_naive())
        .unwrap_or_default()
}

/// Count the jobs in a `/jobs/overview` response array.
fn overview_len(value: &serde_json::Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

/// Resolve PASSED / FAILED / RUNNING for one openQA build.
async fn query_version_status(
    oqa: &ruoqa::Client,
    version: &str,
    build: &str,
    group_id: i64,
) -> Result<VersionResult, OqaSearchError> {
    // Workaround: TERADATA jobs share the base version's URL.
    let version_oqa = if version == "12-SP3-TERADATA" {
        "12-SP3"
    } else {
        version
    };

    // `ruoqa`'s resolved `base_url` always carries a trailing `/`; trim it so
    // the browser URL does not double the slash.
    let base_url = oqa.base_url().as_str().trim_end_matches('/');
    let print_url = openqa_print_url(base_url, version_oqa, build, group_id);
    // These states are always valid; the .expect documents that invariant.
    let running_path = openqa_build_path("running", version_oqa, build, group_id)
        .expect("running is a valid state");
    let failed_path =
        openqa_build_path("failed", version_oqa, build, group_id).expect("failed is a valid state");

    // Independent, and both always needed; the precedence below applies to the
    // results, so concurrency does not change the outcome.
    let (running_results, failed_results) = tokio::join!(
        oqa_request(oqa, &running_path),
        oqa_request(oqa, &failed_path)
    );
    let running_results = running_results?;
    let failed_results = failed_results?;

    let failed = overview_len(&failed_results);
    if failed > 0 {
        return Ok(VersionResult {
            version: version.to_string(),
            url: print_url,
            status: "failed".to_string(),
            failed_count: failed,
            ..Default::default()
        });
    }
    let running = overview_len(&running_results);
    if running > 0 {
        return Ok(VersionResult {
            version: version.to_string(),
            url: print_url,
            status: "running".to_string(),
            running_count: running,
            ..Default::default()
        });
    }
    Ok(VersionResult {
        version: version.to_string(),
        url: print_url,
        status: "passed".to_string(),
        ..Default::default()
    })
}

// --- Build-check log parsing ---

/// Collect `.log` link targets from an HTML directory index. Hrefs are
/// percent-decoded before comparison and returned decoded.
fn parse_log_links(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    // A malformed selector is a programming error, not runtime input.
    let selector = Selector::parse("a[href]").expect("valid selector");
    let mut out = Vec::new();
    for element in document.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            let decoded =
                urlencoding::decode(href).map_or_else(|_| href.to_string(), |c| c.into_owned());
            if decoded.ends_with(".log") {
                out.push(decoded);
            }
        }
    }
    out
}

/// Extract test-summary lines from a build-check log.
///
/// With `custom_pattern` the user's regex wins (invalid regex logs a warning and
/// returns empty). Otherwise a multi-stage heuristic decides.
#[must_use]
pub fn extract_test_results(log_text: &str, custom_pattern: Option<&str>) -> Vec<String> {
    use regex::RegexBuilder;
    use std::sync::LazyLock;

    static OBS_PREFIX: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^\[\s*\d+s\]\s*").unwrap());

    if let Some(pat) = custom_pattern {
        let re = match RegexBuilder::new(pat).case_insensitive(true).build() {
            Ok(re) => re,
            Err(e) => {
                tracing::warn!("Invalid regex pattern {pat:?}: {e}");
                return vec![];
            }
        };
        return log_text
            .lines()
            .filter(|line| re.is_match(line))
            .map(str::to_string)
            .collect();
    }

    let mut matches = Vec::new();
    for line in log_text.lines() {
        let clean_line = OBS_PREFIX.replace(line, "");
        let lower = clean_line.to_lowercase();
        let lower = lower.trim();

        if !TESTSUITE_NUMBERS_PATTERN.is_match(lower) {
            continue;
        }
        if TESTSUITE_WORDS_BLOCKLIST.iter().any(|b| lower.contains(b)) {
            continue;
        }
        if TESTSUITE_VISUAL_SEPARATORS
            .iter()
            .any(|s| clean_line.contains(s))
        {
            matches.push(line.to_string());
            continue;
        }
        if TESTSUITE_SUMMARY_KEYWORDS.iter().any(|k| lower.contains(k)) {
            matches.push(line.to_string());
            continue;
        }
        if TESTSUITE_SUMMARY_PATTERNS.iter().any(|p| p.is_match(lower)) {
            matches.push(line.to_string());
        }
    }
    matches
}

/// Collapse a long match list into a one-line summary.
#[must_use]
pub fn summarize_test_results(lines: &[String]) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    static PASS_A: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(\d+) pass(?:ed)?").unwrap());
    static FAIL_A: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(\d+) fail(?:ed)?").unwrap());
    static PASS_B: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)#\s*pass(?:ed)?:?\s*(\d+)").unwrap());
    static FAIL_B: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)#\s*fail(?:ed)?:?\s*(\d+)").unwrap());

    let mut total_passed: u64 = 0;
    let mut total_failed: u64 = 0;
    let middle = if lines.len() > 2 {
        &lines[1..lines.len() - 1]
    } else {
        &[][..]
    };
    for line in middle {
        let passed = PASS_A
            .captures(line)
            .or_else(|| PASS_B.captures(line))
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok());
        let failed = FAIL_A
            .captures(line)
            .or_else(|| FAIL_B.captures(line))
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u64>().ok());
        if let Some(p) = passed {
            total_passed += p;
        }
        if let Some(f) = failed {
            total_failed += f;
        }
    }
    let more = lines.len().saturating_sub(2);
    format!("({more} more results, {total_passed} passed, {total_failed} failed)")
}

/// Return `true` if `log` belongs to any package in `packages`, including via
/// the flavored-Python normalisation.
#[must_use]
fn log_matches_package(log: &str, packages: &[String]) -> bool {
    for pkg in packages {
        if log.contains(pkg.as_str()) {
            return true;
        }
        let normalized = PYTHON_FLAVOR_RE.replace(pkg, "python-");
        if log.contains(normalized.as_ref()) && normalized.as_ref() != pkg.as_str() {
            return true;
        }
    }
    false
}

// --- Public entry points used by the command layer ---

/// Resolve openQA status for each SLE version of a single incident.
///
/// Never fails as a whole: an unknown version or an openQA query failure is
/// recorded as a `failed` row with a note, so a flaky openQA cannot abort the
/// command.
pub async fn single_incidents(
    oqa: &ruoqa::Client,
    build: &str,
    versions: &[String],
    max_parallel: usize,
) -> Vec<VersionResult> {
    let groups = match fetch_openqa_groups(oqa).await {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("openQA job-group fetch failed: {e}");
            Vec::new()
        }
    };

    // Resolve each version's group id up front (a pure lookup), then fan the
    // queries out concurrently under a bound, carrying the input index so the
    // output order is unchanged. Inputs are cloned into each future to keep them
    // `'static`-friendly when nested in another async context.
    let jobs: Vec<(usize, String, Option<i64>)> = versions
        .iter()
        .enumerate()
        .map(|(idx, version)| (idx, version.clone(), get_group_id(&groups, version)))
        .collect();
    let build = build.to_owned();
    let mut indexed: Vec<(usize, VersionResult)> = stream::iter(jobs)
        .map(|(idx, version, group_id)| {
            let build = build.clone();
            // `ruoqa::Client` is `Arc`-backed: cloning shares the pool.
            let oqa = oqa.clone();
            async move {
                let Some(group_id) = group_id else {
                    let note = format!(
                        "Not a valid version (single incident) or group (aggregated updates): {version}"
                    );
                    tracing::warn!("{note}");
                    return (
                        idx,
                        VersionResult {
                            version,
                            status: "failed".to_string(),
                            note,
                            ..Default::default()
                        },
                    );
                };
                let row = match query_version_status(&oqa, &version, &build, group_id).await {
                    Ok(row) => row,
                    Err(e) => VersionResult {
                        version,
                        status: "failed".to_string(),
                        note: format!("openQA query failed: {e}"),
                        ..Default::default()
                    },
                };
                (idx, row)
            }
        })
        .buffer_unordered(max_parallel.max(1))
        .collect()
        .await;

    indexed.sort_by_key(|(idx, _)| *idx);
    indexed.into_iter().map(|(_, row)| row).collect()
}

/// Walk the last `days` days of aggregated builds for each group.
pub async fn aggregated_updates(
    oqa: &ruoqa::Client,
    incident_id: &str,
    versions: &[String],
    days: u32,
    groups_wanted: &[String],
    max_parallel: usize,
) -> Vec<GroupResult> {
    let filtered_versions: Vec<String> = versions
        .iter()
        .filter(|v| !AGGREGATED_EXCLUDED_VERSIONS.iter().any(|e| v.contains(e)))
        .cloned()
        .collect();

    if filtered_versions.is_empty() {
        return vec![];
    }

    let incident_id_int: Option<i64> = incident_id.parse().ok();

    let groups = match fetch_openqa_groups(oqa).await {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("openQA job-group fetch failed: {e}");
            Vec::new()
        }
    };

    // Valid groups first, in `groups_wanted` order (invalid ones warn and are
    // skipped), so each survivor has a stable output position.
    let mut valid_groups: Vec<(String, i64)> = Vec::new();
    for group in groups_wanted {
        let Some(group_id) = get_group_id(&groups, group) else {
            tracing::warn!(
                "Not a valid version (single incident) or group (aggregated updates): {group}"
            );
            continue;
        };
        valid_groups.push((group.clone(), group_id));
    }

    // The (group, version) day-scans are independent, so fan them out under a
    // bound — each is still early-exit sequential internally — and restore the
    // grouped output order from the index pair.
    let jobs: Vec<(usize, usize, String, i64)> = valid_groups
        .iter()
        .enumerate()
        .flat_map(|(gi, (_, group_id))| {
            let group_id = *group_id;
            filtered_versions
                .iter()
                .enumerate()
                .map(move |(vi, version)| (gi, vi, version.clone(), group_id))
        })
        .collect();
    let mut scanned: Vec<(usize, usize, VersionResult)> = stream::iter(jobs)
        .map(|(gi, vi, version, group_id)| {
            let oqa = oqa.clone();
            async move {
                let row =
                    scan_aggregated_for_version(&oqa, &version, days, group_id, incident_id_int)
                        .await;
                (gi, vi, row)
            }
        })
        .buffer_unordered(max_parallel.max(1))
        .collect()
        .await;

    scanned.sort_by_key(|(gi, vi, _)| (*gi, *vi));

    let mut results: Vec<GroupResult> = valid_groups
        .into_iter()
        .map(|(group, _)| GroupResult {
            group,
            versions: Vec::with_capacity(filtered_versions.len()),
        })
        .collect();
    for (gi, _, row) in scanned {
        results[gi].versions.push(row);
    }
    results
}

/// Find the most recent aggregated build covering the incident.
async fn scan_aggregated_for_version(
    oqa: &ruoqa::Client,
    version: &str,
    days: u32,
    group_id: i64,
    incident_id: Option<i64>,
) -> VersionResult {
    let now = current_utc_date();
    for i in 0..days {
        let day = now - chrono::Duration::days(i64::from(i));
        let build = format!("{}-1", day.format("%Y%m%d"));
        let Some(job_path) = openqa_build_path("all", version, &build, group_id) else {
            continue;
        };
        let jobs = match oqa_request(oqa, &job_path).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("aggregated query for {version}/{build} failed: {e}");
                continue;
            }
        };
        let jobs_arr = jobs.as_array();
        let Some(first) = jobs_arr.and_then(|a| a.first()) else {
            continue;
        };
        let Some(job_id) = first.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let issues = match openqa_job_issues(oqa, job_id).await {
            Ok(i) => i,
            Err(_) => continue,
        };

        if incident_id.is_none() || incident_id.is_some_and(|id| issues.contains(&id)) {
            return match query_version_status(oqa, version, &build, group_id).await {
                Ok(row) => row,
                Err(e) => VersionResult {
                    version: version.to_string(),
                    status: "failed".to_string(),
                    note: format!("openQA query failed: {e}"),
                    ..Default::default()
                },
            };
        }
    }

    VersionResult {
        version: version.to_string(),
        status: "missing".to_string(),
        note: format!("No aggregated updates build for this incident in the last {days} days"),
        ..Default::default()
    }
}

/// Parse the qam.suse.de build_checks index and extract summaries.
///
/// A missing index (or an unreadable log) is not an error — it yields no (or a
/// bare) entry, so a flaky QAM host cannot abort the command.
// One internal caller (`openqa_overview`) passes these positionally, so a
// params struct would be ceremony without a readability win.
#[allow(clippy::too_many_arguments)]
pub async fn build_checks(
    http: &HttpClient,
    product: &str,
    incident_id: &str,
    request_id: i64,
    packages: &[String],
    url_qam: &str,
    test_pattern: Option<&str>,
    max_parallel: usize,
) -> Vec<BuildCheckResult> {
    let base_url =
        format!("{url_qam}/testreports/SUSE:{product}:{incident_id}:{request_id}/build_checks");

    let html_text = match fetch_url_content(http, &base_url).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("build_checks index unavailable: {e}");
            return vec![];
        }
    };

    let all_links = parse_log_links(&html_text);
    let logfiles: Vec<String> = all_links
        .into_iter()
        .filter(|log| log_matches_package(log, packages))
        .collect();

    if logfiles.is_empty() {
        tracing::warn!("No build check logs found for packages {packages:?}");
        return vec![];
    }

    // Fetch + parse each matching log under a concurrency bound, carrying the
    // input index so the output keeps link order.
    let test_pattern = test_pattern.map(str::to_owned);
    let mut indexed: Vec<(usize, BuildCheckResult)> =
        stream::iter(logfiles.into_iter().enumerate())
            .map(|(idx, log)| {
                let base_url = base_url.clone();
                let test_pattern = test_pattern.clone();
                async move {
                    let log_url = format!("{base_url}/{}", urlencoding::encode(&log));
                    let log_text = match fetch_url_content(http, &log_url).await {
                        Ok(t) => t,
                        Err(e) => {
                            // `log_url` comes verbatim from `[url] testreports`
                            // and may embed credentials; this line is WARN, so
                            // visible by default (#431).
                            tracing::warn!(
                                "build_check log {} unavailable: {e}",
                                sanitize_url(&log_url)
                            );
                            return (
                                idx,
                                BuildCheckResult {
                                    url: log_url,
                                    ..Default::default()
                                },
                            );
                        }
                    };
                    let mut matches = extract_test_results(&log_text, test_pattern.as_deref());
                    let mut summary = String::new();
                    if matches.len() > 4 {
                        summary = summarize_test_results(&matches);
                        matches = vec![matches[0].clone(), matches[matches.len() - 1].clone()];
                    }
                    (
                        idx,
                        BuildCheckResult {
                            url: log_url,
                            matches,
                            summary,
                        },
                    )
                }
            })
            .buffer_unordered(max_parallel.max(1))
            .collect()
            .await;

    indexed.sort_by_key(|(idx, _)| *idx);
    indexed.into_iter().map(|(_, entry)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group(id: i64, name: &str, template: &str) -> serde_json::Value {
        json!({"id": id, "name": name, "template": template})
    }

    // --- extract_version ---

    #[test]
    fn extract_version_handles_all_three_forms() {
        assert_eq!(extract_version("Maintenance: 12-SP5"), "12-SP5");
        assert_eq!(
            extract_version("Maintenance: 12 SP3 TERADATA"),
            "12-SP3-TERADATA"
        );
        assert_eq!(
            extract_version("Maintenance: 15-SP4-TERADATA"),
            "15-SP4-TERADATA"
        );
        assert_eq!(extract_version("SLES 16.0 Maintenance Updates"), "16.0");
        assert_eq!(extract_version("no version here"), "");
    }

    #[test]
    fn extract_aggregated_name_maps_and_falls_back() {
        assert_eq!(
            extract_aggregated_name("Public Cloud Maintenance Updates"),
            "cloud"
        );
        assert_eq!(extract_aggregated_name("SAP/HA Maintenance Updates"), "sap");
        assert_eq!(
            extract_aggregated_name("Core Maintenance Updates 15-SP5"),
            "core"
        );
    }

    // --- is_valid_template ---

    #[test]
    fn is_valid_template_filters_micro_and_empty() {
        assert!(!is_valid_template(&group(1, "n", "sle-micro-2")));
        assert!(!is_valid_template(&json!({"id": 1, "name": "n"})));
        assert!(!is_valid_template(&group(1, "n", "")));
        assert!(is_valid_template(&group(1, "n", "sle-15")));
        assert!(is_valid_template(&group(1, "n", "sometext")));
    }

    // --- is_name_matching (single-incidents + aggregated tables) ---

    #[test]
    fn is_name_matching_single_incidents() {
        let cases = [
            ("Maintenance: SLE 15 SP6 Core Incidents - DEV", false),
            ("Maintenance: Leap 15.6 Core Incidents", false),
            ("Maintenance: SLEM 5.4 Incidents", false),
            ("Maintenance: SLE 12 SP5 Core Incidents", true),
        ];
        for (name, expected) in cases {
            let g = group(1, name, "tpl");
            assert_eq!(
                is_name_matching(&g, SINGLE_INCIDENTS_TERMS, EXCLUDED_GROUPS),
                expected,
                "name: {name}"
            );
        }
    }

    #[test]
    fn is_name_matching_aggregated_updates() {
        let cases = [
            ("YaST Maintenance Updates - Development", false),
            (
                "Maintenance: SLE Micro / Public Cloud Maintenance Updates",
                false,
            ),
            ("Core Wicked Maintenance Updates", false),
            ("Helm Chart required Images", false),
        ];
        for (name, expected) in cases {
            let g = group(1, name, "tpl");
            assert_eq!(
                is_name_matching(&g, AGGREGATED_GROUPS_TERMS, EXCLUDED_GROUPS),
                expected,
                "name: {name}"
            );
        }
    }

    // --- filter_openqa_groups ---

    #[test]
    fn filter_openqa_groups_single_incidents() {
        let groups = vec![
            group(123, "Whatever Core Incidents", "sle-micro-testing"),
            group(321, "Wicked Core Incidents", "tpl"),
            group(282, "Maintenance: SLE 12 SP5 Core Incidents", "tpl"),
            group(546, "Maintenance: SLE 15 SP6 Core Incidents", "tpl"),
        ];
        let out = filter_openqa_groups(&groups, SINGLE_INCIDENTS_TERMS, EXCLUDED_GROUPS, |n| {
            extract_version(n)
        });
        let mut expected = HashMap::new();
        expected.insert("12-SP5".to_string(), 282);
        expected.insert("15-SP6".to_string(), 546);
        assert_eq!(out, expected);
    }

    #[test]
    fn filter_openqa_groups_aggregated() {
        let groups = vec![
            group(1, "YaST Maintenance Updates - Development", "tpl"),
            group(
                2,
                "Maintenance: SLE Micro / Public Cloud Maintenance Updates",
                "tpl",
            ),
            group(222, "Public Cloud Maintenance Updates", "tpl"),
            group(333, "Core Maintenance Updates", "tpl"),
        ];
        let out = filter_openqa_groups(&groups, AGGREGATED_GROUPS_TERMS, EXCLUDED_GROUPS, |n| {
            extract_aggregated_name(n)
        });
        let mut expected = HashMap::new();
        expected.insert("cloud".to_string(), 222);
        expected.insert("core".to_string(), 333);
        assert_eq!(out, expected);
    }

    // --- log_matches_package (parametrized table) ---

    #[test]
    fn log_matches_package_table() {
        let cases: &[(&str, &[&str], bool)] = &[
            ("bash.x86_64.log", &["bash"], true),
            ("python-ecdsa.x86_64.log", &["python313-ecdsa"], true),
            ("python-ecdsa.log", &["python38-ecdsa"], true),
            ("python-ecdsa.log", &["bash", "python311-ecdsa"], true),
            ("python-ecdsa.log", &["python-rsa"], false),
            ("python-foo.log", &["python3-foo"], true),
            ("python-tornado.x86_64.log", &["python3-tornado"], true),
            ("python-ecdsa.log", &[], false),
        ];
        for (log, packages, expected) in cases {
            let pkgs: Vec<String> = packages.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(
                log_matches_package(log, &pkgs),
                *expected,
                "log: {log}, packages: {packages:?}"
            );
        }
    }

    // --- extract_test_results custom pattern ---

    #[test]
    fn extract_test_results_custom_pattern_overrides_heuristics() {
        let log = "the syntax of make matters\nfoo: 3 widgets\nbar: 7 widgets";
        let out = extract_test_results(log, Some(r"\d+ widgets"));
        assert_eq!(out, vec!["foo: 3 widgets", "bar: 7 widgets"]);
    }

    #[test]
    fn extract_test_results_bad_regex_returns_empty() {
        assert!(extract_test_results("anything", Some("[unclosed")).is_empty());
    }

    // --- summarize_test_results (parametrized table) ---

    #[test]
    fn summarize_test_results_counts_passed_and_failed() {
        let lines: Vec<String> = [
            "first line (ignored)",
            "5 passed",
            "3 failed, 2 passed",
            "last line (ignored)",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let summary = summarize_test_results(&lines);
        assert!(summary.contains("2 more results"));
        assert!(summary.contains("7 passed"));
        assert!(summary.contains("3 failed"));
    }

    #[test]
    fn summarize_test_results_parametrized() {
        let cases: &[(&[&str], &str)] = &[
            (
                &["First line", "100 passed", "50 failed", "Last line"],
                "(2 more results, 100 passed, 50 failed)",
            ),
            (
                &[
                    "First line",
                    "10 pass",
                    "5 fail",
                    "20 pass",
                    "3 fail",
                    "Last line",
                ],
                "(4 more results, 30 passed, 8 failed)",
            ),
            (
                &[
                    "[  949s] # TOTAL: 2901",
                    "[  949s] # PASS:  2709",
                    "[  949s] # SKIP:  151",
                    "[  949s] # XFAIL: 0",
                    "[  949s] # FAIL:  2",
                    "[  949s] # XPASS: 0",
                    "[  949s] # ERROR: 0",
                    "[  949s] make[1]: Leaving directory '/usr/src/packages/BUILD/automake-1.16.5'",
                ],
                "(6 more results, 2709 passed, 2 failed)",
            ),
        ];
        for (lines, expected) in cases {
            let v: Vec<String> = lines.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(summarize_test_results(&v), *expected);
        }
    }

    // --- parse_log_links ---

    #[test]
    fn parse_log_links_extracts_only_log_hrefs() {
        let html = r#"
<html><body>
<a href="bash.SUSE_SLE-15-SP5_Update.x86_64.log">log1</a>
<a href="bash.SUSE_SLE-15-SP5_Update.aarch64.log">log2</a>
<a href="other-package.log">unrelated</a>
<a href="README.txt">no-log</a>
</body></html>
"#;
        let links = parse_log_links(html);
        assert_eq!(
            links,
            vec![
                "bash.SUSE_SLE-15-SP5_Update.x86_64.log".to_string(),
                "bash.SUSE_SLE-15-SP5_Update.aarch64.log".to_string(),
                "other-package.log".to_string(),
            ]
        );
    }

    #[test]
    fn get_group_id_prefers_incident_then_aggregated() {
        let groups = vec![
            group(490, "SLE 15 SP5 Core Incidents", "tpl"),
            group(333, "Core Maintenance Updates", "tpl"),
        ];
        assert_eq!(get_group_id(&groups, "15-SP5"), Some(490));
        assert_eq!(get_group_id(&groups, "core"), Some(333));
        assert_eq!(get_group_id(&groups, "99-SP99"), None);
    }
}
