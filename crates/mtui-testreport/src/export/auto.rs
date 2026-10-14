//! Exporter for the automatic workflow.
//!
//! Downloads each passing openQA install job's install log.
//!
//! The per-log HTTP download goes through the [`BytesFetcher`] seam (an
//! [`HttpClient`](mtui_datasources::http::HttpClient) in production, a mock in
//! tests), mirroring how the downloader is tested.

use futures::stream::{self, StreamExt};
use mtui_datasources::qem_dashboard::DashboardAutoOpenQA;
use mtui_types::URLs;

use super::base::ExportContext;
use super::downloader::{BytesFetcher, DOWNLOAD_CONCURRENCY, is_safe_component};
use crate::support::fileops::atomic_write_file;

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
    /// `<distri>_<version>_<arch>[_<flavor>].log` filenames.
    ///
    /// The logs are server-derived, so an existing file is rewritten in place.
    async fn get_logs(&self, fetcher: &dyn BytesFetcher) -> Vec<String> {
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

        // The name is dashboard-derived: refuse it before any fetch or write.
        let planned: Vec<(&URLs, String)> = results
            .iter()
            .filter_map(|url| {
                let name = log_file_name(url);
                if is_safe_component(&name) {
                    Some((url, name))
                } else {
                    tracing::error!("Refusing unsafe install log filename {name:?}");
                    None
                }
            })
            .collect();

        // `buffered` yields in input order, so writes stay serial and the last
        // duplicate name wins; only the in-flight window of bodies is retained.
        // Boxed so a caller proving this future `Send` is not defeated by
        // higher-ranked lifetime inference over the combinator chain.
        let mut downloads = stream::iter(planned)
            .map(|(url, name)| async move { (name, self.installog_text(fetcher, url).await) })
            .buffered(DOWNLOAD_CONCURRENCY)
            .boxed();

        let mut filenames = Vec::new();
        while let Some((fn_name, text)) = downloads.next().await {
            if text.is_empty() {
                continue;
            }
            let path = dir.join(&fn_name);
            if let Err(e) = atomic_write_file(text.as_bytes(), &path) {
                tracing::error!("Failed to write {}: {e}", path.display());
            }
            filenames.push(fn_name);
        }
        filenames
    }

    /// Downloads one install log and returns its text, or an empty string on
    /// failure.
    async fn installog_text(&self, fetcher: &dyn BytesFetcher, url: &URLs) -> String {
        match fetcher.get_bytes(&url.url).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => {
                tracing::error!("log {} failed to download", url.url);
                String::new()
            }
        }
    }

    /// Downloads the install logs, returning the written filenames.
    pub async fn write_logs(&self, fetcher: &dyn BytesFetcher) -> Vec<String> {
        self.get_logs(fetcher).await
    }
}

/// `<distri>_<version>_<arch>[_<flavor>].log`; the flavor tells apart the
/// install jobs (plain, SAP, HA, ...) that share a distri/version/arch.
fn log_file_name(url: &URLs) -> String {
    let distri = url.distri.to_lowercase();
    if url.flavor.is_empty() {
        format!("{distri}_{}_{}.log", url.version, url.arch)
    } else {
        format!("{distri}_{}_{}_{}.log", url.version, url.arch, url.flavor)
    }
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
            "",
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

    struct OkFetcher(Vec<u8>);

    #[async_trait]
    impl BytesFetcher for OkFetcher {
        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn installog_text_returns_downloaded_text() {
        let ex = AutoExport::new(ctx(), None);
        let fetcher = OkFetcher(b"line1\nline2\n".to_vec());
        let text = ex.installog_text(&fetcher, &urls("passed")).await;
        assert_eq!(text, "line1\nline2\n");
    }

    #[tokio::test]
    async fn get_logs_empty_without_auto() {
        let ex = AutoExport::new(ctx(), None);
        let out = ex.get_logs(&OkFetcher(b"x".to_vec())).await;
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
            .get_logs(&OkFetcher(b"zypper install log\n".to_vec()))
            .await;

        // Without a flavor the name is `{distri.lower()}_{version}_{arch}.log`.
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
        URLs::new(distri, arch, version, "", path, "passed")
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

        let out = ex.get_logs(&fetcher).await;

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

    fn url_flavored(flavor: &str, path: &str) -> URLs {
        URLs::new("SLES", "x86_64", "15-SP5", flavor, path, "passed")
    }

    fn fetcher_for(bodies: &[(&str, &str)]) -> KeyedFetcher {
        KeyedFetcher {
            bodies: bodies
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.as_bytes().to_vec()))
                .collect(),
            fail: std::collections::HashSet::new(),
        }
    }

    fn dir_listing(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn get_logs_names_each_flavor_separately() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![
            url_flavored("Server-DVD-Incidents-Install", "https://oqa/a/file/log.txt"),
            url_flavored(
                "Server-DVD-HA-Incidents-Install",
                "https://oqa/b/file/log.txt",
            ),
            url_flavored("", "https://oqa/c/file/log.txt"),
        ];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));
        let fetcher = fetcher_for(&[
            ("https://oqa/a/file/log.txt", "body-a\n"),
            ("https://oqa/b/file/log.txt", "body-b\n"),
            ("https://oqa/c/file/log.txt", "body-c\n"),
        ]);

        let out = ex.get_logs(&fetcher).await;

        assert_eq!(
            out,
            vec![
                "sles_15-SP5_x86_64_Server-DVD-Incidents-Install.log".to_string(),
                "sles_15-SP5_x86_64_Server-DVD-HA-Incidents-Install.log".to_string(),
                "sles_15-SP5_x86_64.log".to_string(),
            ]
        );
        let logs = ex.ctx.install_logs_dir();
        assert_eq!(dir_listing(&logs).len(), 3);
        assert_eq!(
            std::fs::read_to_string(logs.join(&out[1])).unwrap(),
            "body-b\n"
        );
    }

    #[tokio::test]
    async fn get_logs_rewrites_in_place_on_repeat_export() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![url_flavored(
            "Server-DVD-Incidents-Install",
            "https://oqa/a/file/log.txt",
        )];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));

        let first = ex
            .get_logs(&fetcher_for(&[("https://oqa/a/file/log.txt", "old\n")]))
            .await;
        let second = ex
            .get_logs(&fetcher_for(&[("https://oqa/a/file/log.txt", "new\n")]))
            .await;

        assert_eq!(first, second);
        let logs = ex.ctx.install_logs_dir();
        assert_eq!(dir_listing(&logs), first);
        assert_eq!(
            std::fs::read_to_string(logs.join(&first[0])).unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn get_logs_keeps_the_downloaded_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let auto = seeded_auto(Some(vec![urls("passed")]), vec![]);
        let ex = AutoExport::new(ctx_in(dir.path()), Some(auto));

        let out = ex.get_logs(&OkFetcher(b"a\n\nb\n".to_vec())).await;

        let written = std::fs::read_to_string(ex.ctx.install_logs_dir().join(&out[0])).unwrap();
        assert_eq!(written, "a\n\nb\n");
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

        let out = ex.get_logs(&fetcher).await;

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

    /// Records every requested URL; serves `bodies`, and answers an error for
    /// anything else so an unfixed unsafe entry cannot write outside the test.
    struct SpyFetcher {
        seen: std::sync::Mutex<Vec<String>>,
        bodies: std::collections::HashMap<String, Vec<u8>>,
    }

    impl SpyFetcher {
        fn new(bodies: &[(&str, &str)]) -> Self {
            Self {
                seen: std::sync::Mutex::default(),
                bodies: bodies
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.as_bytes().to_vec()))
                    .collect(),
            }
        }

        fn seen(&self) -> Vec<String> {
            let mut urls = self.seen.lock().unwrap().clone();
            urls.sort();
            urls
        }
    }

    #[async_trait]
    impl BytesFetcher for SpyFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            self.seen.lock().unwrap().push(url.to_string());
            self.bodies
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no body for {url}"))
        }
    }

    #[tokio::test]
    async fn get_logs_refuses_unsafe_names_before_fetching() {
        let hostile = [
            ("/outside/owned", "x86_64", "15-SP5", ""),
            ("SLES", "x86_64", "15/SP5", ""),
            ("SLES", "x86_64", "15\\SP5", ""),
            ("SLES", "x86/64", "15-SP5", ""),
            ("SLES", "x86\\64", "15-SP5", ""),
            ("SLES", "x86_64", "15-SP5", "a/b"),
            ("SLES", "x86_64", "15-SP5", "a\\b"),
            ("SL\nES", "x86_64", "15-SP5", ""),
            ("SLES", "x86_64", "15\u{7}SP5", ""),
            ("SLES", "x86\u{0}64", "15-SP5", ""),
            ("SLES", "x86_64", "15-SP5", "a\rb"),
        ];
        for (distri, arch, version, flavor) in hostile {
            let dir = tempfile::tempdir().unwrap();
            let results = vec![URLs::new(
                distri,
                arch,
                version,
                flavor,
                "https://oqa/evil/file/log.txt",
                "passed",
            )];
            let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));
            let spy = SpyFetcher::new(&[]);

            let out = ex.get_logs(&spy).await;

            assert!(out.is_empty(), "{distri:?}/{arch:?}/{version:?}/{flavor:?}");
            assert!(
                spy.seen().is_empty(),
                "fetched for {distri:?}/{arch:?}/{version:?}/{flavor:?}"
            );
            assert!(dir_listing(&ex.ctx.install_logs_dir()).is_empty());
        }
    }

    #[tokio::test]
    async fn get_logs_never_writes_outside_the_install_logs_dir() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![url_at(
            "../sentinel",
            "x86_64",
            "15-SP5",
            "https://oqa/evil/file/log.txt",
        )];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));
        let logs = ex.ctx.install_logs_dir();
        std::fs::create_dir_all(&logs).unwrap();
        let sentinel = logs.parent().unwrap().join("sentinel_15-SP5_x86_64.log");
        std::fs::write(&sentinel, "keep\n").unwrap();
        let spy = SpyFetcher::new(&[("https://oqa/evil/file/log.txt", "owned\n")]);

        let out = ex.get_logs(&spy).await;

        assert!(out.is_empty());
        assert!(spy.seen().is_empty());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep\n");
    }

    #[tokio::test]
    async fn get_logs_skips_only_the_unsafe_entry_in_a_mixed_batch() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![
            url_at("SLES", "x86_64", "15-SP5", "https://oqa/a/file/log.txt"),
            url_at(
                "../escape",
                "x86_64",
                "15-SP5",
                "https://oqa/b/file/log.txt",
            ),
            url_at("SLED", "aarch64", "15-SP6", "https://oqa/c/file/log.txt"),
        ];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));
        let spy = SpyFetcher::new(&[
            ("https://oqa/a/file/log.txt", "body-a\n"),
            ("https://oqa/b/file/log.txt", "body-b\n"),
            ("https://oqa/c/file/log.txt", "body-c\n"),
        ]);

        let out = ex.get_logs(&spy).await;

        assert_eq!(
            out,
            vec![
                "sles_15-SP5_x86_64.log".to_string(),
                "sled_15-SP6_aarch64.log".to_string(),
            ]
        );
        assert_eq!(
            spy.seen(),
            vec![
                "https://oqa/a/file/log.txt".to_string(),
                "https://oqa/c/file/log.txt".to_string(),
            ]
        );
        let logs = ex.ctx.install_logs_dir();
        let mut written = out.clone();
        written.sort();
        assert_eq!(dir_listing(&logs), written);
        assert_eq!(
            std::fs::read_to_string(logs.join(&out[0])).unwrap(),
            "body-a\n"
        );
        assert_eq!(
            std::fs::read_to_string(logs.join(&out[1])).unwrap(),
            "body-c\n"
        );
        assert!(
            !logs
                .parent()
                .unwrap()
                .join("escape_15-SP5_x86_64.log")
                .exists()
        );
    }

    #[tokio::test]
    async fn get_logs_keeps_unicode_names() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![url_flavored("Sérver-ß", "https://oqa/a/file/log.txt")];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));

        let out = ex
            .get_logs(&fetcher_for(&[("https://oqa/a/file/log.txt", "u\n")]))
            .await;

        assert_eq!(out, vec!["sles_15-SP5_x86_64_Sérver-ß.log".to_string()]);
        assert_eq!(
            std::fs::read_to_string(ex.ctx.install_logs_dir().join(&out[0])).unwrap(),
            "u\n"
        );
    }

    /// Completes the second URL before the first, so a duplicate destination
    /// is written in input order only if the consumer preserves it.
    struct ReversedPairFetcher {
        second_done: tokio::sync::Notify,
    }

    #[async_trait]
    impl BytesFetcher for ReversedPairFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            if url.contains("/first/") {
                self.second_done.notified().await;
                Ok(b"body-first\n".to_vec())
            } else {
                self.second_done.notify_one();
                Ok(b"body-second\n".to_vec())
            }
        }
    }

    #[tokio::test]
    async fn get_logs_last_input_wins_for_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![
            url_at("SLES", "x86_64", "15-SP5", "https://oqa/first/file/log.txt"),
            url_at(
                "SLES",
                "x86_64",
                "15-SP5",
                "https://oqa/second/file/log.txt",
            ),
        ];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));
        let fetcher = ReversedPairFetcher {
            second_done: tokio::sync::Notify::new(),
        };

        let out = tokio::time::timeout(std::time::Duration::from_secs(10), ex.get_logs(&fetcher))
            .await
            .expect("fetches must overlap");

        assert_eq!(out, vec!["sles_15-SP5_x86_64.log"; 2]);
        let logs = ex.ctx.install_logs_dir();
        assert_eq!(dir_listing(&logs), vec!["sles_15-SP5_x86_64.log"]);
        assert_eq!(
            std::fs::read_to_string(logs.join(&out[0])).unwrap(),
            "body-second\n"
        );
    }

    #[tokio::test]
    async fn get_logs_converts_invalid_utf8_lossily() {
        let dir = tempfile::tempdir().unwrap();
        let auto = seeded_auto(Some(vec![urls("passed")]), vec![]);
        let ex = AutoExport::new(ctx_in(dir.path()), Some(auto));

        let out = ex.get_logs(&OkFetcher(b"ok\xff\xfe\n".to_vec())).await;

        let written = std::fs::read(ex.ctx.install_logs_dir().join(&out[0])).unwrap();
        assert_eq!(written, "ok\u{FFFD}\u{FFFD}\n".as_bytes());
    }

    #[tokio::test]
    async fn get_logs_omits_successful_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let results = vec![
            url_at("SLES", "x86_64", "15-SP5", "https://oqa/a/file/log.txt"),
            url_at("SLED", "aarch64", "15-SP6", "https://oqa/b/file/log.txt"),
        ];
        let ex = AutoExport::new(ctx_in(dir.path()), Some(seeded_auto(Some(results), vec![])));
        let fetcher = fetcher_for(&[
            ("https://oqa/a/file/log.txt", ""),
            ("https://oqa/b/file/log.txt", "body-b\n"),
        ]);

        let out = ex.get_logs(&fetcher).await;

        assert_eq!(out, vec!["sled_15-SP6_aarch64.log".to_string()]);
        assert_eq!(
            dir_listing(&ex.ctx.install_logs_dir()),
            vec!["sled_15-SP6_aarch64.log"]
        );
    }

    #[tokio::test]
    async fn get_logs_fetches_nothing_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "file\n").unwrap();
        let ex = AutoExport::new(
            ctx_in(&blocker),
            Some(seeded_auto(Some(vec![urls("passed")]), vec![])),
        );
        let spy = SpyFetcher::new(&[("https://oqa/tests/1/file/log.txt", "x\n")]);

        let out = ex.get_logs(&spy).await;

        assert!(out.is_empty());
        assert!(spy.seen().is_empty());
        assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "file\n");
    }

    const BATCH: usize = 10;

    fn batch_ex(dir: &std::path::Path) -> AutoExport {
        let results = (0..BATCH)
            .map(|i| url_flavored(&format!("f{i}"), &format!("https://oqa/{i}/file/log.txt")))
            .collect();
        AutoExport::new(ctx_in(dir), Some(seeded_auto(Some(results), vec![])))
    }

    /// Holds every fetch open until released, counting how many are in flight.
    struct GatedFetcher {
        started: std::sync::atomic::AtomicUsize,
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        gate: tokio::sync::Semaphore,
    }

    impl GatedFetcher {
        fn new() -> Self {
            Self {
                started: 0.into(),
                active: 0.into(),
                peak: 0.into(),
                gate: tokio::sync::Semaphore::new(0),
            }
        }
    }

    #[async_trait]
    impl BytesFetcher for GatedFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            use std::sync::atomic::Ordering::SeqCst;
            self.started.fetch_add(1, SeqCst);
            let now = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            self.gate.acquire().await.unwrap().forget();
            self.active.fetch_sub(1, SeqCst);
            Ok(format!("body-{url}\n").into_bytes())
        }
    }

    #[tokio::test]
    async fn auto_pipeline_bounds_in_flight_downloads() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = tempfile::tempdir().unwrap();
        let ex = batch_ex(dir.path());
        let fetcher = GatedFetcher::new();

        let control = async {
            while fetcher.started.load(SeqCst) < 8 {
                tokio::task::yield_now().await;
            }
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            let started = fetcher.started.load(SeqCst);
            fetcher.gate.add_permits(BATCH);
            started
        };
        let (out, started) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(ex.get_logs(&fetcher), control)
        })
        .await
        .expect("fixture stalled");

        assert_eq!(started, 8, "downloads started before any was released");
        assert_eq!(fetcher.peak.load(SeqCst), 8);
        assert_eq!(out.len(), BATCH);
    }

    /// Signals when the ninth request starts and holds it until released.
    struct NinthFetcher {
        ninth_started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl BytesFetcher for NinthFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            if url.contains("/8/") {
                self.ninth_started.notify_one();
                self.release.notified().await;
            }
            Ok(format!("body-{url}\n").into_bytes())
        }
    }

    #[tokio::test]
    async fn auto_pipeline_publishes_the_first_log_before_later_downloads_start() {
        let dir = tempfile::tempdir().unwrap();
        let ex = batch_ex(dir.path());
        let first = ex.ctx.install_logs_dir().join("sles_15-SP5_x86_64_f0.log");
        let fetcher = NinthFetcher {
            ninth_started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        };

        let control = async {
            fetcher.ninth_started.notified().await;
            let published = first.exists();
            fetcher.release.notify_one();
            published
        };
        let (out, published) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(ex.get_logs(&fetcher), control)
        })
        .await
        .expect("fixture stalled");

        assert!(
            published,
            "first log was not on disk when the ninth started"
        );
        assert_eq!(out.len(), BATCH);
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "body-https://oqa/0/file/log.txt\n"
        );
    }

    #[tokio::test]
    async fn get_logs_reports_the_name_when_the_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let ex = AutoExport::new(
            ctx_in(dir.path()),
            Some(seeded_auto(Some(vec![urls("passed")]), vec![])),
        );
        let obstacle = ex.ctx.install_logs_dir().join("sles_15-SP5_x86_64.log");
        std::fs::create_dir_all(&obstacle).unwrap();

        let out = ex.get_logs(&OkFetcher(b"x\n".to_vec())).await;

        assert_eq!(out, vec!["sles_15-SP5_x86_64.log".to_string()]);
        assert!(obstacle.is_dir());
    }
}
