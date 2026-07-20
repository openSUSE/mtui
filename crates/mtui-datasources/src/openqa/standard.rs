//! The standard "auto" openQA connector, ported from
//! `mtui/data_sources/openqa/standard.py`.
//!
//! `AutoOpenQA` checks whether all install jobs passed; if so, it collects the
//! install-log URLs, otherwise it reports no results. Either way it produces a
//! human-readable pretty-print of every job.

use mtui_types::{OpenQAResult, URLs};

use super::base::{Job, OpenQABase};
use super::install::install_logfile_for;
use crate::error::OpenQAError;

/// The openQA install-test job names the auto workflow tracks.
const INSTALL_JOB_NAMES: &[&str] = &[
    "qam-incidentinstall",
    "qam-incidentinstall-ha",
    "qam-incidentinstall-SLFO",
];

/// The standard "auto" openQA connector.
///
/// After [`run`](AutoOpenQA::run), [`results`](AutoOpenQA::results) holds the
/// install-log URLs (when all install jobs passed) or `None`, and
/// [`pp`](AutoOpenQA::pp) holds the pretty-printed job report.
#[derive(Debug, Clone)]
pub struct AutoOpenQA {
    base: OpenQABase,
    pp: Vec<String>,
    results: Option<Vec<URLs>>,
}

impl AutoOpenQA {
    /// The workflow discriminator.
    pub const KIND: &'static str = "auto";

    /// Wrap shared connector state as an auto connector (unpopulated).
    #[must_use]
    pub fn new(base: OpenQABase) -> Self {
        Self {
            base,
            pp: Vec::new(),
            results: None,
        }
    }

    /// The install-log URLs, populated by [`run`](Self::run) when all install
    /// jobs passed.
    #[must_use]
    pub fn results(&self) -> Option<&[URLs]> {
        self.results.as_deref()
    }

    /// The pretty-printed job report.
    #[must_use]
    pub fn pp(&self) -> &[String] {
        &self.pp
    }

    /// Fetch jobs and process them into results + pretty-print.
    ///
    /// Mirrors upstream `run`: results are the install-log URLs when all install
    /// jobs passed, else `None`; the pretty-print always covers every job.
    ///
    /// # Errors
    ///
    /// Returns [`OpenQAError::Fetch`] when the jobs fetch fails (openQA
    /// unreachable / non-2xx / malformed body), so the caller can tell an
    /// unreachable instance apart from a genuinely-empty result. A
    /// valid-but-empty response is `Ok`.
    pub async fn run(mut self) -> Result<Self, OpenQAError> {
        let jobs = self.base.try_get_jobs().await?;
        self.results = if Self::has_passed_install_jobs(Some(&jobs)) {
            self.get_logs_url(Some(&jobs))
        } else {
            None
        };
        self.pp = self.pretty_print(Some(&jobs));
        Ok(self)
    }

    /// Whether all install jobs passed (or softfailed).
    ///
    /// Mirrors upstream `_has_passed_install_jobs`: `None` jobs → `false`;
    /// otherwise every job whose name is an install job must be `passed` or
    /// `softfailed`. With no install jobs at all this is vacuously `true`
    /// (matching python `all([])`).
    #[must_use]
    fn has_passed_install_jobs(jobs: Option<&[Job]>) -> bool {
        let Some(jobs) = jobs else {
            return false;
        };
        jobs.iter()
            .filter(|j| INSTALL_JOB_NAMES.contains(&j.test.as_str()))
            .all(|j| j.result == "passed" || j.result == "softfailed")
    }

    /// Build the install-log URLs for the passing install jobs.
    ///
    /// Mirrors upstream `_get_logs_url`: `None`/empty jobs → `None`; otherwise
    /// one [`URLs`] per install job, with `distri` taken from the leading
    /// hyphen-segment of `HDD_1` and the log filename resolved by
    /// [`install_logfile_for`].
    #[must_use]
    fn get_logs_url(&self, jobs: Option<&[Job]>) -> Option<Vec<URLs>> {
        let jobs = jobs.filter(|j| !j.is_empty())?;
        Some(
            jobs.iter()
                .filter(|j| INSTALL_JOB_NAMES.contains(&j.test.as_str()))
                .map(|job| {
                    let distri = job.setting("HDD_1").split('-').next().unwrap_or("");
                    let url = format!(
                        "{}/tests/{}/file/{}",
                        self.base.host(),
                        job.id,
                        install_logfile_for(&job.test)
                    );
                    URLs::new(
                        distri,
                        job.setting("ARCH"),
                        job.setting("VERSION"),
                        url,
                        job.result.clone(),
                    )
                })
                .collect(),
        )
    }

    /// Pretty-print every job, with a "Failed modules" block per job that has
    /// failing modules.
    ///
    /// Mirrors upstream `_pretty_print`. Returns an empty vector for `None`/
    /// empty jobs.
    #[must_use]
    fn pretty_print(&self, jobs: Option<&[Job]>) -> Vec<String> {
        let Some(jobs) = jobs.filter(|j| !j.is_empty()) else {
            tracing::debug!("No job - no results");
            return Vec::new();
        };
        let mut ret: Vec<String> = vec![
            "\n".to_string(),
            "Results from openQA incidents jobs:\n".to_string(),
            "===================================\n".to_string(),
            "\n".to_string(),
        ];
        for job in jobs {
            ret.push(format!(
                "  Job in flavor: {} - arch: {} - version: {} - test: {} - result: {}\n",
                job.setting("FLAVOR"),
                job.setting("ARCH"),
                job.setting("VERSION"),
                job.test,
                job.result,
            ));
            let failed_modules: Vec<(&str, &str)> = job
                .modules
                .iter()
                .filter(|m| m.result == "failed")
                .map(|m| (m.name.as_str(), m.category.as_str()))
                .collect();
            if !failed_modules.is_empty() {
                ret.push("    Failed modules:\n".to_string());
                for (name, category) in failed_modules {
                    ret.push(format!(
                        "      Module: {name} in category {category} failed\n"
                    ));
                }
                ret.push("\n".to_string());
            }
        }
        ret
    }
}

impl OpenQAResult for AutoOpenQA {
    fn kind(&self) -> &str {
        Self::KIND
    }

    /// The port of upstream `AutoOpenQA.__bool__`: truthy when it has results.
    fn has_results(&self) -> bool {
        self.results.as_ref().is_some_and(|r| !r.is_empty())
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

    fn job(id: i64, test: &str, result: &str, settings: &[(&str, &str)]) -> Job {
        let mut map = BTreeMap::new();
        for (k, v) in settings {
            map.insert((*k).to_string(), (*v).to_string());
        }
        Job {
            id,
            test: test.into(),
            result: result.into(),
            clone_id: None,
            settings: map,
            modules: Vec::new(),
        }
    }

    // TestGetLogsUrl (ported from test_openqa_connector.py)

    #[test]
    fn get_logs_url_no_jobs_returns_none() {
        let auto = AutoOpenQA::new(base());
        assert!(auto.get_logs_url(None).is_none());
    }

    #[test]
    fn get_logs_url_empty_jobs_returns_none() {
        let auto = AutoOpenQA::new(base());
        assert!(auto.get_logs_url(Some(&[])).is_none());
    }

    #[test]
    fn get_logs_url_with_install_jobs() {
        let auto = AutoOpenQA::new(base());
        let jobs = vec![
            job(
                123,
                "qam-incidentinstall",
                "passed",
                &[
                    ("HDD_1", "SLES-15-SP5-x86_64-Build1234.qcow2"),
                    ("ARCH", "x86_64"),
                    ("VERSION", "15-SP5"),
                ],
            ),
            job(
                456,
                "qam-othertest",
                "passed",
                &[
                    ("HDD_1", "SLES-15-SP5-x86_64-Build1234.qcow2"),
                    ("ARCH", "x86_64"),
                    ("VERSION", "15-SP5"),
                ],
            ),
        ];
        let result = auto.get_logs_url(Some(&jobs)).unwrap();
        // Only the install job is included.
        assert_eq!(result.len(), 1);
        assert!(result[0].url.contains("123"));
        assert_eq!(result[0].result, "passed");
        assert_eq!(result[0].distri, "SLES");
    }

    #[test]
    fn get_logs_url_slfo_job_uses_slfo_logfile() {
        let auto = AutoOpenQA::new(base());
        let jobs = vec![job(
            789,
            "qam-incidentinstall-SLFO",
            "passed",
            &[
                ("HDD_1", "SLFO-16.0-x86_64-Build1.qcow2"),
                ("ARCH", "x86_64"),
                ("VERSION", "16.0"),
            ],
        )];
        let result = auto.get_logs_url(Some(&jobs)).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].url.contains("789"));
        assert!(result[0].url.ends_with("SLFO_update_install-zypper.log"));
    }

    // has_passed_install_jobs

    #[test]
    fn has_passed_install_jobs_none_is_false() {
        assert!(!AutoOpenQA::has_passed_install_jobs(None));
    }

    #[test]
    fn has_passed_install_jobs_all_passed_is_true() {
        let jobs = vec![job(1, "qam-incidentinstall", "passed", &[])];
        assert!(AutoOpenQA::has_passed_install_jobs(Some(&jobs)));
    }

    #[test]
    fn has_passed_install_jobs_softfailed_counts_as_passed() {
        let jobs = vec![job(1, "qam-incidentinstall", "softfailed", &[])];
        assert!(AutoOpenQA::has_passed_install_jobs(Some(&jobs)));
    }

    #[test]
    fn has_passed_install_jobs_a_failed_install_is_false() {
        let jobs = vec![
            job(1, "qam-incidentinstall", "passed", &[]),
            job(2, "qam-incidentinstall-ha", "failed", &[]),
        ];
        assert!(!AutoOpenQA::has_passed_install_jobs(Some(&jobs)));
    }

    #[test]
    fn has_passed_install_jobs_ignores_non_install_jobs() {
        let jobs = vec![
            job(1, "qam-incidentinstall", "passed", &[]),
            job(2, "qam-othertest", "failed", &[]),
        ];
        assert!(AutoOpenQA::has_passed_install_jobs(Some(&jobs)));
    }

    // pretty_print

    #[test]
    fn pretty_print_empty_is_empty() {
        let auto = AutoOpenQA::new(base());
        assert!(auto.pretty_print(None).is_empty());
        assert!(auto.pretty_print(Some(&[])).is_empty());
    }

    #[test]
    fn pretty_print_lists_jobs_and_failed_modules() {
        let auto = AutoOpenQA::new(base());
        let mut j = job(
            1,
            "qam-incidentinstall",
            "failed",
            &[
                ("FLAVOR", "Server-DVD"),
                ("ARCH", "x86_64"),
                ("VERSION", "15-SP5"),
            ],
        );
        j.modules.push(JobModule {
            name: "mod_x".into(),
            category: "cat_y".into(),
            result: "failed".into(),
        });
        let out = auto.pretty_print(Some(&[j]));
        let joined = out.join("");
        assert!(joined.contains("Results from openQA incidents jobs:"));
        assert!(joined.contains("Job in flavor: Server-DVD"));
        assert!(joined.contains("Failed modules:"));
        assert!(joined.contains("Module: mod_x in category cat_y failed"));
    }

    // OpenQAResult / has_results

    #[test]
    fn has_results_reflects_populated_results() {
        let mut auto = AutoOpenQA::new(base());
        assert!(!auto.has_results());
        auto.results = Some(vec![URLs::new("SLES", "x86_64", "15-SP5", "u", "passed")]);
        assert!(auto.has_results());
        assert_eq!(auto.kind(), "auto");
    }
}
