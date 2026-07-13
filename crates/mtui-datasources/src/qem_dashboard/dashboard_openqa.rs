//! Dashboard-backed *auto* workflow openQA data provider, ported from
//! `mtui/data_sources/qem_dashboard/dashboard_openqa.py`.
//!
//! [`DashboardAutoOpenQA`] loads an incident's openQA jobs from the QEM
//! Dashboard (both the per-incident jobs and the aggregate/update jobs), derives
//! the install-log URLs when the install jobs passed, and renders the
//! review-facing `Results from openQA jobs` text block.
//!
//! ## Rendering contract (mirrors upstream `_pretty_print`)
//!
//! The block collapses all-passed jobs into per-group summary counts and lists
//! only the failing jobs individually, so a reviewer scans a short report rather
//! than hundreds of `passed` lines. Within a section:
//!
//! * a shared aggregate `BUILD` is *hoisted* once when every job uses it;
//! * *problem* groups (any failed/incomplete/timeout/other) stay per-arch and
//!   are printed first, in dashboard insertion order;
//! * *all-passed* groups fold across architectures into one row;
//! * failing jobs are nested under a per-group header with their openQA URLs
//!   right-aligned by padding the test-name column.
//!
//! The exact byte-for-byte layout is pinned by the ported upstream assertions
//! and `insta` snapshots in the crate tests.
//!
//! ## Async fan-out (deviation from upstream)
//!
//! Upstream fans the per-setting fetches out over a `ThreadPoolExecutor` with a
//! 60s per-future cap. This port uses native `tokio` concurrency with a
//! [`tokio::time::timeout`] per fetch, preserving both the ordering (incident
//! settings first, then update settings; jobs in submission order) and the
//! warn-and-skip-on-timeout behaviour.

use std::collections::BTreeMap;

use serde_json::Value;
use tokio::time::timeout;

use mtui_types::{OpenQAResult, RequestReviewID, URLs};

use crate::openqa::{OPENQA_INSTALL_DISTRI, install_logfile_for};

use super::client::{FAILED_RESULTS, FUTURE_TIMEOUT, QemDashboardClient};
use super::incident::QemIncident;

/// The install-test job-name marker that identifies an incident-install job.
const INCIDENT_INSTALL_MARKER: &str = "qam-incidentinstall";

/// Counter keys in display order. `total` is always kept; the others are only
/// printed when non-zero so the Summary block stays scannable. Ported from
/// upstream `_COUNT_KEYS`.
const COUNT_KEYS: [&str; 6] = [
    "passed",
    "softfailed",
    "failed",
    "incomplete",
    "timeout_exceeded",
    "other",
];

/// A dashboard job normalized to the fields the provider reads.
///
/// The typed replacement for upstream's untyped `dict` (`_normalize_job`): only
/// the fields consumed downstream are retained, keeping the pretty-printer and
/// URL builder honest about their inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedJob {
    /// The openQA job id (`job_id`); `None` when absent.
    id: Option<i64>,
    /// The test/scenario name (`name`); may be empty.
    test: String,
    /// The openQA job result/status; may be empty.
    result: String,
    /// `"incident"` or `"aggregate"`.
    source: String,
    /// The aggregate product (aggregate jobs only).
    product: Option<String>,
    /// Resolved openQA settings the printer/URL builder read.
    distri: String,
    flavor: String,
    arch: String,
    version: String,
    build: String,
    /// Whether this run is superseded by a retrigger (`obsolete` flag).
    obsolete: bool,
}

impl NormalizedJob {
    /// Normalize a raw dashboard job for a given `source`/`setting`, mirroring
    /// upstream `_normalize_job` for the fields this port consumes.
    fn from_raw(job: &Value, source: &str, setting: &Value) -> Self {
        let settings = setting.get("settings");
        let s = |key: &str| settings.and_then(|s| s.get(key)).and_then(Value::as_str);
        let job_str = |key: &str| job.get(key).and_then(Value::as_str);
        let set_str = |key: &str| setting.get(key).and_then(Value::as_str);

        // `distri`/`flavor`/... take the job-level value first, then the
        // setting-level fallback, matching upstream's `job.get(x) or setting...`.
        let pick = |job_key: &str, setting_val: Option<&str>| -> String {
            job_str(job_key)
                .filter(|v| !v.is_empty())
                .or(setting_val.filter(|v| !v.is_empty()))
                .unwrap_or("")
                .to_string()
        };

        NormalizedJob {
            id: job.get("job_id").and_then(Value::as_i64),
            test: job_str("name").unwrap_or("").to_string(),
            result: job_str("status").unwrap_or("").to_string(),
            source: source.to_string(),
            product: if source == "aggregate" {
                set_str("product").map(str::to_owned)
            } else {
                None
            },
            distri: pick("distri", s("DISTRI")),
            flavor: pick("flavor", set_str("flavor")),
            arch: pick("arch", set_str("arch")),
            version: pick("version", set_str("version")),
            build: pick("build", set_str("build")),
            obsolete: job
                .get("obsolete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// Build a job directly from an already-normalized test map (used by tests
    /// and callers that pre-shape jobs), reading the `settings` sub-map.
    #[cfg(test)]
    fn from_normalized(job: &Value) -> Self {
        let settings = job.get("settings");
        let s = |key: &str| {
            settings
                .and_then(|m| m.get(key))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        NormalizedJob {
            id: job.get("id").and_then(Value::as_i64),
            test: job.get("test").and_then(Value::as_str).unwrap_or("").into(),
            result: job
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            source: job
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("incident")
                .into(),
            product: job
                .get("product")
                .and_then(Value::as_str)
                .map(str::to_owned),
            distri: s("DISTRI"),
            flavor: s("FLAVOR"),
            arch: s("ARCH"),
            version: s("VERSION"),
            build: s("BUILD"),
            obsolete: job
                .get("obsolete")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// Whether this is a superseded run that must not count toward results.
    ///
    /// Mirrors upstream `_is_obsolete`: when an openQA job is retriggered the
    /// dashboard keeps the older run but marks it superseded — either with an
    /// `obsolete` flag or an `"obsoleted"` result. Both must be dropped so a
    /// stale failure does not poison the install verdict
    /// ([`has_passed_install_jobs`](Self::has_passed_install_jobs)) or surface as
    /// a phantom entry in the failed-jobs listing. Matches `oqa_search`'s
    /// `incident_jobs`, which filters `result == "obsoleted"`; only the current
    /// run matters.
    fn is_obsolete(&self) -> bool {
        self.obsolete || self.result == "obsoleted"
    }

    /// The openQA host job URL (`{host}/tests/{id}`); empty when no id.
    fn job_url(&self, host: &str) -> String {
        match self.id {
            Some(id) => format!("{}/tests/{id}", host.trim_end_matches('/')),
            None => String::new(),
        }
    }
}

/// Per-result counters for one group.
#[derive(Debug, Clone, Default)]
struct Counts {
    passed: u32,
    softfailed: u32,
    failed: u32,
    incomplete: u32,
    timeout_exceeded: u32,
    other: u32,
    total: u32,
}

impl Counts {
    fn get(&self, key: &str) -> u32 {
        match key {
            "passed" => self.passed,
            "softfailed" => self.softfailed,
            "failed" => self.failed,
            "incomplete" => self.incomplete,
            "timeout_exceeded" => self.timeout_exceeded,
            "other" => self.other,
            "total" => self.total,
            _ => 0,
        }
    }

    fn add(&mut self, key: &str, n: u32) {
        match key {
            "passed" => self.passed += n,
            "softfailed" => self.softfailed += n,
            "failed" => self.failed += n,
            "incomplete" => self.incomplete += n,
            "timeout_exceeded" => self.timeout_exceeded += n,
            "other" => self.other += n,
            "total" => self.total += n,
            _ => {}
        }
    }

    /// Whether this group has any non-passing result. Mirrors `_has_problems`.
    fn has_problems(&self) -> bool {
        self.failed != 0 || self.incomplete != 0 || self.timeout_exceeded != 0 || self.other != 0
    }

    /// Render dropping zero entries; `total` is always last. Mirrors
    /// `_format_counts`.
    fn format(&self) -> String {
        let mut parts: Vec<String> = COUNT_KEYS
            .iter()
            .filter(|k| self.get(k) != 0)
            .map(|k| format!("{k}: {}", self.get(k)))
            .collect();
        parts.push(format!("total: {}", self.total));
        parts.join(", ")
    }
}

/// Dashboard-backed auto-workflow data provider.
///
/// Mirrors upstream `DashboardAutoOpenQA`: [`run`](Self::run) loads the jobs,
/// resolves the install-log [`results`](Self::results) when the install jobs
/// passed, and renders the [`pp`](Self::pp) text block.
#[derive(Debug, Clone)]
pub struct DashboardAutoOpenQA {
    /// The openQA host (base URL) used in rendered URLs.
    pub host: String,
    /// The request/review id.
    pub rrid: RequestReviewID,
    /// The dashboard client (shared with the incident).
    pub client: QemDashboardClient,
    /// The resolved dashboard incident number.
    pub incident_number: String,
    /// The rendered `Results from openQA jobs` block (empty until [`run`]).
    pub pp: Vec<String>,
    /// The install-log URLs, or `None` when the install jobs did not all pass.
    pub results: Option<Vec<URLs>>,
    /// The normalized jobs (populated by [`run`]).
    jobs: Vec<NormalizedJob>,
}

impl DashboardAutoOpenQA {
    /// The connector kind tag, mirroring upstream `kind = "auto"`.
    pub const KIND: &'static str = "auto";

    /// Build the provider for an incident on a given openQA `host`.
    #[must_use]
    pub fn new(host: impl Into<String>, incident: &QemIncident, rrid: RequestReviewID) -> Self {
        Self {
            host: host.into(),
            rrid,
            client: incident.client.clone(),
            incident_number: incident.incident_number.clone(),
            pp: Vec::new(),
            results: None,
            jobs: Vec::new(),
        }
    }

    /// Load, resolve, and render. Mirrors upstream `run`.
    pub async fn run(&mut self) -> &mut Self {
        self.jobs = self.load_jobs(FUTURE_TIMEOUT).await;
        self.results = if Self::has_passed_install_jobs(&self.jobs) {
            self.get_logs_url(&self.jobs)
        } else {
            None
        };
        self.pp = Self::pretty_print(&self.host, &self.jobs);
        self
    }

    /// Test seam: run with a shortened per-fetch timeout so the timeout
    /// warn-and-skip paths can be exercised without a 60s wait.
    #[cfg(test)]
    pub(crate) async fn run_with_timeout(&mut self, per_fetch: std::time::Duration) -> &mut Self {
        self.jobs = self.load_jobs(per_fetch).await;
        self.results = if Self::has_passed_install_jobs(&self.jobs) {
            self.get_logs_url(&self.jobs)
        } else {
            None
        };
        self.pp = Self::pretty_print(&self.host, &self.jobs);
        self
    }

    /// Test seam: expose the loaded-and-normalized job test names, so the
    /// integration tests can assert ordering / timeout-skip behaviour without
    /// reaching into the private `jobs` field.
    #[cfg(test)]
    pub(crate) fn job_test_names(&self) -> Vec<String> {
        self.jobs.iter().map(|j| j.test.clone()).collect()
    }

    /// Whether the provider produced any renderable output. Mirrors `__bool__`.
    #[must_use]
    pub fn is_present(&self) -> bool {
        !self.pp.is_empty() || self.results.as_ref().is_some_and(|r| !r.is_empty())
    }

    /// Fetch and normalize all incident + aggregate jobs.
    ///
    /// The two top-level settings lists are fetched concurrently; then the
    /// per-setting job fetches fan out concurrently while the results are read
    /// back in submission order (incident settings first, then update settings)
    /// so the resulting list order is deterministic. Each fetch is guarded by a
    /// [`FUTURE_TIMEOUT`] cap: a timed-out top-level settings fetch is treated as
    /// empty, and a timed-out per-setting jobs fetch is skipped — both with a
    /// `warn` — so one slow endpoint neither blocks nor corrupts the batch.
    async fn load_jobs(&self, per_fetch: std::time::Duration) -> Vec<NormalizedJob> {
        let n = &self.incident_number;

        // Top-level settings: independent, fetched concurrently.
        let (incident_settings, update_settings) = tokio::join!(
            Self::await_settings(
                self.client.incident_settings(n),
                "incident_settings",
                per_fetch
            ),
            Self::await_settings(self.client.update_settings(n), "update_settings", per_fetch),
        );

        // Flat task list preserving insertion order: incident settings first.
        let mut tasks: Vec<(&'static str, &Value, i64)> = Vec::new();
        for setting in &incident_settings {
            if let Some(id) = setting.get("id").and_then(Value::as_i64) {
                tasks.push(("incident", setting, id));
            }
        }
        for setting in &update_settings {
            if let Some(id) = setting.get("id").and_then(Value::as_i64) {
                tasks.push(("aggregate", setting, id));
            }
        }
        if tasks.is_empty() {
            return Vec::new();
        }

        // Fan out per-setting jobs fetches; await in submission order.
        let futures: Vec<_> = tasks
            .iter()
            .map(|&(source, _, id)| {
                let client = &self.client;
                async move {
                    let fut = async {
                        if source == "incident" {
                            client.incident_jobs(id).await
                        } else {
                            client.update_jobs(id).await
                        }
                    };
                    (source, id, timeout(per_fetch, fut).await)
                }
            })
            .collect();

        let results = futures_join_all(futures).await;

        let mut jobs = Vec::new();
        for ((source, setting, _), (_, id, outcome)) in tasks.iter().zip(results) {
            match outcome {
                Ok(setting_jobs) => {
                    for job in &setting_jobs {
                        let normalized = NormalizedJob::from_raw(job, source, setting);
                        if normalized.is_obsolete() {
                            tracing::debug!(
                                "dropping obsoleted {source} job {:?} ({})",
                                normalized.id,
                                normalized.test
                            );
                            continue;
                        }
                        jobs.push(normalized);
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        "QEM Dashboard {source} jobs fetch for setting {id} timed out; skipping"
                    );
                }
            }
        }
        jobs
    }

    /// Await a top-level settings future, returning `[]` on timeout.
    ///
    /// Mirrors `_await_settings`: a timeout is logged at `warn` and treated as
    /// empty, so callers see the same shape whether the failure was an HTTP error
    /// (already folded to `[]` by the client) or a timeout.
    async fn await_settings<F>(fut: F, label: &str, per_fetch: std::time::Duration) -> Vec<Value>
    where
        F: std::future::Future<Output = Vec<Value>>,
    {
        match timeout(per_fetch, fut).await {
            Ok(settings) => settings,
            Err(_) => {
                tracing::warn!("QEM Dashboard {label} fetch timed out; treating as empty");
                Vec::new()
            }
        }
    }

    /// Whether a result counts as a pass. Mirrors `_normalize_result`.
    fn normalize_result(result: &str) -> bool {
        result == "passed" || result == "softfailed"
    }

    /// Whether every incident-install job passed. Mirrors
    /// `_has_passed_install_jobs`: install jobs are those whose test name
    /// contains `qam-incidentinstall`; a missing name is tolerated (never
    /// matches). An empty install-job set is vacuously `true`.
    fn has_passed_install_jobs(jobs: &[NormalizedJob]) -> bool {
        jobs.iter()
            .filter(|j| j.test.contains(INCIDENT_INSTALL_MARKER))
            .all(|j| Self::normalize_result(&j.result))
    }

    /// Build the install-log URLs for the passing install jobs. Mirrors
    /// `_get_logs_url`: `None` when there are no jobs, else one [`URLs`] per
    /// passing install job, with `distri` falling back to
    /// [`OPENQA_INSTALL_DISTRI`] and the log filename resolved per job name.
    fn get_logs_url(&self, jobs: &[NormalizedJob]) -> Option<Vec<URLs>> {
        if jobs.is_empty() {
            return None;
        }
        Some(
            jobs.iter()
                .filter(|j| {
                    j.test.contains(INCIDENT_INSTALL_MARKER) && Self::normalize_result(&j.result)
                })
                .map(|j| {
                    let distri = if j.distri.is_empty() {
                        OPENQA_INSTALL_DISTRI.to_string()
                    } else {
                        j.distri.clone()
                    };
                    let id = j.id.map(|i| i.to_string()).unwrap_or_default();
                    let url = format!(
                        "{}/tests/{id}/file/{}",
                        self.host.trim_end_matches('/'),
                        install_logfile_for(&j.test)
                    );
                    URLs::new(
                        distri,
                        j.arch.clone(),
                        j.version.clone(),
                        url,
                        j.result.clone(),
                    )
                })
                .collect(),
        )
    }

    /// Render the `Results from openQA jobs` block. Mirrors `_pretty_print`.
    fn pretty_print(host: &str, jobs: &[NormalizedJob]) -> Vec<String> {
        if jobs.is_empty() {
            tracing::debug!("No dashboard jobs - no results");
            return Vec::new();
        }
        let mut ret = vec![
            "\n".to_string(),
            "Results from openQA jobs:\n".to_string(),
            "=========================\n".to_string(),
            "\n".to_string(),
        ];
        Self::pretty_print_section(&mut ret, host, "Incident jobs", jobs, "incident");
        Self::pretty_print_section(&mut ret, host, "Aggregate jobs", jobs, "aggregate");
        ret
    }

    /// Render one section (`incident` or `aggregate`). Mirrors
    /// `_pretty_print_section`.
    fn pretty_print_section(
        ret: &mut Vec<String>,
        host: &str,
        title: &str,
        jobs: &[NormalizedJob],
        source: &str,
    ) {
        let section: Vec<&NormalizedJob> = jobs.iter().filter(|j| j.source == source).collect();
        if section.is_empty() {
            return;
        }
        ret.push(format!("{title}:\n"));

        // Hoist a shared aggregate BUILD when every job uses the same one.
        let mut hoisted_build: Option<String> = None;
        if source == "aggregate" {
            let builds: std::collections::BTreeSet<String> =
                section.iter().map(|j| val(&j.build)).collect();
            if builds.len() == 1 {
                let b = builds.into_iter().next().unwrap();
                ret.push(format!("  build: {b}\n"));
                hoisted_build = Some(b);
            }
        }
        let hoisted = hoisted_build.is_some();

        // Per-group counts + failed jobs, in insertion order.
        let mut order: Vec<(String, String, String)> = Vec::new();
        let mut groups: BTreeMap<(String, String, String), Counts> = BTreeMap::new();
        let mut failed_by_group: BTreeMap<(String, String, String), Vec<&NormalizedJob>> =
            BTreeMap::new();
        for job in &section {
            let key = group_key(job, source);
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            let counts = groups.entry(key.clone()).or_default();
            counts.total += 1;
            let result = if job.result.is_empty() {
                "other"
            } else {
                job.result.as_str()
            };
            if COUNT_KEYS.contains(&result) {
                counts.add(result, 1);
            } else {
                counts.other += 1;
            }
            if FAILED_RESULTS.contains(&job.result.as_str()) {
                failed_by_group.entry(key).or_default().push(job);
            }
        }

        let problem_keys: Vec<&(String, String, String)> =
            order.iter().filter(|k| groups[*k].has_problems()).collect();
        let passed_keys: Vec<&(String, String, String)> = order
            .iter()
            .filter(|k| !groups[*k].has_problems())
            .collect();

        ret.push("  Summary:\n".to_string());

        // Problem groups first, in insertion order.
        for key in &problem_keys {
            ret.push(format!(
                "  {} -> {}\n",
                format_group_header(source, key, hoisted),
                groups[*key].format()
            ));
        }

        // Fold all-passed groups across archs.
        let mut fold_order: Vec<Vec<String>> = Vec::new();
        let mut folded: BTreeMap<Vec<String>, (Vec<String>, Counts)> = BTreeMap::new();
        for key in &passed_keys {
            let (a, b, arch) = (&key.0, &key.1, &key.2);
            let fold_key: Vec<String> = if source == "aggregate" {
                if hoisted {
                    vec![a.clone()]
                } else {
                    vec![a.clone(), b.clone()]
                }
            } else {
                vec![a.clone(), b.clone()]
            };
            if !folded.contains_key(&fold_key) {
                fold_order.push(fold_key.clone());
            }
            let entry = folded
                .entry(fold_key)
                .or_insert_with(|| (Vec::new(), Counts::default()));
            if !entry.0.contains(arch) {
                entry.0.push(arch.clone());
            }
            for ckey in COUNT_KEYS {
                entry.1.add(ckey, groups[*key].get(ckey));
            }
            entry.1.total += groups[*key].total;
        }

        for fold_key in &fold_order {
            let (archs, counts) = &folded[fold_key];
            let n_archs = archs.len();
            ret.push(format!(
                "  {} - archs: {} -> {} ({n_archs} arch{})\n",
                format_folded_header(source, fold_key, hoisted),
                archs.join(", "),
                counts.format(),
                if n_archs == 1 { "" } else { "es" }
            ));
        }

        // Failed jobs, nested under group headers.
        if !failed_by_group.is_empty() {
            ret.push("  Failed jobs:\n".to_string());
            for key in &problem_keys {
                let Some(fjobs) = failed_by_group.get(*key) else {
                    continue;
                };
                if fjobs.is_empty() {
                    continue;
                }
                ret.push(failed_group_header(source, key, fjobs.len(), hoisted));
                let width = fjobs.iter().map(|j| j.test.len()).max().unwrap_or(0);
                for job in fjobs {
                    let url = job.job_url(host);
                    let suffix = if url.is_empty() {
                        String::new()
                    } else {
                        format!("  {url}")
                    };
                    if job.result == "failed" {
                        ret.push(format!("      {:<width$}{suffix}\n", job.test));
                    } else {
                        ret.push(format!(
                            "      {:<width$}  [{}]{suffix}\n",
                            job.test, job.result
                        ));
                    }
                }
            }
        } else if !problem_keys.is_empty() {
            // Problem groups exist but none carry a failed/incomplete/
            // timeout_exceeded job, so `failed_by_group` is empty — their
            // problems are entirely in the `other` bucket (still-running,
            // parallel_failed, skipped, ...). The Summary already flags them;
            // don't claim success (the old bug) and don't print an empty
            // `Failed jobs:` block. Mirrors upstream.
            ret.push(
                "  No failed jobs, but some groups need review (see Summary above).\n".to_string(),
            );
        } else {
            ret.push("  All jobs passed.\n".to_string());
        }
        ret.push("\n".to_string());
    }
}

impl OpenQAResult for DashboardAutoOpenQA {
    fn kind(&self) -> &str {
        Self::KIND
    }

    /// The port of upstream `DashboardAutoOpenQA.__bool__`: truthy when the
    /// rendered block or the resolved install-log results are non-empty
    /// (delegates to [`is_present`](Self::is_present)).
    fn has_results(&self) -> bool {
        self.is_present()
    }
}

/// Await a list of futures, preserving order. A tiny `join_all` avoiding a
/// `futures` crate dependency (the fetches share no state; correctness only
/// needs the results back in the same order they were submitted).
async fn futures_join_all<F, T>(futures: Vec<F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    let mut out = Vec::with_capacity(futures.len());
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    for fut in &mut pinned {
        out.push(fut.as_mut().await);
    }
    out
}

/// Coerce a value to a display string, mapping empty to `"unknown"`. Mirrors
/// `_val`.
fn val(value: &str) -> String {
    if value.is_empty() {
        "unknown".to_string()
    } else {
        value.to_string()
    }
}

/// The 3-tuple group key for a job. Mirrors `_group_key`.
fn group_key(job: &NormalizedJob, source: &str) -> (String, String, String) {
    if source == "aggregate" {
        (
            val(job.product.as_deref().unwrap_or("")),
            val(&job.build),
            val(&job.arch),
        )
    } else {
        (val(&job.version), val(&job.flavor), val(&job.arch))
    }
}

/// The per-group summary header. Mirrors `_format_group_header`.
fn format_group_header(
    source: &str,
    key: &(String, String, String),
    hoisted_build: bool,
) -> String {
    let (a, b, arch) = (&key.0, &key.1, &key.2);
    if source == "aggregate" {
        if hoisted_build {
            format!("    product: {a} - arch: {arch}")
        } else {
            format!("    product: {a} - build: {b} - arch: {arch}")
        }
    } else {
        format!("    version: {a} - flavor: {b} - arch: {arch}")
    }
}

/// The folded all-passed header. Mirrors `_format_folded_header`.
fn format_folded_header(source: &str, fold_key: &[String], hoisted_build: bool) -> String {
    if source == "aggregate" {
        if hoisted_build {
            format!("    product: {}", fold_key[0])
        } else {
            format!("    product: {} - build: {}", fold_key[0], fold_key[1])
        }
    } else {
        format!("    version: {} - flavor: {}", fold_key[0], fold_key[1])
    }
}

/// The nested failed-jobs group header. Mirrors `_failed_group_header`.
fn failed_group_header(
    source: &str,
    key: &(String, String, String),
    n_failed: usize,
    hoisted_build: bool,
) -> String {
    let (a, b, arch) = (&key.0, &key.1, &key.2);
    if source == "aggregate" {
        if hoisted_build {
            format!("    {a} / {arch} ({n_failed} failed):\n")
        } else {
            format!("    {a} / {b} / {arch} ({n_failed} failed):\n")
        }
    } else {
        format!("    {a} / {b} / {arch} ({n_failed} failed):\n")
    }
}

#[cfg(test)]
mod tests {
    //! Ported from `tests/test_qem_dashboard_connector.py`, the
    //! `_pretty_print` / pure-helper assertions. The `run`/`load_jobs`
    //! wiremock + timeout tests live in `tests/qem_dashboard.rs`.
    use super::*;
    use serde_json::json;

    const OPENQA_HOST: &str = "https://openqa.example.com";

    fn render(jobs: &[NormalizedJob]) -> String {
        DashboardAutoOpenQA::pretty_print(OPENQA_HOST, jobs).concat()
    }

    fn incident_job(job_id: i64, name: &str, result: &str) -> NormalizedJob {
        incident_job_v(
            job_id,
            name,
            result,
            "15-SP5",
            "Server-DVD-Incidents",
            "x86_64",
        )
    }

    fn incident_job_v(
        job_id: i64,
        name: &str,
        result: &str,
        version: &str,
        flavor: &str,
        arch: &str,
    ) -> NormalizedJob {
        NormalizedJob::from_normalized(&json!({
            "id": job_id,
            "test": name,
            "result": result,
            "source": "incident",
            "settings": {
                "DISTRI": "sle",
                "FLAVOR": flavor,
                "ARCH": arch,
                "VERSION": version,
                "BUILD": ":12358:bash",
            },
        }))
    }

    fn aggregate_job(
        job_id: i64,
        name: &str,
        result: &str,
        product: &str,
        build: &str,
        arch: &str,
    ) -> NormalizedJob {
        NormalizedJob::from_normalized(&json!({
            "id": job_id,
            "test": name,
            "result": result,
            "source": "aggregate",
            "product": product,
            "settings": {
                "DISTRI": "sle",
                "FLAVOR": "Server-DVD-Updates",
                "ARCH": arch,
                "VERSION": "15-SP5",
                "BUILD": build,
            },
        }))
    }

    #[test]
    fn pretty_print_collapses_passed() {
        let jobs: Vec<_> = (0..50)
            .map(|i| incident_job(2000 + i, &format!("qam-test-{i}"), "passed"))
            .collect();
        let out = render(&jobs);

        assert!(out.contains("Summary:"));
        assert!(out.contains("passed: 50"));
        assert!(out.contains("total: 50"));
        for noisy in [
            "softfailed: 0",
            "failed: 0",
            "incomplete: 0",
            "timeout_exceeded: 0",
            "other: 0",
        ] {
            assert!(!out.contains(noisy), "unexpected zero counter: {noisy}");
        }
        for i in 0..50 {
            assert!(!out.contains(&format!("qam-test-{i}")));
        }
        assert!(!out.contains("Failed jobs:"));
        assert!(out.contains("All jobs passed."));
    }

    #[test]
    fn pretty_print_lists_failed() {
        let mut jobs: Vec<_> = (0..10)
            .map(|i| incident_job(3000 + i, &format!("qam-pass-{i}"), "passed"))
            .collect();
        jobs.push(incident_job(3100, "qam-failure", "failed"));
        jobs.push(incident_job(3101, "qam-incomplete", "incomplete"));
        jobs.push(incident_job(3102, "qam-timeout", "timeout_exceeded"));
        jobs.push(incident_job(3103, "qam-soft", "softfailed"));
        let out = render(&jobs);

        assert!(out.contains("passed: 10"));
        assert!(out.contains("softfailed: 1"));
        assert!(out.contains("failed: 1"));
        assert!(out.contains("incomplete: 1"));
        assert!(out.contains("timeout_exceeded: 1"));
        assert!(out.contains("total: 14"));
        assert!(!out.contains("other: 0"));

        assert!(out.contains("Failed jobs:"));
        assert!(out.contains("15-SP5 / Server-DVD-Incidents / x86_64 (3 failed):"));
        assert!(out.contains("qam-failure"));
        assert!(out.contains("qam-incomplete"));
        assert!(out.contains("qam-timeout"));
        assert!(out.contains("[incomplete]"));
        assert!(out.contains("[timeout_exceeded]"));
        assert!(!out.contains("qam-soft"));
        for i in 0..10 {
            assert!(!out.contains(&format!("qam-pass-{i}")));
        }

        assert!(out.contains(&format!("{OPENQA_HOST}/tests/3100")));
        assert!(out.contains(&format!("{OPENQA_HOST}/tests/3101")));
        assert!(out.contains(&format!("{OPENQA_HOST}/tests/3102")));
    }

    #[test]
    fn pretty_print_aggregate_grouping() {
        let jobs = vec![
            aggregate_job(
                4000,
                "mau-a",
                "passed",
                "SLES-15-SP5",
                "20240101-1",
                "x86_64",
            ),
            aggregate_job(
                4001,
                "mau-b",
                "passed",
                "SLES-15-SP5",
                "20240101-1",
                "x86_64",
            ),
            aggregate_job(
                4002,
                "mau-c",
                "failed",
                "SLES-15-SP6",
                "20240101-1",
                "x86_64",
            ),
        ];
        let out = render(&jobs);

        assert!(out.contains("Aggregate jobs:"));
        assert_eq!(out.matches("  build: 20240101-1\n").count(), 1);
        assert!(out.contains("product: SLES-15-SP5"));
        assert!(out.contains("product: SLES-15-SP6"));
        assert!(!out.contains(" - build: "));
        assert!(out.find("SLES-15-SP6").unwrap() < out.find("SLES-15-SP5").unwrap());
        assert!(out.contains("passed: 2"));
        assert!(out.contains("Failed jobs:"));
        assert!(out.contains("SLES-15-SP6 / x86_64 (1 failed):"));
        assert!(out.contains("mau-c"));
        assert!(!out.contains("mau-a"));
        assert!(!out.contains("mau-b"));
    }

    #[test]
    fn pretty_print_unknown_grouping_keys() {
        let mut job = incident_job(5000, "qam-x", "passed");
        job.version = String::new();
        job.flavor = String::new();
        let out = render(&[job]);

        assert!(out.contains("version: unknown"));
        assert!(out.contains("flavor: unknown"));
    }

    #[test]
    fn is_obsolete_flag_and_result() {
        // The `obsolete` flag alone marks a superseded run.
        let flagged = NormalizedJob::from_normalized(&json!({
            "test": "qam-x", "result": "passed", "obsolete": true,
        }));
        assert!(flagged.is_obsolete());

        // A `result == "obsoleted"` also marks a superseded run.
        let obsoleted = NormalizedJob::from_normalized(&json!({
            "test": "qam-x", "result": "obsoleted",
        }));
        assert!(obsoleted.is_obsolete());

        // A normal current run is not obsolete.
        let current = incident_job(1, "qam-x", "passed");
        assert!(!current.is_obsolete());
    }

    #[test]
    fn pretty_print_other_bucket_needs_review_not_all_passed() {
        // A group whose only problem lives in the `other` bucket (a still-running
        // `result: none`) is flagged in the Summary (`has_problems`) but carries
        // no failed/incomplete/timeout job, so `failed_by_group` is empty. The
        // trailer must NOT claim success and must NOT print an empty Failed jobs
        // block; it prints the "need review" note instead.
        let jobs = vec![
            incident_job(1, "qam-pass", "passed"),
            incident_job(2, "qam-running", "none"),
        ];
        let out = render(&jobs);

        assert!(out.contains("No failed jobs, but some groups need review (see Summary above)."));
        assert!(!out.contains("All jobs passed."));
        assert!(!out.contains("Failed jobs:"));
        assert!(out.contains("other: 1"));
    }

    #[test]
    fn format_counts_skips_zeros() {
        let counts = Counts {
            passed: 10,
            failed: 2,
            total: 12,
            ..Default::default()
        };
        let out = counts.format();
        assert_eq!(out, "passed: 10, failed: 2, total: 12");
        assert!(!out.contains("softfailed"));
        assert!(!out.contains("other"));
    }

    #[test]
    fn pretty_print_aggregate_hoists_build() {
        let same_build = vec![
            aggregate_job(6000, "mau-x", "passed", "A", "20260101-1", "x86_64"),
            aggregate_job(6001, "mau-y", "passed", "B", "20260101-1", "x86_64"),
        ];
        let out_same = render(&same_build);
        assert_eq!(out_same.matches("  build: 20260101-1\n").count(), 1);
        assert!(!out_same.contains(" - build: "));

        let mixed_build = vec![
            aggregate_job(6100, "mau-x", "passed", "A", "20260101-1", "x86_64"),
            aggregate_job(6101, "mau-y", "passed", "B", "20260102-2", "x86_64"),
        ];
        let out_mixed = render(&mixed_build);
        assert!(!out_mixed.contains("  build: 20260101-1\n"));
        assert!(!out_mixed.contains("  build: 20260102-2\n"));
        assert!(out_mixed.contains("- build: 20260101-1"));
        assert!(out_mixed.contains("- build: 20260102-2"));
    }

    #[test]
    fn pretty_print_problem_groups_sorted_first() {
        let jobs = vec![
            incident_job_v(
                7000,
                "qam-ok-1",
                "passed",
                "15-SP4",
                "Server-DVD-Incidents",
                "x86_64",
            ),
            incident_job_v(
                7001,
                "qam-ok-2",
                "passed",
                "15-SP4",
                "Server-DVD-Incidents",
                "x86_64",
            ),
            incident_job_v(
                7100,
                "qam-bad",
                "failed",
                "15-SP7",
                "Server-DVD-Incidents",
                "x86_64",
            ),
        ];
        let out = render(&jobs);
        let summary_start = out.find("Summary:").unwrap();
        let failed_start = out.find("Failed jobs:").unwrap();
        let block = &out[summary_start..failed_start];
        assert!(block.find("15-SP7").unwrap() < block.find("15-SP4").unwrap());
    }

    #[test]
    fn pretty_print_folds_all_passed_archs() {
        let jobs = vec![
            incident_job_v(
                8000,
                "t1",
                "passed",
                "15-SP5",
                "Server-DVD-Incidents",
                "x86_64",
            ),
            incident_job_v(
                8001,
                "t2",
                "passed",
                "15-SP5",
                "Server-DVD-Incidents",
                "x86_64",
            ),
            incident_job_v(
                8002,
                "t3",
                "passed",
                "15-SP5",
                "Server-DVD-Incidents",
                "aarch64",
            ),
            incident_job_v(
                8003,
                "t4",
                "passed",
                "15-SP5",
                "Server-DVD-Incidents",
                "s390x",
            ),
        ];
        let out = render(&jobs);
        assert!(out.contains("archs: x86_64, aarch64, s390x"));
        assert!(out.contains("passed: 4"));
        assert!(out.contains("total: 4"));
        assert!(out.contains("(3 arches)"));
        assert!(!out.contains("arch: x86_64"));
        assert!(!out.contains("arch: aarch64"));
        assert!(!out.contains("arch: s390x"));
    }

    #[test]
    fn pretty_print_does_not_fold_problem_groups() {
        let jobs = vec![
            incident_job_v(
                9000,
                "t-pass",
                "passed",
                "15-SP5",
                "Server-DVD-Incidents",
                "x86_64",
            ),
            incident_job_v(
                9001,
                "t-fail",
                "failed",
                "15-SP5",
                "Server-DVD-Incidents",
                "aarch64",
            ),
        ];
        let out = render(&jobs);
        assert!(out.contains("arch: aarch64"));
        assert!(!out.contains("arch: x86_64"));
        assert!(out.contains("archs: x86_64"));
    }

    #[test]
    fn pretty_print_failed_jobs_grouped() {
        let jobs = vec![
            aggregate_job(10000, "test-alpha", "failed", "P", "20240101-1", "x86_64"),
            aggregate_job(
                10001,
                "test-beta-longer",
                "failed",
                "P",
                "20240101-1",
                "x86_64",
            ),
        ];
        let out = render(&jobs);
        assert!(out.contains("P / x86_64 (2 failed):"));
        let lines: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("test-alpha") || l.contains("test-beta-longer"))
            .collect();
        assert_eq!(lines.len(), 2);
        let url_offsets: Vec<usize> = lines.iter().map(|l| l.find(OPENQA_HOST).unwrap()).collect();
        assert_eq!(url_offsets[0], url_offsets[1]);
        let after = out.split("Failed jobs:").nth(1).unwrap();
        assert!(!after.contains("product: P - "));
    }

    #[test]
    fn has_passed_install_jobs_tolerates_missing_test_field() {
        let jobs = vec![
            NormalizedJob::from_normalized(&json!({"test": null, "result": "passed"})),
            NormalizedJob::from_normalized(
                &json!({"test": "qam-incidentinstall", "result": "passed"}),
            ),
        ];
        assert!(DashboardAutoOpenQA::has_passed_install_jobs(&jobs));
    }

    #[test]
    fn has_passed_install_jobs_counts_slfo_jobs() {
        let passed = vec![NormalizedJob::from_normalized(
            &json!({"test": "qam-incidentinstall-SLFO", "result": "passed"}),
        )];
        assert!(DashboardAutoOpenQA::has_passed_install_jobs(&passed));
        let failed = vec![NormalizedJob::from_normalized(
            &json!({"test": "qam-incidentinstall-SLFO", "result": "failed"}),
        )];
        assert!(!DashboardAutoOpenQA::has_passed_install_jobs(&failed));
    }

    fn dashboard_for_urls() -> DashboardAutoOpenQA {
        let http = crate::http::HttpClient::new(crate::http::VerifyPolicy::Default(true)).unwrap();
        let client = QemDashboardClient::with_client(http, "https://d/api");
        DashboardAutoOpenQA {
            host: OPENQA_HOST.to_string(),
            rrid: "SUSE:Maintenance:12358:199773".parse().unwrap(),
            client,
            incident_number: "12358".to_string(),
            pp: Vec::new(),
            results: None,
            jobs: Vec::new(),
        }
    }

    #[test]
    fn get_logs_url_tolerates_missing_test_field() {
        let dashboard = dashboard_for_urls();
        let jobs = vec![
            NormalizedJob::from_normalized(&json!({"test": null, "result": "passed", "id": 1})),
            NormalizedJob::from_normalized(&json!({
                "test": "qam-incidentinstall",
                "result": "passed",
                "id": 2,
                "settings": {"DISTRI": "sle", "ARCH": "x86_64", "VERSION": "15-SP5"},
            })),
        ];
        let urls = dashboard.get_logs_url(&jobs).unwrap();
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn get_logs_url_includes_slfo_jobs_with_slfo_logfile() {
        let dashboard = dashboard_for_urls();
        let jobs = vec![
            NormalizedJob::from_normalized(&json!({
                "test": "qam-incidentinstall-SLFO",
                "result": "passed",
                "id": 10,
                "settings": {"DISTRI": "sle", "ARCH": "x86_64", "VERSION": "16.0"},
            })),
            NormalizedJob::from_normalized(&json!({
                "test": "qam-incidentinstall",
                "result": "passed",
                "id": 11,
                "settings": {"DISTRI": "sle", "ARCH": "x86_64", "VERSION": "15-SP5"},
            })),
        ];
        let urls = dashboard.get_logs_url(&jobs).unwrap();
        assert_eq!(urls.len(), 2);
        let by_id: std::collections::HashMap<&str, &str> = urls
            .iter()
            .map(|u| {
                let id = u
                    .url
                    .split("/tests/")
                    .nth(1)
                    .unwrap()
                    .split('/')
                    .next()
                    .unwrap();
                (id, u.url.as_str())
            })
            .collect();
        assert!(by_id["10"].ends_with("SLFO_update_install-zypper.log"));
        assert!(by_id["11"].ends_with("update_install-zypper.log"));
    }

    // --- insta snapshots of full rendered output (main scenarios) ---

    #[test]
    fn snapshot_collapses_passed() {
        let jobs: Vec<_> = (0..5)
            .map(|i| incident_job(2000 + i, &format!("qam-test-{i}"), "passed"))
            .collect();
        insta::assert_snapshot!(render(&jobs));
    }

    #[test]
    fn snapshot_lists_failed() {
        let jobs = vec![
            incident_job(3000, "qam-pass", "passed"),
            incident_job(3100, "qam-failure", "failed"),
            incident_job(3101, "qam-incomplete", "incomplete"),
        ];
        insta::assert_snapshot!(render(&jobs));
    }

    #[test]
    fn snapshot_other_bucket_needs_review() {
        // Problem group present (an `other`-bucket still-running job) but no
        // failed job: renders the "need review" note, never "All jobs passed."
        let jobs = vec![
            incident_job(2500, "qam-pass", "passed"),
            incident_job(2501, "qam-running", "none"),
        ];
        insta::assert_snapshot!(render(&jobs));
    }

    #[test]
    fn snapshot_obsoleted_excluded() {
        // Obsoleted runs are dropped before rendering; only the current passed
        // run remains, so the block reports a clean pass with no phantom failure.
        let settings = json!({
            "DISTRI": "sle", "FLAVOR": "Server-DVD-Incidents",
            "ARCH": "x86_64", "VERSION": "15-SP5", "BUILD": ":12358:bash",
        });
        let jobs: Vec<_> = [
            json!({"test": "qam-incidentinstall", "result": "failed", "obsolete": true,
                   "source": "incident", "id": 1, "settings": settings}),
            json!({"test": "qam-incidentinstall", "result": "obsoleted",
                   "source": "incident", "id": 2, "settings": settings}),
            json!({"test": "qam-incidentinstall", "result": "passed",
                   "source": "incident", "id": 3, "settings": settings}),
        ]
        .iter()
        .map(NormalizedJob::from_normalized)
        .filter(|j| !j.is_obsolete())
        .collect();
        insta::assert_snapshot!(render(&jobs));
    }

    #[test]
    fn snapshot_aggregate_grouping() {
        let jobs = vec![
            aggregate_job(
                4000,
                "mau-a",
                "passed",
                "SLES-15-SP5",
                "20240101-1",
                "x86_64",
            ),
            aggregate_job(
                4001,
                "mau-b",
                "passed",
                "SLES-15-SP5",
                "20240101-1",
                "aarch64",
            ),
            aggregate_job(
                4002,
                "mau-c",
                "failed",
                "SLES-15-SP6",
                "20240101-1",
                "x86_64",
            ),
        ];
        insta::assert_snapshot!(render(&jobs));
    }

    // --- load_jobs fan-out: ordering + timeout skip (async timeout seam) ---

    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn dashboard_against(server: &MockServer) -> DashboardAutoOpenQA {
        let http = crate::http::HttpClient::new(crate::http::VerifyPolicy::Default(true)).unwrap();
        let client = QemDashboardClient::with_client(http, format!("{}/api", server.uri()));
        DashboardAutoOpenQA {
            host: OPENQA_HOST.to_string(),
            rrid: "SUSE:Maintenance:12358:199773".parse().unwrap(),
            client,
            incident_number: "12358".to_string(),
            pp: Vec::new(),
            results: None,
            jobs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn load_jobs_fans_out_and_preserves_order() {
        // Ported from test_dashboard_auto_openqa_fans_out_per_setting_fetches:
        // incident jobs (in setting order) come before aggregate jobs, and each
        // per-setting URL is hit exactly once.
        let server = MockServer::start().await;
        let incident_ids = [11i64, 12, 13];
        let update_ids = [21i64, 22, 23];

        Mock::given(method("GET"))
            .and(path("/api/incident_settings/12358"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    incident_ids
                        .iter()
                        .map(|sid| json!({"id": sid, "settings": {}}))
                        .collect::<Vec<_>>(),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/update_settings/12358"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    update_ids
                        .iter()
                        .map(|sid| json!({"id": sid, "settings": {}}))
                        .collect::<Vec<_>>(),
                ),
            )
            .mount(&server)
            .await;
        for sid in incident_ids {
            Mock::given(method("GET"))
                .and(path(format!("/api/jobs/incident/{sid}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!([{"job_id": 1000 + sid, "name": format!("qam-incident-{sid}"), "status": "passed"}]),
                ))
                .expect(1)
                .mount(&server)
                .await;
        }
        for sid in update_ids {
            Mock::given(method("GET"))
                .and(path(format!("/api/jobs/update/{sid}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!([{"job_id": 2000 + sid, "name": format!("mau-update-{sid}"), "status": "passed"}]),
                ))
                .expect(1)
                .mount(&server)
                .await;
        }

        let mut dashboard = dashboard_against(&server);
        dashboard.run().await;

        let expected: Vec<String> = incident_ids
            .iter()
            .map(|sid| format!("qam-incident-{sid}"))
            .chain(update_ids.iter().map(|sid| format!("mau-update-{sid}")))
            .collect();
        assert_eq!(dashboard.job_test_names(), expected);
        // `.expect(1)` on each mock asserts exactly-once on server drop.
    }

    #[tokio::test]
    async fn load_jobs_drops_obsoleted_runs_and_keeps_verdict() {
        // A retriggered install scenario: the dashboard keeps an older superseded
        // run (an `obsolete` flag on a stale `failed`, and a separate stale run
        // marked with `status: "obsoleted"`) alongside the current `passed` run.
        // The obsoleted runs must be dropped so they neither appear in the job
        // list nor poison the install verdict — `results` must be `Some`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/12358"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"id": 11, "settings": {"DISTRI": "sle"}}])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/update_settings/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/jobs/incident/11"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                // Stale failed run, superseded via the `obsolete` flag.
                {"job_id": 1, "name": "qam-incidentinstall", "status": "failed", "obsolete": true},
                // Stale run superseded via an `obsoleted` result.
                {"job_id": 2, "name": "qam-incidentinstall", "status": "obsoleted"},
                // Current run that actually passed.
                {"job_id": 3, "name": "qam-incidentinstall", "status": "passed"},
            ])))
            .mount(&server)
            .await;

        let mut dashboard = dashboard_against(&server);
        dashboard.run().await;

        // Only the current run survives.
        assert_eq!(
            dashboard.job_test_names(),
            vec!["qam-incidentinstall".to_string()]
        );
        // The stale failed run no longer poisons the verdict.
        let results = dashboard.results.expect("install verdict should be Some");
        assert_eq!(results.len(), 1);
        assert!(results[0].url.contains("/tests/3/"));
        // No phantom failed entry in the rendered block.
        let rendered = dashboard.pp.concat();
        assert!(!rendered.contains("Failed jobs:"));
        assert!(rendered.contains("All jobs passed."));
    }

    #[tokio::test]
    async fn load_jobs_skips_timed_out_per_setting_future() {
        // Ported from test_load_jobs_skips_timed_out_per_setting_future.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": 11, "settings": {}},
                {"id": 12, "settings": {}},
                {"id": 13, "settings": {}}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/update_settings/12358"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        for sid in [11i64, 13] {
            Mock::given(method("GET"))
                .and(path(format!("/api/jobs/incident/{sid}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!([{"job_id": 1000 + sid, "name": format!("qam-{sid}"), "status": "passed"}]),
                ))
                .mount(&server)
                .await;
        }
        // Setting 12 hangs past the short timeout.
        Mock::given(method("GET"))
            .and(path("/api/jobs/incident/12"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(
                        json!([{"job_id": 9999, "name": "should-not-appear", "status": "passed"}]),
                    ),
            )
            .mount(&server)
            .await;

        let mut dashboard = dashboard_against(&server);
        dashboard.run_with_timeout(Duration::from_millis(50)).await;

        let names = dashboard.job_test_names();
        assert!(names.contains(&"qam-11".to_string()));
        assert!(names.contains(&"qam-13".to_string()));
        assert!(!names.contains(&"should-not-appear".to_string()));
    }

    #[tokio::test]
    async fn load_jobs_top_level_settings_timeout_returns_empty() {
        // Ported from test_load_jobs_top_level_settings_timeout_returns_empty.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/incident_settings/12358"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(json!([{"id": 1, "settings": {}}])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/update_settings/12358"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .set_body_json(json!([{"id": 2, "settings": {}}])),
            )
            .mount(&server)
            .await;

        let mut dashboard = dashboard_against(&server);
        dashboard.run_with_timeout(Duration::from_millis(50)).await;

        assert!(dashboard.job_test_names().is_empty());
        assert!(dashboard.pp.is_empty());
    }
}
