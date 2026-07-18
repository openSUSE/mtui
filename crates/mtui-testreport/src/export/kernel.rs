//! Exporter for kernel jobs.
//!
//! Ports `mtui.update_workflow.export.kernel.KernelExport`. It inserts the
//! kernel openQA result matrices under the `regression tests:` section,
//! downloads the per-job logs via the shared [`download_logs`], and runs the
//! base sequence.

use mtui_datasources::OpenQAOverviewResult;
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_types::OpenQAResult;

use super::base::ExportContext;
use super::downloader::{BytesFetcher, ErrorMode, download_logs};

/// The kernel-jobs exporter.
pub struct KernelExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The kernel openQA connector results (regular + baremetal instances).
    pub kernel: Vec<KernelOpenQA>,
    /// The openqa_overview payload, if the overview command ran.
    pub overview: Option<OpenQAOverviewResult>,
}

impl KernelExport {
    /// Builds a kernel exporter over `ctx`.
    #[must_use]
    pub fn new(
        ctx: ExportContext,
        kernel: Vec<KernelOpenQA>,
        overview: Option<OpenQAOverviewResult>,
    ) -> Self {
        Self {
            ctx,
            kernel,
            overview,
        }
    }

    /// Downloads the kernel logs and returns the `*.log` filenames now present
    /// in the install-logs directory (upstream `get_logs`).
    pub async fn get_logs(&self, fetcher: &dyn BytesFetcher) -> Vec<String> {
        let in_path = self.ctx.install_logs_dir();
        let res_path = self
            .ctx
            .config
            .template_dir
            .join(self.ctx.rrid.to_string())
            .join("results");
        if let Err(e) = std::fs::create_dir_all(&res_path) {
            tracing::error!("Failed to create {}: {e}", res_path.display());
        }

        // Build the (host, tests) matrix from each populated connector.
        let connectors: Vec<(String, Vec<mtui_types::Test>)> = self
            .kernel
            .iter()
            .filter(|k| k.has_results())
            .map(|k| {
                (
                    k.host().to_string(),
                    k.results().map(<[_]>::to_vec).unwrap_or_default(),
                )
            })
            .collect();

        // TODO: configurable errormode (upstream hard-codes "tolerant").
        let _ = download_logs(
            fetcher,
            &connectors,
            &res_path,
            &in_path,
            ErrorMode::Tolerant,
        )
        .await;

        // Return the *.log filenames now in the install-logs directory. Scan off
        // the async worker so a slow filesystem does not block a Tokio thread.
        let mut filenames = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&in_path).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("log")
                    && let Some(name) = path.file_name().and_then(|n| n.to_str())
                {
                    filenames.push(name.to_string());
                }
            }
        }
        filenames.sort();
        filenames
    }

    /// Inserts the kernel result matrices under `regression tests:`
    /// (upstream `kernel_results`).
    ///
    /// The insertion point is the `(put your details here)` placeholder (removed
    /// if present); otherwise the block replaces any existing content between the
    /// kernel-default link (or the `regression tests:` header) and
    /// `build log review:`.
    pub fn kernel_results(&mut self, now: &str) {
        let template = &mut self.ctx.template;
        let Some(regression) = template.iter().position(|l| l == "regression tests:\n") else {
            return;
        };

        let mut line = if let Some(placeholder) = template
            .iter()
            .skip(regression)
            .position(|l| l == "(put your details here)\n")
            .map(|i| i + regression)
        {
            template.remove(placeholder);
            placeholder
        } else {
            let start = template
                .iter()
                .position(|l| l == "    * https://pes.suse.de/QA_Maintenance/kernel-default/\n")
                .map_or(regression + 1, |i| i + 1);
            if let Some(e_line) = template.iter().position(|l| l == "build log review:\n") {
                template.drain(start..e_line);
            }
            start
        };

        template.insert(line, format!("Results added on {now}\n"));
        template.insert(line + 1, "\n".to_string());
        template.insert(line + 2, "Results from openQA:\n".to_string());
        template.insert(line + 3, "\n".to_string());
        line += 4;

        for results in &self.kernel {
            if results.has_results() {
                for r in results.pp() {
                    template.insert(line, r.clone());
                    line += 1;
                }
                line += 1;
            }
        }

        if let Some(build_review) = template.iter().position(|l| l == "build log review:\n") {
            template.insert(build_review, "\n".to_string());
        }
    }

    /// Runs the exporter (upstream `run`).
    pub async fn run(&mut self, fetcher: &dyn BytesFetcher) -> Vec<String> {
        self.ctx.install_results();
        // Kernel exports have no "auto" connector, so inject_openqa is a no-op
        // (upstream guards on self.openqa.auto being falsy).
        self.ctx.inject_openqa(&[]);
        if let Some(overview) = self.overview.clone() {
            self.ctx.inject_overview(&overview);
        }
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S%.6f")
            .to_string();
        self.kernel_results(&now);
        let filenames = self.get_logs(fetcher).await;
        self.ctx.installlogs_lines(&filenames);
        self.ctx.add_sysinfo();
        self.ctx.dedup_lines();
        self.ctx.template.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::options::Config;

    fn ctx(template: &[&str]) -> ExportContext {
        let cfg = Config::default();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        let lines: Vec<String> = template.iter().map(|s| (*s).to_string()).collect();
        ExportContext::new(cfg, &lines, false, rrid)
    }

    #[test]
    fn kernel_results_replaces_placeholder_and_inserts_headers() {
        let mut ex = KernelExport::new(
            ctx(&[
                "regression tests:\n",
                "\n",
                "(put your details here)\n",
                "\n",
                "build log review:\n",
            ]),
            Vec::new(),
            None,
        );
        ex.kernel_results("2026-01-01 00:00:00");
        let body = ex.ctx.template.concat();
        assert!(!body.contains("(put your details here)"));
        assert!(body.contains("Results added on 2026-01-01 00:00:00\n"));
        assert!(body.contains("Results from openQA:\n"));
    }

    #[test]
    fn kernel_results_noop_without_regression_header() {
        let mut ex = KernelExport::new(ctx(&["nothing\n"]), Vec::new(), None);
        let before = ex.ctx.template.clone();
        ex.kernel_results("t");
        assert_eq!(ex.ctx.template, before);
    }

    struct OkFetcher;

    #[async_trait::async_trait]
    impl BytesFetcher for OkFetcher {
        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
            Ok(b"log".to_vec())
        }
    }

    fn temp_ctx(template: &[&str]) -> ExportContext {
        let mut cfg = Config::default();
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the path stays valid for the test's lifetime.
        cfg.template_dir = dir.keep();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        let lines: Vec<String> = template.iter().map(|s| (*s).to_string()).collect();
        ExportContext::new(cfg, &lines, false, rrid)
    }

    #[tokio::test]
    async fn get_logs_creates_dirs_and_lists_logs() {
        let ex = KernelExport::new(temp_ctx(&[]), Vec::new(), None);
        let in_path = ex.ctx.install_logs_dir();
        std::fs::create_dir_all(&in_path).unwrap();
        std::fs::write(in_path.join("h-zypper-x86_64.log"), b"x").unwrap();
        std::fs::write(in_path.join("ignore.txt"), b"x").unwrap();

        let out = ex.get_logs(&OkFetcher).await;
        assert_eq!(out, vec!["h-zypper-x86_64.log".to_string()]);
    }

    #[tokio::test]
    async fn run_returns_template_with_footer() {
        let mut ex = KernelExport::new(
            temp_ctx(&[
                "regression tests:\n",
                "\n",
                "(put your details here)\n",
                "\n",
                "build log review:\n",
            ]),
            Vec::new(),
            None,
        );
        let out = ex.run(&OkFetcher).await;
        assert!(out.iter().any(|l| l.contains("Results from openQA:")));
        assert!(out.last().unwrap().starts_with("## export MTUI:"));
    }
}
