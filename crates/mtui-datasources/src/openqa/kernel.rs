//! The kernel openQA connector, ported from
//! `mtui/data_sources/openqa/kernel.py`.
//!
//! `KernelOpenQA` filters jobs to the kernel flavor, parses each into a
//! [`Test`], and renders a result matrix restricted to LTP (`ltp_`-prefixed)
//! tests.

use std::collections::BTreeMap;

use mtui_types::{OpenQAResult, Test};

use super::base::{Job, OpenQABase};
use crate::error::OpenQAError;
use crate::http::sanitize_url;

/// Job results that are excluded from the parsed test list (not real outcomes).
const EXCLUDED_RESULTS: &[&str] = &[
    "skipped",
    "user_cancelled",
    "incomplete",
    "user_restarted",
    "obsoleted",
];

/// Module names dropped from every parsed test's module map (boilerplate).
const EXCLUDED_MODULES: &[&str] = &["boot_ltp", "shutdown_ltp"];

/// The kernel openQA connector.
///
/// After [`run`](KernelOpenQA::run), [`results`](KernelOpenQA::results) holds
/// the parsed [`Test`] list and [`pp`](KernelOpenQA::pp) holds the rendered
/// result matrix.
#[derive(Debug, Clone)]
pub struct KernelOpenQA {
    base: OpenQABase,
    pp: Vec<String>,
    results: Option<Vec<Test>>,
}

impl KernelOpenQA {
    /// The workflow discriminator.
    const KIND: &'static str = "kernel";

    /// Wrap shared connector state as a kernel connector (unpopulated).
    #[must_use]
    pub fn new(base: OpenQABase) -> Self {
        Self {
            base,
            pp: Vec::new(),
            results: None,
        }
    }

    /// The parsed test results.
    #[must_use]
    pub fn results(&self) -> Option<&[Test]> {
        self.results.as_deref()
    }

    /// The rendered result matrix.
    #[must_use]
    pub fn pp(&self) -> &[String] {
        &self.pp
    }

    /// The openQA instance host this connector targets.
    ///
    /// Exposes the shared-base host so the export downloader can build the
    /// per-instance log URLs (upstream reads `host.host`).
    #[must_use]
    pub fn host(&self) -> &str {
        self.base.host()
    }

    /// Fetch jobs, filter to kernel jobs, parse, and render.
    ///
    /// Mirrors upstream `run`.
    ///
    /// # Errors
    ///
    /// Returns [`OpenQAError::Fetch`] when the jobs fetch fails (openQA
    /// unreachable / non-2xx / malformed body), so the caller can tell an
    /// unreachable instance apart from a genuinely-empty result. A
    /// valid-but-empty response is `Ok` with no results.
    pub async fn run(mut self) -> Result<Self, OpenQAError> {
        let jobs = self.base.try_get_jobs().await?;
        let filtered = Self::filter_jobs(Some(&jobs));
        self.results = Self::parse_jobs(filtered.as_deref());
        self.pp = self.pretty_print();
        Ok(self)
    }

    /// Keep only kernel-flavor jobs.
    ///
    /// Mirrors upstream `_filter_jobs`: `None` → `None`; else jobs whose
    /// `FLAVOR` (lowercased, split on `-`) contains the segment `kernel`.
    #[must_use]
    fn filter_jobs(jobs: Option<&[Job]>) -> Option<Vec<Job>> {
        let jobs = jobs?;
        Some(
            jobs.iter()
                .filter(|j| {
                    j.setting("FLAVOR")
                        .to_lowercase()
                        .split('-')
                        .any(|seg| seg == "kernel")
                })
                .cloned()
                .collect(),
        )
    }

    /// Parse kernel jobs into [`Test`] objects.
    ///
    /// Mirrors upstream `_parse_jobs`: `None` → `None`; else one [`Test`] per
    /// non-cloned job (dropping [`EXCLUDED_MODULES`] from each module map),
    /// excluding any whose result is in [`EXCLUDED_RESULTS`].
    #[must_use]
    fn parse_jobs(jobs: Option<&[Job]>) -> Option<Vec<Test>> {
        let jobs = jobs?;
        Some(
            jobs.iter()
                .filter(|j| j.clone_id.is_none())
                .map(|j| {
                    let modules: BTreeMap<String, String> = j
                        .modules
                        .iter()
                        .filter(|m| !EXCLUDED_MODULES.contains(&m.name.as_str()))
                        .map(|m| (m.name.clone(), m.result.clone()))
                        .collect();
                    Test::new(&j.test, &j.result, j.id, j.setting("ARCH"), modules)
                })
                .filter(|t| !EXCLUDED_RESULTS.contains(&t.result.as_str()))
                .collect(),
        )
    }

    /// Render the pretty-printed report (host header + result matrix).
    ///
    /// Mirrors upstream `_pretty_print`: empty when the connector has no
    /// results.
    #[must_use]
    fn pretty_print(&self) -> Vec<String> {
        if !self.has_results() {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "openQA instance: {} :\n",
            sanitize_url(self.base.host())
        )];
        if let Some(results) = &self.results {
            lines.extend(Self::result_matrix(results));
        }
        lines
    }

    /// Format the LTP test results into a sorted matrix.
    ///
    /// Mirrors upstream `_result_matrix`: only `ltp_`-prefixed tests produce a
    /// line; a failed LTP test appends a `<module>: ...` line per failed
    /// module. Column widths match the upstream `str.format` field specs
    /// (`{0:36}`, `{1:<3}`, `{2:8}`). The result is sorted.
    #[must_use]
    fn result_matrix(testresults: &[Test]) -> Vec<String> {
        let mut matrix: Vec<String> = Vec::new();
        for test in testresults {
            if !test.name.starts_with("ltp_") {
                continue;
            }
            // "  test: {name:36} {-:<3}arch: {arch:8} {-:<3}result: {result}\n"
            let mut text = format!(
                "  test: {:<36} {:<3}arch: {:<8} {:<3}result: {}\n",
                test.name, "-", test.arch, "-", test.result
            );
            if test.result == "failed" {
                text = text.replace("failed", "failed:");
                for (module, result) in &test.modules {
                    if result == "failed" {
                        text.push_str(&format!("\n      {module}: ...\n"));
                    }
                }
            }
            matrix.push(text);
        }
        matrix.sort();
        matrix
    }
}

impl OpenQAResult for KernelOpenQA {
    fn kind(&self) -> &str {
        Self::KIND
    }

    /// The port of upstream `KernelOpenQA.__bool__`: truthy when `pp` or
    /// `results` is non-empty. Called during `pretty_print`, where `pp` is not
    /// yet populated, so it effectively keys on `results`.
    fn has_results(&self) -> bool {
        !self.pp.is_empty() || self.results.as_ref().is_some_and(|r| !r.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::super::base::tests::{MockIncident, dummy_client};
    use super::super::base::{Job, JobModule, OpenQABase};
    use super::*;
    use mtui_types::RequestReviewID;
    use std::collections::BTreeMap;

    fn base() -> OpenQABase {
        let rrid = RequestReviewID::parse("SUSE:Maintenance:1:1").unwrap();
        OpenQABase::new(dummy_client(), &rrid, &MockIncident::new("bash"))
    }

    fn job(
        id: i64,
        test: &str,
        result: &str,
        flavor: &str,
        arch: &str,
        clone_id: Option<i64>,
        modules: &[(&str, &str)],
    ) -> Job {
        let mut settings = BTreeMap::new();
        settings.insert("FLAVOR".to_string(), flavor.to_string());
        settings.insert("ARCH".to_string(), arch.to_string());
        Job {
            id,
            test: test.into(),
            result: result.into(),
            clone_id,
            settings,
            modules: modules
                .iter()
                .map(|(n, r)| JobModule {
                    name: (*n).into(),
                    category: String::new(),
                    result: (*r).into(),
                })
                .collect(),
        }
    }

    // filter_jobs

    #[test]
    fn filter_jobs_none_is_none() {
        assert!(KernelOpenQA::filter_jobs(None).is_none());
    }

    #[test]
    fn filter_jobs_keeps_only_kernel_flavor() {
        let jobs = vec![
            job(
                1,
                "t",
                "passed",
                "SLES-15-SP5-Kernel-x86_64",
                "x86_64",
                None,
                &[],
            ),
            job(
                2,
                "t",
                "passed",
                "SLES-15-SP5-Server-x86_64",
                "x86_64",
                None,
                &[],
            ),
        ];
        let out = KernelOpenQA::filter_jobs(Some(&jobs)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 1);
    }

    // parse_jobs

    #[test]
    fn parse_jobs_none_is_none() {
        assert!(KernelOpenQA::parse_jobs(None).is_none());
    }

    #[test]
    fn parse_jobs_drops_cloned_and_excluded_results_and_modules() {
        let jobs = vec![
            job(
                1,
                "ltp_a",
                "passed",
                "Kernel",
                "x86_64",
                None,
                &[
                    ("real_mod", "passed"),
                    ("boot_ltp", "passed"),
                    ("shutdown_ltp", "passed"),
                ],
            ),
            // cloned -> dropped
            job(2, "ltp_b", "passed", "Kernel", "x86_64", Some(99), &[]),
            // excluded result -> dropped
            job(3, "ltp_c", "skipped", "Kernel", "x86_64", None, &[]),
        ];
        let out = KernelOpenQA::parse_jobs(Some(&jobs)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].test_id, 1);
        // boot_ltp / shutdown_ltp stripped, real_mod kept
        assert!(out[0].modules.contains_key("real_mod"));
        assert!(!out[0].modules.contains_key("boot_ltp"));
        assert!(!out[0].modules.contains_key("shutdown_ltp"));
    }

    // result_matrix / pretty_print

    #[test]
    fn result_matrix_only_ltp_tests() {
        let mut m = BTreeMap::new();
        m.insert("mod".to_string(), "passed".to_string());
        let tests = vec![
            Test::new("ltp_syscalls", "passed", 1, "x86_64", BTreeMap::new()),
            Test::new("qam-other", "passed", 2, "x86_64", m),
        ];
        let matrix = KernelOpenQA::result_matrix(&tests);
        assert_eq!(matrix.len(), 1);
        assert!(matrix[0].contains("ltp_syscalls"));
        assert!(matrix[0].contains("arch: x86_64"));
        assert!(matrix[0].contains("result: passed"));
    }

    #[test]
    fn result_matrix_failed_ltp_appends_failed_modules() {
        let mut mods = BTreeMap::new();
        mods.insert("badmod".to_string(), "failed".to_string());
        mods.insert("okmod".to_string(), "passed".to_string());
        let tests = vec![Test::new("ltp_fs", "failed", 1, "x86_64", mods)];
        let matrix = KernelOpenQA::result_matrix(&tests);
        assert_eq!(matrix.len(), 1);
        assert!(matrix[0].contains("result: failed:"));
        assert!(matrix[0].contains("badmod: ..."));
        assert!(!matrix[0].contains("okmod: ..."));
    }

    #[test]
    fn result_matrix_is_sorted() {
        let tests = vec![
            Test::new("ltp_zzz", "passed", 1, "x86_64", BTreeMap::new()),
            Test::new("ltp_aaa", "passed", 2, "x86_64", BTreeMap::new()),
        ];
        let matrix = KernelOpenQA::result_matrix(&tests);
        assert!(matrix[0].contains("ltp_aaa"));
        assert!(matrix[1].contains("ltp_zzz"));
    }

    #[test]
    fn pretty_print_empty_without_results() {
        let k = KernelOpenQA::new(base());
        assert!(k.pretty_print().is_empty());
    }

    #[test]
    fn pretty_print_has_host_header_and_matrix() {
        let mut k = KernelOpenQA::new(base());
        k.results = Some(vec![Test::new(
            "ltp_x",
            "passed",
            1,
            "x86_64",
            BTreeMap::new(),
        )]);
        let out = k.pretty_print();
        assert!(out[0].contains("openQA instance: https://openqa.example.com"));
        assert!(out.iter().any(|l| l.contains("ltp_x")));
    }

    #[test]
    fn has_results_and_kind() {
        let mut k = KernelOpenQA::new(base());
        assert!(!k.has_results());
        k.results = Some(vec![Test::new(
            "ltp_x",
            "passed",
            1,
            "x86_64",
            BTreeMap::new(),
        )]);
        assert!(k.has_results());
        assert_eq!(k.kind(), "kernel");
    }
}
