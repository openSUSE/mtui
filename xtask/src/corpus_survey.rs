//! `cargo xtask corpus-survey` — Phase 0.3 of the JSON-template migration gap
//! analysis (`plans/phase0-gap-analysis.md`).
//!
//! Reads the in-flight update queue (`GET /api/v1/updates?status=testing`),
//! then issues one `GET /api/v2/reports/{id}` per id, and reports the status
//! split (200 / 404 / 503 `generating` / other) broken down by RRID kind. This
//! sizes T5 (the legacy-corpus backfill) in the umbrella plan. Read-only: no
//! `Authorization` header, no write verb.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result};
use mtui_datasources::{HttpClient, HttpError, TeReGen, UpdatesQuery, VerifyPolicy};
use mtui_types::RequestReviewID;

/// The default v1 base (matches `mtui_config`'s `default_teregen_api`).
pub const DEFAULT_V1_BASE: &str = "https://qam.suse.de/api/v1";
/// The v2 sibling of [`DEFAULT_V1_BASE`] — same host, no v1 client for it yet
/// (that is exactly what this migration adds).
pub const DEFAULT_V2_BASE: &str = "https://qam.suse.de/api/v2";

/// One probed report's outcome bucket.
///
/// `ServerError500` is its own bucket, not folded into `Other`: a live sweep
/// (2026-09-12, `plans/phase0-gap-analysis.md`) found it is not noise — every
/// 500 is `master`'s legacy-log-to-document renderer failing schema
/// validation (`additionalProperties: false` rejecting fields the renderer
/// emits, or a `required` field coming back `null`/empty), which is exactly
/// T5's "does not validate" prediction with a concrete error signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportStatus {
    Ok200,
    NotFound404,
    Generating503,
    ServerError500,
    Other,
}

/// Classify a `GET /api/v2/reports/{id}` outcome by status code.
///
/// `is_ok` from [`HttpClient::get_bytes_capped`] only ever means 2xx (it calls
/// `error_for_status` internally), so it always buckets as 200. A non-2xx
/// status is read via [`HttpError::status`] before calling this — pure offline
/// logic over plain values, no network access or `HttpError` construction
/// needed, so it is unit-tested directly.
fn classify(is_ok: bool, status: Option<u16>) -> ReportStatus {
    if is_ok {
        return ReportStatus::Ok200;
    }
    match status {
        Some(404) => ReportStatus::NotFound404,
        Some(503) => ReportStatus::Generating503,
        Some(500) => ReportStatus::ServerError500,
        _ => ReportStatus::Other,
    }
}

/// Per-kind status tally. Row key is the RRID kind's canonical wire string
/// (`RequestKind::as_str`), or `"unknown"` for an id `RequestReviewID::parse`
/// cannot classify.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct KindCounts {
    ok_200: u32,
    not_found_404: u32,
    generating_503: u32,
    server_error_500: u32,
    other: u32,
}

impl KindCounts {
    fn record(&mut self, status: ReportStatus) {
        match status {
            ReportStatus::Ok200 => self.ok_200 += 1,
            ReportStatus::NotFound404 => self.not_found_404 += 1,
            ReportStatus::Generating503 => self.generating_503 += 1,
            ReportStatus::ServerError500 => self.server_error_500 += 1,
            ReportStatus::Other => self.other += 1,
        }
    }

    fn total(&self) -> u32 {
        self.ok_200 + self.not_found_404 + self.generating_503 + self.server_error_500 + self.other
    }
}

/// The full survey result: total ids probed and the per-kind breakdown.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SurveyReport {
    by_kind: BTreeMap<String, KindCounts>,
}

impl SurveyReport {
    /// Fold one `(id, outcome)` pair into the report. Pure aggregation, no I/O
    /// — the seam unit tests exercise directly.
    fn record(&mut self, id: &str, outcome: &std::result::Result<Vec<u8>, HttpError>) {
        let kind = RequestReviewID::parse(id)
            .map(|r| r.kind.as_str().to_owned())
            .unwrap_or_else(|_| "unknown".to_owned());
        let status = classify(
            outcome.is_ok(),
            outcome.as_ref().err().and_then(HttpError::status),
        );
        self.by_kind.entry(kind).or_default().record(status);
    }

    fn total(&self) -> u32 {
        self.by_kind.values().map(KindCounts::total).sum()
    }
}

impl fmt::Display for SurveyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<12} {:>6} {:>6} {:>10} {:>6} {:>6} {:>7}",
            "kind", "200", "404", "503-gen", "500", "other", "total"
        )?;
        for (kind, counts) in &self.by_kind {
            writeln!(
                f,
                "{:<12} {:>6} {:>6} {:>10} {:>6} {:>6} {:>7}",
                kind,
                counts.ok_200,
                counts.not_found_404,
                counts.generating_503,
                counts.server_error_500,
                counts.other,
                counts.total()
            )?;
        }
        write!(f, "{:<12} {:>39} {:>7}", "TOTAL", "", self.total())
    }
}

/// Run the survey against `v1_base`/`v2_base` and print the report to stdout.
///
/// # Errors
///
/// Returns an error if the update queue itself cannot be fetched
/// ([`mtui_datasources::TeReGenError`]) — an unreachable server is a hard
/// failure, not a data point. Per-id v2 probe failures are not: they are the
/// measurement.
pub async fn run(v1_base: &str, v2_base: &str, verify: VerifyPolicy) -> Result<()> {
    let http = HttpClient::new(verify).context("building the shared HTTP client")?;
    let teregen = TeReGen::with_client(http.clone(), v1_base);

    let query = UpdatesQuery {
        review_group: None,
        status: Some("testing"),
        assignee: None,
        unassigned: false,
        with_assignment: false,
        no_cache: false,
    };
    let updates = teregen
        .updates(&query)
        .await
        .context("fetching the update queue")?
        .context("update queue response carried no `updates` key")?;
    let ids: Vec<String> = updates
        .as_array()
        .context("`updates` was not a JSON array")?
        .iter()
        .filter_map(|u| u.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();

    eprintln!("corpus-survey: {} ids in the testing queue", ids.len());

    let mut report = SurveyReport::default();
    for (i, id) in ids.iter().enumerate() {
        let url = format!("{v2_base}/reports/{id}");
        let outcome = http
            .get_bytes_capped(&url, mtui_datasources::MAX_API_BODY)
            .await;
        report.record(id, &outcome);
        eprint!("\r  probed {}/{}", i + 1, ids.len());
    }
    eprintln!();

    println!("{report}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> std::result::Result<Vec<u8>, HttpError> {
        Ok(b"{}".to_vec())
    }

    #[test]
    fn classify_maps_known_statuses_and_falls_back_to_other() {
        assert_eq!(classify(true, None), ReportStatus::Ok200);
        assert_eq!(classify(false, Some(404)), ReportStatus::NotFound404);
        assert_eq!(classify(false, Some(503)), ReportStatus::Generating503);
        assert_eq!(classify(false, Some(500)), ReportStatus::ServerError500);
        assert_eq!(classify(false, Some(403)), ReportStatus::Other);
        assert_eq!(classify(false, None), ReportStatus::Other);
    }

    #[test]
    fn record_classifies_by_kind_and_counts_all_ids_exactly_once() {
        let mut report = SurveyReport::default();
        report.record("SUSE:Maintenance:1:2", &ok());
        report.record("SUSE:SLFO:1.2:3", &ok());
        report.record("SUSE:PI:16.0:4", &ok());
        report.record("not-an-rrid", &ok());

        assert_eq!(report.total(), 4);
        assert_eq!(report.by_kind["Maintenance"].total(), 1);
        assert_eq!(report.by_kind["SLFO"].total(), 1);
        assert_eq!(report.by_kind["PI"].total(), 1);
        assert_eq!(report.by_kind["unknown"].total(), 1);
    }

    #[test]
    fn unparseable_id_buckets_as_unknown_not_dropped() {
        let mut report = SurveyReport::default();
        report.record("garbage", &ok());
        assert_eq!(report.total(), 1);
        assert_eq!(report.by_kind["unknown"].ok_200, 1);
    }

    #[test]
    fn display_totals_match_recorded_count() {
        let mut report = SurveyReport::default();
        for _ in 0..3 {
            report.record("SUSE:Maintenance:1:2", &ok());
        }
        let rendered = report.to_string();
        assert!(rendered.contains("Maintenance"));
        assert!(rendered.contains(&report.total().to_string()));
    }
}
