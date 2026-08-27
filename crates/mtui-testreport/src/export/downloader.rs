//! Download openQA logs for the auto/kernel exporters.
//!
//! Each openQA test maps to a downloader by the first `_`-segment of its name
//! (`install` → the zypper install log, `ltp` → the result-array JSON), with an
//! empty-log fallback for everything else. Downloads fan out async (tokio) with
//! bounded concurrency.
//!
//! Error modes: `tolerant` logs and skips a failed download *or* local write;
//! `full` returns the first [`ResultsMissingError`] / [`DownloadError::Write`]
//! once the whole batch has finished.
//!
//! HTTP is abstracted behind the [`BytesFetcher`] trait so the exporters inject
//! an [`HttpClient`](mtui_datasources::http::HttpClient)-backed fetcher while
//! tests inject a mock.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use mtui_types::Test;

use crate::support::fileops::atomic_write_file;

/// Max concurrent downloads.
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
/// Its `Display` string is a stable, user-facing message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Test: {test} on arch: {arch} missing results.json file. Please restart it.")]
pub struct ResultsMissingError {
    /// The test name.
    test: String,
    /// The architecture.
    arch: String,
}

/// A download failure surfaced under [`ErrorMode::Full`].
///
/// Separates a *download* failure from a *write* failure (fetched, but not
/// persistable locally, or its off-thread write task failed to join), giving
/// callers a distinct local-write signal. Not `Clone`/`Eq`: the
/// [`Write`](DownloadError::Write) variant carries a [`std::io::Error`].
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The openQA log could not be downloaded.
    #[error(transparent)]
    ResultsMissing(#[from] ResultsMissingError),
    /// The downloaded log could not be written to its local path (a filesystem
    /// error, or the [`spawn_blocking`](tokio::task::spawn_blocking) write task
    /// failed to join).
    #[error("Failed to write {path}: {source}")]
    Write {
        /// The local path the log was to be written to.
        path: PathBuf,
        /// The underlying I/O (or synthesized join) failure.
        source: std::io::Error,
    },
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
    /// Dispatches on `name.split('_')[0]`.
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

/// The last `/`-segment of `host`.
fn host_tail(host: &str) -> &str {
    host.rsplit('/').next().unwrap_or(host)
}

/// Whether `s` is a single, ordinary path component safe to embed in a local
/// filename.
///
/// [`plan`] interpolates openQA-controlled strings (`host_tail`, `test.arch`,
/// `test.name`) into local write paths; a separator, a `..` component or a
/// control byte would let a hostile response escape the export directories and
/// overwrite arbitrary files. Mirrors
/// `mtui_hosts::connection::ssh::validate_sftp_component`, ported rather than
/// shared because `mtui-testreport` must not depend on the lower `mtui-hosts`.
fn is_safe_component(s: &str) -> bool {
    // `\` is rejected regardless of host OS: the *local* side may be Windows.
    if s.is_empty()
        || s == "."
        || s == ".."
        || s.contains('/')
        || s.contains('\\')
        || s.chars().any(char::is_control)
    {
        return false;
    }
    // Must resolve to exactly one normal component identical to the input:
    // catches drive/root prefixes and platform-specific separator forms.
    let mut comps = Path::new(s).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(std::path::Component::Normal(c)), None) if c == s
    )
}

/// Builds the `(remote_url, local_path)` for a test, or `None` for an empty log.
fn plan(
    host: &str,
    test: &Test,
    resultsdir: &Path,
    installlogsdir: &Path,
) -> Option<(String, PathBuf)> {
    let kind = LogKind::for_name(&test.name);
    // `Empty` has no local path, so it needs neither the check nor its ERROR.
    let tail = host_tail(host);
    if kind != LogKind::Empty
        && !(is_safe_component(tail)
            && is_safe_component(&test.arch)
            && is_safe_component(&test.name))
    {
        tracing::error!(
            "Refusing unsafe export path component for test {:?} (arch {:?}) on {host}",
            test.name,
            test.arch
        );
        return None;
    }
    match kind {
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

/// Downloads one log: fetch + atomic write.
///
/// Under [`ErrorMode::Full`] a fetch failure returns [`ResultsMissingError`] and
/// a local-write (or write-task join) failure returns [`DownloadError::Write`];
/// under [`ErrorMode::Tolerant`] both are logged and swallowed.
async fn subdl(
    fetcher: &dyn BytesFetcher,
    remote: &str,
    local: &Path,
    test: &Test,
    mode: ErrorMode,
) -> Result<(), DownloadError> {
    tracing::info!("Downloading log {remote}");
    match fetcher.get_bytes(remote).await {
        Ok(data) => {
            // Off the async worker: a slow filesystem (network mount) must not
            // block a Tokio thread mid-fan-out.
            let local_owned = local.to_path_buf();
            let write =
                tokio::task::spawn_blocking(move || atomic_write_file(&data, &local_owned)).await;
            let err = match write {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(e)) => e,
                Err(join) => std::io::Error::other(format!("write task failed: {join}")),
            };
            tracing::error!("Failed to write {}: {err}", local.display());
            match mode {
                ErrorMode::Full => Err(DownloadError::Write {
                    path: local.to_path_buf(),
                    source: err,
                }),
                ErrorMode::Tolerant => Ok(()),
            }
        }
        Err(error) => {
            tracing::error!("Download from {remote} failed: {error}");
            match mode {
                ErrorMode::Full => Err(ResultsMissingError {
                    test: test.name.clone(),
                    arch: test.arch.clone(),
                }
                .into()),
                ErrorMode::Tolerant => Ok(()),
            }
        }
    }
}

/// Downloads all logs for a set of `(host, tests)` connectors.
///
/// Builds the `(host, test)` matrix, dispatches each to its downloader, runs
/// them with bounded concurrency, and — under [`ErrorMode::Full`] — returns
/// the first [`DownloadError`] after the whole batch has finished.
///
/// # Errors
///
/// Returns the first [`DownloadError`] when `mode` is [`ErrorMode::Full`] and any
/// download or local write failed (a fetch failure as
/// [`DownloadError::ResultsMissing`], a persist failure as
/// [`DownloadError::Write`]).
pub async fn download_logs(
    fetcher: &dyn BytesFetcher,
    connectors: &[(String, Vec<Test>)],
    resultsdir: &Path,
    installlogsdir: &Path,
    mode: ErrorMode,
) -> Result<(), DownloadError> {
    // The `Test` is cloned into each job so the download future owns its data:
    // a borrow across the async block defeats higher-ranked-lifetime inference
    // when the batch is awaited inside a spawned/boxed future.
    let jobs: Vec<(Test, String, PathBuf)> = connectors
        .iter()
        .flat_map(|(host, tests)| {
            tests.iter().filter_map(move |test| {
                plan(host, test, resultsdir, installlogsdir)
                    .map(|(remote, local)| (test.clone(), remote, local))
            })
        })
        .collect();

    let total = jobs.len();
    let results: Vec<Result<(), DownloadError>> =
        stream::iter(jobs)
            .map(|(test, remote, local)| async move {
                subdl(fetcher, &remote, &local, &test, mode).await
            })
            .buffer_unordered(DOWNLOAD_CONCURRENCY)
            .collect()
            .await;

    let failures: Vec<DownloadError> = results.into_iter().filter_map(Result::err).collect();
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
        test_arch(name, "x86_64")
    }

    fn test_arch(name: &str, arch: &str) -> Test {
        Test::new(name, "passed", 42, arch, std::collections::BTreeMap::new())
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
    fn is_safe_component_accepts_benign_and_rejects_traversal() {
        for ok in ["h", "x86_64", "install_kernel", "install_kernel.foo", "a-b"] {
            assert!(is_safe_component(ok), "should accept {ok:?}");
        }
        for bad in [
            "", ".", "..", "../x", "a/b", "a\\b", "a\0b", "/abs", "a\nb", "sub/",
        ] {
            assert!(!is_safe_component(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn plan_rejects_traversal_components() {
        let r = Path::new("/res");
        let i = Path::new("/inst");
        // Traversal in test name.
        assert!(plan("http://h", &test("install_../../evil"), r, i).is_none());
        assert!(plan("http://h", &test("ltp/../../evil"), r, i).is_none());
        // Traversal in arch (keep a downloadable name prefix).
        assert!(plan("http://h", &test_arch("install_k", "../etc"), r, i).is_none());
        assert!(plan("http://h", &test_arch("ltp", "a/b"), r, i).is_none());
        // Unsafe host tail.
        assert!(plan("http://h/..", &test("install_k"), r, i).is_none());
        assert!(plan("http://h/", &test("install_k"), r, i).is_none());
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
    async fn traversal_named_test_is_skipped_and_not_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let res = dir.path().join("results");
        let inst = dir.path().join("install");
        let fetcher = OkFetcher {
            seen: Mutex::new(Vec::new()),
        };
        let connectors = vec![(
            "http://h".to_string(),
            vec![test("install_../../evil"), test("ltp/../../evil")],
        )];

        download_logs(&fetcher, &connectors, &res, &inst, ErrorMode::Tolerant)
            .await
            .unwrap();

        // No fetch attempted and nothing written anywhere under the temp root.
        assert!(fetcher.seen.lock().unwrap().is_empty());
        assert!(!dir.path().join("evil").exists());
        assert!(!res.exists() || std::fs::read_dir(&res).unwrap().next().is_none());
        assert!(!inst.exists() || std::fs::read_dir(&inst).unwrap().next().is_none());
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
        let DownloadError::ResultsMissing(missing) = err else {
            panic!("expected ResultsMissing, got {err:?}");
        };
        assert_eq!(missing.test, "install_kernel");
        assert_eq!(missing.arch, "x86_64");
        assert!(missing.to_string().contains("missing results.json file"));
    }

    /// A download that succeeds but cannot be persisted: point the install-logs
    /// dir at a path whose parent is a *regular file*, so `atomic_write_file`
    /// fails deterministically on any OS (no perms/root tricks).
    fn unwritable_dirs(dir: &Path) -> (PathBuf, PathBuf) {
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        // Both resultsdir and installlogsdir live *under* the regular file.
        (file.join("results"), file.join("install"))
    }

    #[tokio::test]
    async fn full_propagates_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = unwritable_dirs(dir.path());
        let fetcher = OkFetcher {
            seen: Mutex::new(Vec::new()),
        };
        let connectors = vec![("http://h".to_string(), vec![test("install_kernel")])];

        let err = download_logs(&fetcher, &connectors, &res, &inst, ErrorMode::Full)
            .await
            .unwrap_err();

        // The fetch succeeded, so this is a Write failure, not ResultsMissing.
        assert_eq!(fetcher.seen.lock().unwrap().len(), 1);
        let DownloadError::Write { path, .. } = err else {
            panic!("expected Write, got {err:?}");
        };
        assert!(path.ends_with("h-zypper-x86_64.log"), "path was {path:?}");
    }

    #[tokio::test]
    async fn tolerant_swallows_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = unwritable_dirs(dir.path());
        let fetcher = OkFetcher {
            seen: Mutex::new(Vec::new()),
        };
        let connectors = vec![("http://h".to_string(), vec![test("install_kernel")])];

        let out = download_logs(&fetcher, &connectors, &res, &inst, ErrorMode::Tolerant).await;

        // Fetch happened, write failed, but tolerant mode still returns Ok.
        assert_eq!(fetcher.seen.lock().unwrap().len(), 1);
        assert!(out.is_ok());
    }
}
