//! Exporter for kernel jobs.
//!
//! Downloads the per-job kernel openQA logs via the shared [`download_logs`].

use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_types::OpenQAResult;

use super::base::ExportContext;
use super::downloader::{BytesFetcher, ErrorMode, download_logs};

/// The kernel-jobs exporter.
pub struct KernelExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The kernel openQA connector results (regular + baremetal instances).
    kernel: Vec<KernelOpenQA>,
}

impl KernelExport {
    /// Builds a kernel exporter over `ctx`.
    #[must_use]
    pub fn new(ctx: ExportContext, kernel: Vec<KernelOpenQA>) -> Self {
        Self { ctx, kernel }
    }

    /// Downloads the kernel logs and returns the `*.log` filenames now present
    /// in the install-logs directory.
    async fn get_logs(&self, fetcher: &dyn BytesFetcher) -> Vec<String> {
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

        // TODO: configurable errormode (currently hard-coded to "tolerant").
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

    /// Downloads the kernel logs (and writes `results/`), returning the `*.log`
    /// filenames present.
    pub async fn write_logs(&self, fetcher: &dyn BytesFetcher) -> Vec<String> {
        self.get_logs(fetcher).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::options::Config;

    struct OkFetcher;

    #[async_trait::async_trait]
    impl BytesFetcher for OkFetcher {
        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
            Ok(b"log".to_vec())
        }
    }

    fn temp_ctx() -> ExportContext {
        let mut cfg = Config::default();
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the path stays valid for the test's lifetime.
        cfg.template_dir = dir.keep();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        ExportContext::new(cfg, false, rrid)
    }

    #[tokio::test]
    async fn get_logs_creates_dirs_and_lists_logs() {
        let ex = KernelExport::new(temp_ctx(), Vec::new());
        let in_path = ex.ctx.install_logs_dir();
        std::fs::create_dir_all(&in_path).unwrap();
        std::fs::write(in_path.join("h-zypper-x86_64.log"), b"x").unwrap();
        std::fs::write(in_path.join("ignore.txt"), b"x").unwrap();

        let out = ex.get_logs(&OkFetcher).await;
        assert_eq!(out, vec!["h-zypper-x86_64.log".to_string()]);
    }
}
