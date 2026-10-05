//! Exporter for the automatic workflow.
//!
//! Downloads each passing openQA install job's install log.
//!
//! The per-log HTTP download goes through the [`BytesFetcher`] seam (an
//! [`HttpClient`](mtui_datasources::http::HttpClient) in production, a mock in
//! tests), mirroring how the downloader is tested.

use mtui_datasources::qem_dashboard::DashboardAutoOpenQA;
use mtui_types::URLs;

use super::base::{ExportContext, OverwritePrompt};
use super::downloader::BytesFetcher;

/// The automatic-workflow exporter.
pub struct AutoExport {
    /// Shared export state and helpers.
    pub ctx: ExportContext,
    /// The QEM-dashboard "auto" openQA connector, `None` when unpopulated.
    auto: Option<DashboardAutoOpenQA>,
}

impl AutoExport {
    /// Builds an auto exporter over `ctx`.
    #[must_use]
    pub fn new(ctx: ExportContext, auto: Option<DashboardAutoOpenQA>) -> Self {
        Self { ctx, auto }
    }

    /// Downloads each passing job's install log, returning the written
    /// `<distri>_<version>_<arch>.log` filenames.
    async fn get_logs(
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

        // Download concurrently (order-preserving), then write serially: the
        // write may prompt on overwrite, so it must not run in parallel.
        let downloads = results.iter().map(|url| self.installog_lines(fetcher, url));
        let all_lines = futures::future::join_all(downloads).await;

        let mut filenames = Vec::new();
        for (url, lines) in results.iter().zip(all_lines) {
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

    /// Downloads one install log and returns its lines (with trailing
    /// newlines), or an empty vec on failure.
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

    /// Downloads the install logs, returning the written filenames.
    pub async fn write_logs(
        &self,
        fetcher: &dyn BytesFetcher,
        prompt: &dyn OverwritePrompt,
    ) -> Vec<String> {
        self.get_logs(fetcher, prompt).await
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
        ExportContext::new(cfg, false, rrid)
    }

    /// Builds a `DashboardAutoOpenQA` with seeded `results`/`pp`, without any
    /// network I/O: `new()` only stores config, then we set the public output
    /// fields directly (as the connector's `run()` would).
    fn seeded_auto(results: Option<Vec<URLs>>, pp: Vec<String>) -> DashboardAutoOpenQA {
        use mtui_datasources::{QemDashboardClient, QemIncident, VerifyPolicy};
        let rrid: mtui_types::RequestReviewID = "SUSE:Maintenance:1:2".parse().unwrap();
        let client =
            QemDashboardClient::new("http://dashboard.invalid/api", VerifyPolicy::Default(false))
                .expect("client builds");
        let incident = QemIncident {
            rrid: rrid.clone(),
            incident_number: "1".to_string(),
            source: mtui_types::UpdateSource::Obs,
            client,
            data: None,
        };
        let mut auto = DashboardAutoOpenQA::new("http://oqa.invalid", &incident, rrid, 1);
        auto.results = results;
        auto.pp = pp;
        auto
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
        let ex = AutoExport::new(ctx(), None);
        let fetcher = OkFetcher(b"line1\nline2\n".to_vec());
        let lines = ex.installog_lines(&fetcher, &urls("passed")).await;
        assert_eq!(lines, vec!["line1\n", "line2\n"]);
    }

    #[tokio::test]
    async fn get_logs_empty_without_auto() {
        let ex = AutoExport::new(ctx(), None);
        let out = ex
            .get_logs(
                &OkFetcher(b"x".to_vec()),
                &super::super::base::DenyOverwrite,
            )
            .await;
        assert!(out.is_empty());
    }

    /// Builds a `ctx` whose `install_logs` writes land in `dir` (isolated).
    fn ctx_in(dir: &std::path::Path) -> ExportContext {
        let mut cfg = Config::default();
        cfg.template_dir = dir.to_path_buf();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        ExportContext::new(cfg, false, rrid)
    }

    #[tokio::test]
    async fn get_logs_downloads_and_writes_seeded_results() {
        let dir = tempfile::tempdir().unwrap();
        let auto = seeded_auto(Some(vec![urls("passed")]), vec![]);
        let ex = AutoExport::new(ctx_in(dir.path()), Some(auto));

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

    /// A fetcher returning a distinct body per URL, or an error for URLs in
    /// its `fail` set — lets tests assert content lands in the right file
    /// regardless of download completion order.
    struct KeyedFetcher {
        bodies: std::collections::HashMap<String, Vec<u8>>,
        fail: std::collections::HashSet<String>,
    }

    #[async_trait]
    impl BytesFetcher for KeyedFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            if self.fail.contains(url) {
                return Err(format!("boom: {url}"));
            }
            self.bodies
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no body for {url}"))
        }
    }

    fn url_at(distri: &str, arch: &str, version: &str, path: &str) -> URLs {
        URLs::new(distri, arch, version, path, "passed")
    }

    #[tokio::test]
    async fn get_logs_preserves_order_under_concurrent_fanout() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![
            url_at("SLES", "x86_64", "15-SP5", "https://oqa/a/file/log.txt"),
            url_at("SLED", "aarch64", "15-SP6", "https://oqa/b/file/log.txt"),
            url_at("SLES", "s390x", "15-SP4", "https://oqa/c/file/log.txt"),
        ];
        let auto = seeded_auto(Some(results.clone()), vec![]);
        let ex = AutoExport::new(ctx_in(dir.path()), Some(auto));

        let bodies = [
            ("https://oqa/a/file/log.txt", b"body-a\n".to_vec()),
            ("https://oqa/b/file/log.txt", b"body-b\n".to_vec()),
            ("https://oqa/c/file/log.txt", b"body-c\n".to_vec()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let fetcher = KeyedFetcher {
            bodies,
            fail: std::collections::HashSet::new(),
        };

        let out = ex
            .get_logs(&fetcher, &super::super::base::DenyOverwrite)
            .await;

        // Filenames follow input order.
        assert_eq!(
            out,
            vec![
                "sles_15-SP5_x86_64.log".to_string(),
                "sled_15-SP6_aarch64.log".to_string(),
                "sles_15-SP4_s390x.log".to_string(),
            ]
        );
        // Each file holds the body for its own URL (content correctly paired).
        let dir = ex.ctx.install_logs_dir();
        for (name, expect) in [
            ("sles_15-SP5_x86_64.log", "body-a\n"),
            ("sled_15-SP6_aarch64.log", "body-b\n"),
            ("sles_15-SP4_s390x.log", "body-c\n"),
        ] {
            assert_eq!(std::fs::read_to_string(dir.join(name)).unwrap(), expect);
        }
    }

    #[tokio::test]
    async fn get_logs_skips_failed_download_and_pairs_rest() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![
            url_at("SLES", "x86_64", "15-SP5", "https://oqa/a/file/log.txt"),
            url_at("SLED", "aarch64", "15-SP6", "https://oqa/b/file/log.txt"),
            url_at("SLES", "s390x", "15-SP4", "https://oqa/c/file/log.txt"),
        ];
        let auto = seeded_auto(Some(results.clone()), vec![]);
        let ex = AutoExport::new(ctx_in(dir.path()), Some(auto));

        let bodies = [
            ("https://oqa/a/file/log.txt", b"body-a\n".to_vec()),
            ("https://oqa/c/file/log.txt", b"body-c\n".to_vec()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let fetcher = KeyedFetcher {
            bodies,
            fail: std::iter::once("https://oqa/b/file/log.txt".to_string()).collect(),
        };

        let out = ex
            .get_logs(&fetcher, &super::super::base::DenyOverwrite)
            .await;

        // The failed middle download is skipped; the surrounding logs still pair.
        assert_eq!(
            out,
            vec![
                "sles_15-SP5_x86_64.log".to_string(),
                "sles_15-SP4_s390x.log".to_string(),
            ]
        );
        let dir = ex.ctx.install_logs_dir();
        assert_eq!(
            std::fs::read_to_string(dir.join("sles_15-SP5_x86_64.log")).unwrap(),
            "body-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sles_15-SP4_s390x.log")).unwrap(),
            "body-c\n"
        );
        assert!(!dir.join("sled_15-SP6_aarch64.log").exists());
    }
}
