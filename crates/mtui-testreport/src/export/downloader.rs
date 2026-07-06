//! Download openQA logs for the auto/kernel exporters.
//!
//! Ports `mtui.update_workflow.export.downloader`. Each openQA test maps to a
//! downloader by the first `_`-segment of its name (`install` → the zypper
//! install log, `ltp` → the result-array JSON), with an empty-log fallback for
//! everything else. Upstream fanned the downloads out over a thread pool; this
//! port is async (tokio) with **bounded concurrency**, matching the crate's
//! async-native mandate.
//!
//! ## Error modes
//!
//! * `tolerant` — a failed download is logged and skipped.
//! * `full` — a failed download yields a [`ResultsMissingError`]; after the
//!   whole batch finishes, the first such error is returned.
//!
//! ## Fetch seam
//!
//! HTTP is abstracted behind the [`BytesFetcher`] trait so the exporters inject
//! an [`HttpClient`]-backed fetcher while tests inject a mock. This mirrors the
//! way upstream patched `get_bytes` in `test_export_downloader.py`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use mtui_types::Test;

use crate::support::fileops::atomic_write_file;

/// Max concurrent downloads (bounded fan-out replacing the upstream pool).
const DOWNLOAD_CONCURRENCY: usize = 8;

/// The download error mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorMode {
    /// Log and skip a failed download.
    Tolerant,
    /// Surface a failed download as [`ResultsMissingError`].
    Full,
}

/// A missing openQA result log under [`ErrorMode::Full`].
///
/// Ports upstream `ResultsMissingError`; its `Display` matches the upstream
/// message verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Test: {test} on arch: {arch} missing results.json file. Please restart it.")]
pub struct ResultsMissingError {
    /// The test name.
    pub test: String,
    /// The architecture.
    pub arch: String,
}

/// Fetches the bytes at a URL. The fetch seam for [`download_logs`].
#[async_trait]
pub trait BytesFetcher: Sync {
    /// Fetches `url`, returning its bytes or an error string.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String>;
}

#[async_trait]
impl BytesFetcher for mtui_datasources::http::HttpClient {
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        mtui_datasources::http::HttpClient::get_bytes(self, url)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Which log a test maps to, by the first `_`-segment of its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    /// `install*` → the zypper install log.
    Install,
    /// `ltp*` → the result-array JSON.
    Ltp,
    /// Anything else → no log to download.
    Empty,
}

impl LogKind {
    /// Dispatches on `name.split('_')[0]` (upstream `downloader.get(...)`).
    fn for_name(name: &str) -> Self {
        match name.split('_').next().unwrap_or("") {
            "install" => Self::Install,
            "ltp" => Self::Ltp,
            _ => Self::Empty,
        }
    }
}

/// Joins path segments like `os.path.join` for URL-ish paths (no re-encoding).
fn join_url(base: &str, parts: &[&str]) -> String {
    let mut s = base.trim_end_matches('/').to_string();
    for p in parts {
        s.push('/');
        s.push_str(p);
    }
    s
}

/// The last `/`-segment of `host` (upstream `host.split('/')[-1]`).
fn host_tail(host: &str) -> &str {
    host.rsplit('/').next().unwrap_or(host)
}

/// Builds the `(remote_url, local_path)` for a test, or `None` for an empty log.
fn plan(
    host: &str,
    test: &Test,
    resultsdir: &Path,
    installlogsdir: &Path,
) -> Option<(String, PathBuf)> {
    match LogKind::for_name(&test.name) {
        LogKind::Ltp => {
            let remote = join_url(
                host,
                &[
                    "tests",
                    &test.test_id.to_string(),
                    "file",
                    "result_array.json",
                ],
            );
            let local = resultsdir.join(format!(
                "{}-{}-{}.json",
                host_tail(host),
                test.arch,
                test.name
            ));
            Some((remote, local))
        }
        LogKind::Install => {
            let remote = join_url(
                host,
                &[
                    "tests",
                    &test.test_id.to_string(),
                    "file",
                    "update_kernel-zypper.log",
                ],
            );
            let local =
                installlogsdir.join(format!("{}-zypper-{}.log", host_tail(host), test.arch));
            Some((remote, local))
        }
        LogKind::Empty => {
            tracing::debug!("No log to download for test: {} on {host}", test.name);
            None
        }
    }
}

/// Downloads one log (upstream `_subdl`): fetch + atomic write.
///
/// Under [`ErrorMode::Full`] a fetch failure returns [`ResultsMissingError`];
/// under [`ErrorMode::Tolerant`] it is logged and swallowed.
async fn subdl(
    fetcher: &dyn BytesFetcher,
    remote: &str,
    local: &Path,
    test: &Test,
    mode: ErrorMode,
) -> Result<(), ResultsMissingError> {
    tracing::info!("Downloading log {remote}");
    match fetcher.get_bytes(remote).await {
        Ok(data) => {
            if let Err(e) = atomic_write_file(&data, local) {
                tracing::error!("Failed to write {}: {e}", local.display());
            }
            Ok(())
        }
        Err(error) => {
            tracing::error!("Download from {remote} failed: {error}");
            match mode {
                ErrorMode::Full => Err(ResultsMissingError {
                    test: test.name.clone(),
                    arch: test.arch.clone(),
                }),
                ErrorMode::Tolerant => Ok(()),
            }
        }
    }
}

/// Downloads all logs for a set of `(host, tests)` connectors.
///
/// Ports upstream `download_logs`: builds the `(host, test)` matrix, dispatches
/// each to its downloader, runs them with bounded concurrency, and — under
/// [`ErrorMode::Full`] — returns the first [`ResultsMissingError`] after the
/// whole batch has finished.
///
/// # Errors
///
/// Returns the first [`ResultsMissingError`] when `mode` is [`ErrorMode::Full`]
/// and any download failed.
pub async fn download_logs(
    fetcher: &dyn BytesFetcher,
    connectors: &[(String, Vec<Test>)],
    resultsdir: &Path,
    installlogsdir: &Path,
    mode: ErrorMode,
) -> Result<(), ResultsMissingError> {
    // Flatten to (host, test) pairs, keeping only those with a log to fetch.
    let jobs: Vec<(&str, &Test, String, PathBuf)> = connectors
        .iter()
        .flat_map(|(host, tests)| {
            tests.iter().filter_map(move |test| {
                plan(host, test, resultsdir, installlogsdir)
                    .map(|(remote, local)| (host.as_str(), test, remote, local))
            })
        })
        .collect();

    let total = jobs.len();
    let results: Vec<Result<(), ResultsMissingError>> = stream::iter(jobs)
        .map(|(_host, test, remote, local)| async move {
            subdl(fetcher, &remote, &local, test, mode).await
        })
        .buffer_unordered(DOWNLOAD_CONCURRENCY)
        .collect()
        .await;

    let failures: Vec<ResultsMissingError> = results.into_iter().filter_map(Result::err).collect();
    if !failures.is_empty() {
        tracing::warn!("{} of {total} openQA log downloads failed", failures.len());
        if mode == ErrorMode::Full {
            return Err(failures.into_iter().next().expect("non-empty checked"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn test(name: &str) -> Test {
        Test::new(
            name,
            "passed",
            42,
            "x86_64",
            std::collections::BTreeMap::new(),
        )
    }

    struct OkFetcher {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BytesFetcher for OkFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            self.seen.lock().unwrap().push(url.to_string());
            Ok(b"log-bytes".to_vec())
        }
    }

    struct FailFetcher;

    #[async_trait]
    impl BytesFetcher for FailFetcher {
        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, String> {
            Err("404".to_string())
        }
    }

    #[test]
    fn dispatch_by_name_prefix() {
        assert_eq!(LogKind::for_name("install_kernel"), LogKind::Install);
        assert_eq!(LogKind::for_name("ltp"), LogKind::Ltp);
        assert_eq!(LogKind::for_name("something_else"), LogKind::Empty);
    }

    #[test]
    fn plan_builds_expected_paths() {
        let (remote, local) = plan(
            "http://h",
            &test("install_kernel"),
            Path::new("/res"),
            Path::new("/inst"),
        )
        .unwrap();
        assert!(remote.contains("update_kernel-zypper.log"));
        assert_eq!(local, Path::new("/inst/h-zypper-x86_64.log"));

        let (remote, local) = plan(
            "http://h",
            &test("ltp"),
            Path::new("/res"),
            Path::new("/inst"),
        )
        .unwrap();
        assert!(remote.contains("result_array.json"));
        assert_eq!(local, Path::new("/res/h-x86_64-ltp.json"));

        assert!(plan("http://h", &test("other"), Path::new("/r"), Path::new("/i")).is_none());
    }

    #[tokio::test]
    async fn download_writes_and_skips_empty() {
        let dir = tempfile::tempdir().unwrap();
        let res = dir.path().join("results");
        let inst = dir.path().join("install");
        let fetcher = OkFetcher {
            seen: Mutex::new(Vec::new()),
        };
        let connectors = vec![(
            "http://h".to_string(),
            vec![test("install_kernel"), test("ltp"), test("noop")],
        )];

        download_logs(&fetcher, &connectors, &res, &inst, ErrorMode::Tolerant)
            .await
            .unwrap();

        // Two files fetched (install + ltp); the "noop" test is skipped.
        assert_eq!(fetcher.seen.lock().unwrap().len(), 2);
        assert_eq!(
            std::fs::read(inst.join("h-zypper-x86_64.log")).unwrap(),
            b"log-bytes"
        );
        assert!(res.join("h-x86_64-ltp.json").exists());
    }

    #[tokio::test]
    async fn tolerant_swallows_failures() {
        let dir = tempfile::tempdir().unwrap();
        let connectors = vec![("http://h".to_string(), vec![test("install_kernel")])];
        let out = download_logs(
            &FailFetcher,
            &connectors,
            dir.path(),
            dir.path(),
            ErrorMode::Tolerant,
        )
        .await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn full_returns_results_missing() {
        let dir = tempfile::tempdir().unwrap();
        let connectors = vec![("http://h".to_string(), vec![test("install_kernel")])];
        let err = download_logs(
            &FailFetcher,
            &connectors,
            dir.path(),
            dir.path(),
            ErrorMode::Full,
        )
        .await
        .unwrap_err();
        assert_eq!(err.test, "install_kernel");
        assert_eq!(err.arch, "x86_64");
        assert!(err.to_string().contains("missing results.json file"));
    }
}
