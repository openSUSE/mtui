//! Exporter for the automatic workflow.
//!
//! Ports `mtui.update_workflow.export.auto.AutoExport`. It renders the openQA
//! install-test job lines, computes the install status, downloads each passing
//! job's install log, and runs the shared base sequence.
//!
//! The per-log HTTP download goes through the [`BytesFetcher`] seam (an
//! [`HttpClient`](mtui_datasources::http::HttpClient) in production, a mock in
//! tests), mirroring how the downloader is tested.

use mtui_datasources::OpenQAOverviewResult;
use mtui_datasources::qem_dashboard::DashboardAutoOpenQA;
use mtui_types::URLs;

use super::base::{ExportContext, OverwritePrompt};
use super::downloader::BytesFetcher;

/// The automatic-workflow exporter.
pub struct AutoExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The QEM-dashboard "auto" openQA connector, `None` when unpopulated.
    pub auto: Option<DashboardAutoOpenQA>,
    /// The openqa_overview payload, if the overview command ran.
    pub overview: Option<OpenQAOverviewResult>,
}

impl AutoExport {
    /// Builds an auto exporter over `ctx`.
    #[must_use]
    pub fn new(
        ctx: ExportContext,
        auto: Option<DashboardAutoOpenQA>,
        overview: Option<OpenQAOverviewResult>,
    ) -> Self {
        Self {
            ctx,
            auto,
            overview,
        }
    }

    /// Renders one install-job line (upstream `_install_job_line`).
    ///
    /// `"{distri}_{version}_{arch} => {STATUS}: {job_url}\n"`, where `job_url`
    /// is the log URL truncated at `/file/` and `STATUS` is the uppercased
    /// result (or `UNKNOWN` when empty).
    #[must_use]
    fn install_job_line(result: &URLs) -> String {
        let status = if result.result.is_empty() {
            "UNKNOWN".to_string()
        } else {
            result.result.to_uppercase()
        };
        let job_url = result
            .url
            .rsplit_once("/file/")
            .map_or(result.url.as_str(), |(head, _)| head);
        format!(
            "{}_{}_{} => {status}: {job_url}\n",
            result.distri, result.version, result.arch
        )
    }

    /// The overall install status (upstream `_install_status`).
    ///
    /// `UNKNOWN` when there are no results (not run / running / unfetchable);
    /// `PASSED` when every result is `passed`/`softfailed`; else `FAILED`.
    #[must_use]
    fn install_status(&self) -> &'static str {
        let results = self.auto.as_ref().and_then(|a| a.results.as_deref());
        match results {
            None => "UNKNOWN",
            Some([]) => "UNKNOWN",
            Some(rs) => {
                if rs
                    .iter()
                    .all(|r| r.result == "passed" || r.result == "softfailed")
                {
                    "PASSED"
                } else {
                    "FAILED"
                }
            }
        }
    }

    /// Inserts/replaces the `Install tests:` block (upstream `install_results`).
    pub fn install_results(&mut self) {
        let status_line = format!(
            "Installation tests done in openQA with following results: {}\n",
            self.install_status()
        );
        let result_lines: Vec<String> = self
            .auto
            .as_ref()
            .and_then(|a| a.results.as_deref())
            .unwrap_or(&[])
            .iter()
            .map(Self::install_job_line)
            .collect();

        let template = &mut self.ctx.template;

        // Find the block start ("Install tests:" header minus one), or append a
        // fresh header near the end (before any sysinfo footer).
        let start = match template.iter().position(|l| l == "Install tests:\n") {
            Some(idx) => idx - 1,
            None => {
                let mut start = template.len();
                if template
                    .last()
                    .is_some_and(|l| l.contains("## export MTUI:"))
                {
                    start -= 1;
                }
                let header = [
                    "##############\n",
                    "Install tests:\n",
                    "##############\n",
                    "\n",
                ]
                .map(String::from);
                template.splice(start..start, header);
                start
            }
        };

        // Find the block end: before "Links for update logs:" (trimming
        // trailing blanks), else before the sysinfo footer, else end of file.
        let end = if let Some(mut end) = template
            .iter()
            .skip(start)
            .position(|l| l == "Links for update logs:\n")
            .map(|i| i + start)
        {
            while end > start && template[end - 1] == "\n" {
                end -= 1;
            }
            end
        } else if let Some(end) = template
            .iter()
            .skip(start)
            .position(|l| l.contains("## export MTUI:"))
            .map(|i| i + start)
        {
            end
        } else {
            template.len()
        };

        let mut block: Vec<String> = vec![
            "##############\n".to_string(),
            "Install tests:\n".to_string(),
            "##############\n".to_string(),
            "\n".to_string(),
            status_line,
            "\n".to_string(),
        ];
        block.extend(result_lines);
        block.push("\n".to_string());
        template.splice(start..end, block);
    }

    /// Downloads each passing job's install log and returns their filenames
    /// (upstream `get_logs` + `_openqa_installog_to_template`).
    ///
    /// Returns the written `<distri>_<version>_<arch>.log` filenames.
    pub async fn get_logs(
        &self,
        fetcher: &dyn BytesFetcher,
        prompt: &dyn OverwritePrompt,
    ) -> Vec<String> {
        let Some(auto) = &self.auto else {
            return Vec::new();
        };
        let Some(results) = auto.results.as_deref() else {
            return Vec::new();
        };

        let dir = self.ctx.install_logs_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!("Failed to create {}: {e}", dir.display());
            return Vec::new();
        }

        let mut filenames = Vec::new();
        for url in results {
            let lines = self.installog_lines(fetcher, url).await;
            if lines.is_empty() {
                continue;
            }
            let fn_name = format!(
                "{}_{}_{}.log",
                url.distri.to_lowercase(),
                url.version,
                url.arch
            );
            self.ctx.writer(&dir.join(&fn_name), &lines, prompt);
            filenames.push(fn_name);
        }
        filenames
    }

    /// Downloads one install log and returns its lines (with trailing newlines),
    /// or an empty vec on failure (upstream `_openqa_installog_to_template`).
    async fn installog_lines(&self, fetcher: &dyn BytesFetcher, url: &URLs) -> Vec<String> {
        match fetcher.get_bytes(&url.url).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                splitlines_keepends(&text)
            }
            Err(_) => {
                tracing::error!("log {} failed to download", url.url);
                Vec::new()
            }
        }
    }

    /// Runs the exporter (upstream `run`).
    ///
    /// Returns the finished template lines.
    pub async fn run(
        &mut self,
        fetcher: &dyn BytesFetcher,
        prompt: &dyn OverwritePrompt,
    ) -> Vec<String> {
        let install_logs_current = self
            .ctx
            .template
            .iter()
            .any(|l| l.contains("Installation tests done in openQA with following results:"))
            && !self.ctx.force;

        self.install_results();
        let pp: Vec<String> = self.auto.as_ref().map(|a| a.pp.clone()).unwrap_or_default();
        self.ctx.inject_openqa(&pp);
        if let Some(overview) = self.overview.clone() {
            self.ctx.inject_overview(&overview);
        }
        if !install_logs_current {
            let filenames = self.get_logs(fetcher, prompt).await;
            self.ctx.installlogs_lines(&filenames);
        }
        self.ctx.add_sysinfo();
        self.ctx.dedup_lines();
        self.ctx.template.clone()
    }
}

/// Splits `text` into lines that each keep their trailing `\n`
/// (Python `splitlines(keepends=True)` for Unix newlines).
fn splitlines_keepends(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(text[start..=i].to_string());
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(text[start..].to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mtui_config::options::Config;

    fn urls(result: &str) -> URLs {
        URLs::new(
            "SLES",
            "x86_64",
            "15-SP5",
            "https://oqa/tests/1/file/log.txt",
            result,
        )
    }

    fn ctx() -> ExportContext {
        let cfg = Config::default();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        ExportContext::new(cfg, &[], false, rrid)
    }

    /// Builds a `DashboardAutoOpenQA` with seeded `results`/`pp`, without any
    /// network I/O: `new()` only stores config, then we set the public output
    /// fields directly (as `run()` would).
    fn seeded_auto(results: Option<Vec<URLs>>, pp: Vec<String>) -> DashboardAutoOpenQA {
        use mtui_datasources::{QemDashboardClient, QemIncident, VerifyPolicy};
        let rrid: mtui_types::RequestReviewID = "SUSE:Maintenance:1:2".parse().unwrap();
        let client =
            QemDashboardClient::new("http://dashboard.invalid/api", VerifyPolicy::Default(false))
                .expect("client builds");
        let incident = QemIncident {
            rrid: rrid.clone(),
            incident_number: "1".to_string(),
            client,
            data: None,
        };
        let mut auto = DashboardAutoOpenQA::new("http://oqa.invalid", &incident, rrid);
        auto.results = results;
        auto.pp = pp;
        auto
    }

    #[test]
    fn install_job_line_truncates_at_file_and_uppercases() {
        let line = AutoExport::install_job_line(&urls("passed"));
        assert_eq!(line, "SLES_15-SP5_x86_64 => PASSED: https://oqa/tests/1\n");
    }

    #[test]
    fn install_job_line_unknown_when_result_empty() {
        let line = AutoExport::install_job_line(&urls(""));
        assert!(line.contains("=> UNKNOWN:"));
    }

    #[test]
    fn install_status_unknown_when_no_auto() {
        let ex = AutoExport::new(ctx(), None, None);
        assert_eq!(ex.install_status(), "UNKNOWN");
    }

    #[test]
    fn install_status_passed_with_seeded_auto() {
        let auto = seeded_auto(Some(vec![urls("passed"), urls("softfailed")]), vec![]);
        let ex = AutoExport::new(ctx(), Some(auto), None);
        assert_eq!(ex.install_status(), "PASSED");
    }

    #[test]
    fn install_status_failed_with_a_failing_job() {
        let auto = seeded_auto(Some(vec![urls("passed"), urls("failed")]), vec![]);
        let ex = AutoExport::new(ctx(), Some(auto), None);
        assert_eq!(ex.install_status(), "FAILED");
    }

    #[test]
    fn splitlines_keepends_preserves_newlines() {
        assert_eq!(splitlines_keepends("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(splitlines_keepends("a\nb"), vec!["a\n", "b"]);
    }

    struct OkFetcher(Vec<u8>);

    #[async_trait]
    impl BytesFetcher for OkFetcher {
        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn installog_lines_splits_downloaded_text() {
        let ex = AutoExport::new(ctx(), None, None);
        let fetcher = OkFetcher(b"line1\nline2\n".to_vec());
        let lines = ex.installog_lines(&fetcher, &urls("passed")).await;
        assert_eq!(lines, vec!["line1\n", "line2\n"]);
    }

    #[test]
    fn install_results_appends_block_when_absent() {
        let mut ex = AutoExport::new(ctx(), None, None);
        ex.ctx.template = vec!["body\n".to_string()];
        ex.install_results();
        let body = ex.ctx.template.concat();
        assert!(body.contains("Install tests:\n"));
        assert!(body.contains("Installation tests done in openQA with following results: UNKNOWN"));
    }

    #[tokio::test]
    async fn get_logs_empty_without_auto() {
        let ex = AutoExport::new(ctx(), None, None);
        let out = ex
            .get_logs(
                &OkFetcher(b"x".to_vec()),
                &super::super::base::DenyOverwrite,
            )
            .await;
        assert!(out.is_empty());
    }

    /// Builds a `ctx` whose `install_logs` writes land in `dir` (isolated).
    fn ctx_in(dir: &std::path::Path, template: &[&str]) -> ExportContext {
        let mut cfg = Config::default();
        cfg.template_dir = dir.to_path_buf();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        let lines: Vec<String> = template.iter().map(|s| (*s).to_string()).collect();
        ExportContext::new(cfg, &lines, false, rrid)
    }

    #[tokio::test]
    async fn get_logs_downloads_and_writes_seeded_results() {
        let dir = tempfile::tempdir().unwrap();
        let auto = seeded_auto(Some(vec![urls("passed")]), vec![]);
        let ex = AutoExport::new(ctx_in(dir.path(), &[]), Some(auto), None);

        let out = ex
            .get_logs(
                &OkFetcher(b"zypper install log\n".to_vec()),
                &super::super::base::DenyOverwrite,
            )
            .await;

        // Filename is `{distri.lower()}_{version}_{arch}.log`.
        assert_eq!(out, vec!["sles_15-SP5_x86_64.log".to_string()]);
        let written = std::fs::read_to_string(ex.ctx.install_logs_dir().join(&out[0])).unwrap();
        assert_eq!(written, "zypper install log\n");
    }

    #[tokio::test]
    async fn run_with_seeded_auto_injects_status_pp_and_writes_log() {
        let dir = tempfile::tempdir().unwrap();
        let auto = seeded_auto(
            Some(vec![urls("passed")]),
            vec!["Results from openQA jobs\n".to_string()],
        );
        // A realistic template: the `source code change review:` anchor is not
        // the first line (upstream templates always have a header above it), so
        // the `inject_openqa` insertion point (anchor - 1) is in range.
        let ctx = ctx_in(
            dir.path(),
            &[
                "Test results by product-arch:\n",
                "\n",
                "source code change review:\n",
            ],
        );
        let mut ex = AutoExport::new(ctx, Some(auto), None);

        let out = ex
            .run(
                &OkFetcher(b"install log body\n".to_vec()),
                &super::super::base::DenyOverwrite,
            )
            .await;

        let body = out.concat();
        // Status computed from the seeded (passing) result, not UNKNOWN.
        assert!(
            body.contains("Installation tests done in openQA with following results: PASSED"),
            "status line missing: {body}"
        );
        // The connector's pretty-print block was injected.
        assert!(
            body.contains("Results from openQA jobs"),
            "pp missing: {body}"
        );
        // The per-job install log was downloaded and written to disk.
        let logfile = ex.ctx.install_logs_dir().join("sles_15-SP5_x86_64.log");
        assert!(logfile.exists(), "install log not written: {logfile:?}");
    }

    #[tokio::test]
    async fn run_without_auto_runs_pipeline_and_returns_template() {
        // With no auto connector, run still executes install_results + sysinfo
        // + dedup and returns the mutated template.
        let mut cfg = Config::default();
        // Point template_dir at a temp dir so any install-logs write is isolated.
        let dir = tempfile::tempdir().unwrap();
        cfg.template_dir = dir.path().to_path_buf();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        let ctx = ExportContext::new(cfg, &["body\n".to_string()], false, rrid);
        let mut ex = AutoExport::new(ctx, None, None);

        let out = ex
            .run(
                &OkFetcher(b"x".to_vec()),
                &super::super::base::DenyOverwrite,
            )
            .await;

        assert!(out.iter().any(|l| l.contains("Install tests:")));
        assert!(out.last().unwrap().starts_with("## export MTUI:"));
    }
}
