//! Authoring for the automatic workflow: `testing.openqa.install` from the
//! dashboard's install-job results.
//!
//! Mirrors `export::auto`'s verdict rule (P4-D3), extracted from
//! `AutoExport::install_status` rather than re-derived.

use mtui_datasources::qem_dashboard::DashboardAutoOpenQA;
use mtui_types::URLs;
use mtui_types::report_document::{OpenqaInstall, OpenqaJob, OpenqaVerdict, Req};

/// Builds `testing.openqa.install` from the auto connector's install-job
/// results.
///
/// `None` when there are no results yet (not run / running / unfetchable) —
/// per the step 1 gap analysis, that "unknown" state has no `OpenqaVerdict`
/// value of its own, so the whole object is omitted rather than emitted with
/// an invented verdict (`Openqa.install: Option<OpenqaInstall>`).
#[must_use]
pub fn openqa_install_from_auto(auto: Option<&DashboardAutoOpenQA>) -> Option<OpenqaInstall> {
    let results = auto.and_then(|a| a.results.as_deref())?;
    if results.is_empty() {
        return None;
    }

    let verdict = if results
        .iter()
        .all(|r| r.result == "passed" || r.result == "softfailed")
    {
        OpenqaVerdict::Passed
    } else {
        OpenqaVerdict::Failed
    };

    Some(OpenqaInstall {
        verdict: Req(Some(verdict)),
        jobs: results.iter().map(openqa_job).collect(),
    })
}

/// One `URLs` result as a schema `openqa_install.jobs[]` entry.
fn openqa_job(result: &URLs) -> OpenqaJob {
    OpenqaJob {
        scenario: format!("{}_{}_{}", result.distri, result.version, result.arch),
        result: result.result.clone(),
        job: job_id_from_url(&result.url),
    }
}

/// Extracts the numeric job id from an openQA test URL
/// (`.../tests/<id>/...` or `.../tests/<id>`), if present.
fn job_id_from_url(url: &str) -> Option<i64> {
    let after = url.split_once("/tests/")?.1;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use mtui_datasources::{QemDashboardClient, QemIncident, VerifyPolicy};
    use mtui_types::{RequestReviewID, UpdateSource};

    use super::*;

    fn urls(result: &str) -> URLs {
        URLs::new(
            "SLES",
            "x86_64",
            "15-SP5",
            "https://oqa/tests/42/file/log.txt",
            result,
        )
    }

    fn seeded_auto(results: Option<Vec<URLs>>) -> DashboardAutoOpenQA {
        let rrid: RequestReviewID = "SUSE:Maintenance:1:2".parse().unwrap();
        let client =
            QemDashboardClient::new("http://dashboard.invalid/api", VerifyPolicy::Default(false))
                .expect("client builds");
        let incident = QemIncident {
            rrid: rrid.clone(),
            incident_number: "1".to_string(),
            source: UpdateSource::Obs,
            client,
            data: None,
        };
        let mut auto = DashboardAutoOpenQA::new("http://oqa.invalid", &incident, rrid, 1);
        auto.results = results;
        auto
    }

    #[test]
    fn no_auto_omits_the_object() {
        assert!(openqa_install_from_auto(None).is_none());
    }

    #[test]
    fn no_results_yet_omits_the_object() {
        let auto = seeded_auto(None);
        assert!(openqa_install_from_auto(Some(&auto)).is_none());
    }

    #[test]
    fn empty_results_omits_the_object() {
        let auto = seeded_auto(Some(vec![]));
        assert!(openqa_install_from_auto(Some(&auto)).is_none());
    }

    #[test]
    fn all_passed_or_softfailed_is_passed() {
        let auto = seeded_auto(Some(vec![urls("passed"), urls("softfailed")]));
        let install = openqa_install_from_auto(Some(&auto)).unwrap();
        assert_eq!(*install.verdict, Some(OpenqaVerdict::Passed));
        assert_eq!(install.jobs.len(), 2);
    }

    #[test]
    fn any_failure_is_failed() {
        let auto = seeded_auto(Some(vec![urls("passed"), urls("failed")]));
        let install = openqa_install_from_auto(Some(&auto)).unwrap();
        assert_eq!(*install.verdict, Some(OpenqaVerdict::Failed));
    }

    #[test]
    fn job_carries_scenario_result_and_id() {
        let auto = seeded_auto(Some(vec![urls("passed")]));
        let install = openqa_install_from_auto(Some(&auto)).unwrap();
        let job = &install.jobs[0];
        assert_eq!(job.scenario, "SLES_15-SP5_x86_64");
        assert_eq!(job.result, "passed");
        assert_eq!(job.job, Some(42));
    }

    #[test]
    fn job_id_from_url_parses_the_tests_segment() {
        assert_eq!(job_id_from_url("https://oqa/tests/7"), Some(7));
        assert_eq!(job_id_from_url("https://oqa/tests/7/file/log.txt"), Some(7));
        assert_eq!(job_id_from_url("https://oqa/nope"), None);
    }
}
