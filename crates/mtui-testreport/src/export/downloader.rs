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

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use mtui_types::Test;

use crate::support::fileops::atomic_write_file;

/// Max concurrent downloads.
pub(super) const DOWNLOAD_CONCURRENCY: usize = 8;

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
pub(super) fn is_safe_component(s: &str) -> bool {
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

/// One planned download: the test, its remote URL and its local destination.
struct Job {
    test: Test,
    remote: String,
    local: PathBuf,
}

/// `local` with `-job-<id>` (and `-<n>` once `n > 1`) inserted before the
/// extension.
fn with_job_suffix(local: &Path, id: i64, n: u32) -> PathBuf {
    let mut name = local.file_stem().unwrap_or_default().to_os_string();
    name.push(format!("-job-{id}"));
    if n > 1 {
        name.push(format!("-{n}"));
    }
    if let Some(ext) = local.extension() {
        name.push(".");
        name.push(ext);
    }
    local.with_file_name(name)
}

/// Gives every distinct download its own local path.
///
/// Jobs that would write the same path get a job-ID suffix; a path claimed by
/// only one job is left alone. Names are assigned in `(test_id, remote)` order
/// over the whole batch, so they depend on the set of jobs, never on input or
/// completion order. An identical `(remote, local)` pair is one download.
fn resolve_collisions(jobs: Vec<Job>) -> Vec<Job> {
    let mut seen = HashSet::new();
    let mut jobs: Vec<Job> = jobs
        .into_iter()
        .filter(|j| seen.insert((j.remote.clone(), j.local.clone())))
        .collect();

    let mut groups: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
    for (i, job) in jobs.iter().enumerate() {
        groups.entry(job.local.clone()).or_default().push(i);
    }
    // Every legacy path is reserved up front so a suffixed name can never take
    // the destination of a job that did not collide.
    let mut taken: HashSet<PathBuf> = groups.keys().cloned().collect();

    for mut members in groups.into_values().filter(|m| m.len() > 1) {
        members.sort_by(|&a, &b| {
            (jobs[a].test.test_id, &jobs[a].remote).cmp(&(jobs[b].test.test_id, &jobs[b].remote))
        });
        for i in members {
            let id = jobs[i].test.test_id;
            let mut n = 1;
            let unique = loop {
                let candidate = with_job_suffix(&jobs[i].local, id, n);
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                n += 1;
            };
            jobs[i].local = unique;
        }
    }
    jobs
}

/// Runs a blocking filesystem job off the async worker, mapping a join failure
/// to an [`io::Error`].
///
/// The spawned job runs to completion even if this future is dropped, so
/// callers await it rather than abandon it.
async fn run_file_write(job: impl FnOnce() -> io::Result<()> + Send + 'static) -> io::Result<()> {
    match tokio::task::spawn_blocking(job).await {
        Ok(result) => result,
        Err(join) => Err(io::Error::other(format!("write task failed: {join}"))),
    }
}

/// Atomically writes a downloaded log to `destination` off the async worker, so
/// a slow filesystem (a network mount) cannot block a Tokio thread.
pub(super) async fn persist_download(data: Vec<u8>, destination: PathBuf) -> io::Result<()> {
    run_file_write(move || atomic_write_file(&data, &destination)).await
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
            let err = match persist_download(data, local.to_path_buf()).await {
                Ok(()) => return Ok(()),
                Err(e) => e,
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
    let jobs = resolve_collisions(
        connectors
            .iter()
            .flat_map(|(host, tests)| {
                tests.iter().filter_map(move |test| {
                    plan(host, test, resultsdir, installlogsdir).map(|(remote, local)| Job {
                        test: test.clone(),
                        remote,
                        local,
                    })
                })
            })
            .collect(),
    );

    let total = jobs.len();
    let results: Vec<Result<(), DownloadError>> = stream::iter(jobs)
        .map(
            |Job {
                 test,
                 remote,
                 local,
             }| async move { subdl(fetcher, &remote, &local, &test, mode).await },
        )
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
    use std::sync::{Arc, Mutex};

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

    fn job(name: &str, arch: &str, id: i64) -> Test {
        Test::new(name, "passed", id, arch, std::collections::BTreeMap::new())
    }

    /// Answers every URL with the URL itself, so a file's body names the remote
    /// it came from.
    struct EchoFetcher;

    #[async_trait]
    impl BytesFetcher for EchoFetcher {
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
            Ok(url.as_bytes().to_vec())
        }
    }

    fn sorted_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[tokio::test]
    async fn kernel_export_collision_install_jobs_keep_both_logs() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = (dir.path().join("results"), dir.path().join("install"));
        let connectors = vec![(
            "http://h".to_string(),
            vec![
                job("install_kernel", "x86_64", 41),
                job("install_kernel", "x86_64", 42),
            ],
        )];

        download_logs(&EchoFetcher, &connectors, &res, &inst, ErrorMode::Full)
            .await
            .unwrap();

        assert_eq!(
            sorted_names(&inst),
            ["h-zypper-x86_64-job-41.log", "h-zypper-x86_64-job-42.log"]
        );
        for id in [41, 42] {
            assert_eq!(
                std::fs::read_to_string(inst.join(format!("h-zypper-x86_64-job-{id}.log")))
                    .unwrap(),
                format!("http://h/tests/{id}/file/update_kernel-zypper.log")
            );
        }
    }

    #[tokio::test]
    async fn kernel_export_collision_ltp_jobs_keep_both_results() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = (dir.path().join("results"), dir.path().join("install"));
        let connectors = vec![(
            "http://h".to_string(),
            vec![job("ltp", "x86_64", 41), job("ltp", "x86_64", 42)],
        )];

        download_logs(&EchoFetcher, &connectors, &res, &inst, ErrorMode::Full)
            .await
            .unwrap();

        assert_eq!(
            sorted_names(&res),
            ["h-x86_64-ltp-job-41.json", "h-x86_64-ltp-job-42.json"]
        );
        for id in [41, 42] {
            assert_eq!(
                std::fs::read_to_string(res.join(format!("h-x86_64-ltp-job-{id}.json"))).unwrap(),
                format!("http://h/tests/{id}/file/result_array.json")
            );
        }
    }

    fn planned(host: &str, test: &Test) -> Job {
        let (remote, local) = plan(host, test, Path::new("/res"), Path::new("/inst")).unwrap();
        Job {
            test: test.clone(),
            remote,
            local,
        }
    }

    fn mapping(jobs: &[Job]) -> BTreeMap<String, PathBuf> {
        jobs.iter()
            .map(|j| (j.remote.clone(), j.local.clone()))
            .collect()
    }

    #[test]
    fn kernel_export_collision_resolver_suffixes_only_colliding_paths() {
        let jobs = vec![
            planned("http://h", &job("install_kernel", "x86_64", 41)),
            planned("http://h", &job("install_kernel", "x86_64", 42)),
            planned("http://h", &job("install_kernel", "aarch64", 43)),
            planned("http://h", &job("ltp", "x86_64", 44)),
            planned("http://h", &job("ltp", "x86_64", 45)),
            planned("http://h", &job("ltp_other", "x86_64", 46)),
        ];

        let got = mapping(&resolve_collisions(jobs));

        let expected: BTreeMap<String, PathBuf> = [
            (41, "/inst/h-zypper-x86_64-job-41.log"),
            (42, "/inst/h-zypper-x86_64-job-42.log"),
            (43, "/inst/h-zypper-aarch64.log"),
            (44, "/res/h-x86_64-ltp-job-44.json"),
            (45, "/res/h-x86_64-ltp-job-45.json"),
            (46, "/res/h-x86_64-ltp_other.json"),
        ]
        .into_iter()
        .map(|(id, path)| {
            let kind = if path.contains("zypper") {
                "update_kernel-zypper.log"
            } else {
                "result_array.json"
            };
            (
                format!("http://h/tests/{id}/file/{kind}"),
                PathBuf::from(path),
            )
        })
        .collect();
        assert_eq!(got, expected);
        let unique: HashSet<_> = got.values().collect();
        assert_eq!(unique.len(), got.len());
    }

    #[test]
    fn kernel_export_collision_resolver_ignores_input_order() {
        let jobs = || {
            vec![
                planned("http://h", &job("install_kernel", "x86_64", 42)),
                planned("https://a.example/h", &job("install_kernel", "x86_64", 41)),
                planned("https://b.example/h", &job("install_kernel", "x86_64", 41)),
                planned("http://h", &job("install_kernel", "x86_64", 41)),
            ]
        };
        let forward = mapping(&resolve_collisions(jobs()));
        let mut reversed = jobs();
        reversed.reverse();
        let backward = mapping(&resolve_collisions(reversed));

        assert_eq!(forward, backward);
        let unique: HashSet<_> = forward.values().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn kernel_export_collision_same_id_from_two_sources_gets_distinct_names() {
        let jobs = vec![
            planned("https://b.example/h", &job("install_kernel", "x86_64", 41)),
            planned("https://a.example/h", &job("install_kernel", "x86_64", 41)),
        ];

        let got = mapping(&resolve_collisions(jobs));

        assert_eq!(
            got["https://a.example/h/tests/41/file/update_kernel-zypper.log"],
            Path::new("/inst/h-zypper-x86_64-job-41.log")
        );
        assert_eq!(
            got["https://b.example/h/tests/41/file/update_kernel-zypper.log"],
            Path::new("/inst/h-zypper-x86_64-job-41-2.log")
        );
    }

    #[test]
    fn kernel_export_collision_generated_name_never_takes_a_singleton_path() {
        // The singleton's legacy name equals job 41's would-be suffixed name.
        let singleton = planned("http://h", &job("install_kernel", "x86_64-job-41", 7));
        assert_eq!(
            singleton.local,
            Path::new("/inst/h-zypper-x86_64-job-41.log")
        );
        let jobs = vec![
            planned("http://h", &job("install_kernel", "x86_64", 41)),
            planned("http://h", &job("install_kernel", "x86_64", 42)),
            singleton,
        ];

        let got = mapping(&resolve_collisions(jobs));

        assert_eq!(
            got["http://h/tests/7/file/update_kernel-zypper.log"],
            Path::new("/inst/h-zypper-x86_64-job-41.log")
        );
        assert_eq!(
            got["http://h/tests/41/file/update_kernel-zypper.log"],
            Path::new("/inst/h-zypper-x86_64-job-41-2.log")
        );
        assert_eq!(
            got["http://h/tests/42/file/update_kernel-zypper.log"],
            Path::new("/inst/h-zypper-x86_64-job-42.log")
        );
        let unique: HashSet<_> = got.values().collect();
        assert_eq!(unique.len(), 3);
        for path in got.values() {
            assert!(is_safe_component(
                path.file_name().unwrap().to_str().unwrap()
            ));
        }
    }

    #[tokio::test]
    async fn kernel_export_collision_identical_duplicate_is_fetched_once() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = (dir.path().join("results"), dir.path().join("install"));
        let fetcher = OkFetcher {
            seen: Mutex::new(Vec::new()),
        };
        let connectors = vec![
            (
                "http://h".to_string(),
                vec![job("install_kernel", "x86_64", 41)],
            ),
            (
                "http://h".to_string(),
                vec![job("install_kernel", "x86_64", 41)],
            ),
        ];

        download_logs(&fetcher, &connectors, &res, &inst, ErrorMode::Full)
            .await
            .unwrap();

        assert_eq!(fetcher.seen.lock().unwrap().len(), 1);
        assert_eq!(sorted_names(&inst), ["h-zypper-x86_64.log"]);
    }

    #[tokio::test]
    async fn kernel_export_collision_non_colliding_jobs_keep_legacy_names() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = (dir.path().join("results"), dir.path().join("install"));
        let connectors = vec![
            (
                "http://h1".to_string(),
                vec![
                    job("install_kernel", "x86_64", 41),
                    job("ltp", "x86_64", 42),
                ],
            ),
            (
                "http://h2".to_string(),
                vec![
                    job("install_kernel", "x86_64", 43),
                    job("noop", "x86_64", 44),
                ],
            ),
        ];

        download_logs(&EchoFetcher, &connectors, &res, &inst, ErrorMode::Full)
            .await
            .unwrap();

        assert_eq!(
            sorted_names(&inst),
            ["h1-zypper-x86_64.log", "h2-zypper-x86_64.log"]
        );
        assert_eq!(sorted_names(&res), ["h1-x86_64-ltp.json"]);
    }

    #[tokio::test]
    async fn kernel_export_collision_names_ignore_completion_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Semaphore;

        /// Holds each fetch until the test releases that URL.
        struct GatedFetcher {
            gates: BTreeMap<String, Semaphore>,
            done: AtomicUsize,
        }

        #[async_trait]
        impl BytesFetcher for GatedFetcher {
            async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
                self.gates[url].acquire().await.unwrap().forget();
                self.done.fetch_add(1, Ordering::SeqCst);
                Ok(url.as_bytes().to_vec())
            }
        }

        let urls = [
            "http://h/tests/41/file/update_kernel-zypper.log".to_string(),
            "http://h/tests/42/file/update_kernel-zypper.log".to_string(),
        ];
        let mut outcomes = Vec::new();
        for release_order in [[0, 1], [1, 0]] {
            let dir = tempfile::tempdir().unwrap();
            let (res, inst) = (dir.path().join("results"), dir.path().join("install"));
            let fetcher = GatedFetcher {
                gates: urls
                    .iter()
                    .map(|u| (u.clone(), Semaphore::new(0)))
                    .collect(),
                done: AtomicUsize::new(0),
            };
            let connectors = vec![(
                "http://h".to_string(),
                vec![
                    job("install_kernel", "x86_64", 41),
                    job("install_kernel", "x86_64", 42),
                ],
            )];
            let driver = async {
                for (finished, idx) in release_order.into_iter().enumerate() {
                    fetcher.gates[&urls[idx]].add_permits(1);
                    while fetcher.done.load(Ordering::SeqCst) <= finished {
                        tokio::task::yield_now().await;
                    }
                }
            };

            let (out, ()) = tokio::join!(
                download_logs(&fetcher, &connectors, &res, &inst, ErrorMode::Full),
                driver
            );
            out.unwrap();

            outcomes.push(
                sorted_names(&inst)
                    .into_iter()
                    .map(|n| {
                        let body = std::fs::read_to_string(inst.join(&n)).unwrap();
                        (n, body)
                    })
                    .collect::<Vec<_>>(),
            );
        }

        assert_eq!(outcomes[0], outcomes[1]);
        assert_eq!(
            outcomes[0],
            [
                ("h-zypper-x86_64-job-41.log".to_string(), urls[0].clone()),
                ("h-zypper-x86_64-job-42.log".to_string(), urls[1].clone()),
            ]
        );
    }

    #[tokio::test]
    async fn kernel_export_collision_reexport_overwrites_same_files() {
        let dir = tempfile::tempdir().unwrap();
        let (res, inst) = (dir.path().join("results"), dir.path().join("install"));
        let connectors = vec![(
            "http://h".to_string(),
            vec![
                job("install_kernel", "x86_64", 41),
                job("install_kernel", "x86_64", 42),
            ],
        )];

        for _ in 0..2 {
            download_logs(&EchoFetcher, &connectors, &res, &inst, ErrorMode::Full)
                .await
                .unwrap();
        }

        assert_eq!(
            sorted_names(&inst),
            ["h-zypper-x86_64-job-41.log", "h-zypper-x86_64-job-42.log"]
        );
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

    #[tokio::test]
    async fn file_write_job_runs_off_the_runtime_worker() {
        let worker = std::thread::current().id();

        let ran_on = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&ran_on);
        run_file_write(move || {
            *slot.lock().unwrap() = Some(std::thread::current().id());
            Ok(())
        })
        .await
        .unwrap();

        let ran_on = ran_on.lock().unwrap().expect("job ran");
        assert_ne!(ran_on, worker);
    }

    #[tokio::test]
    async fn file_write_job_failure_is_returned_unchanged() {
        let err = run_file_write(|| Err(io::Error::new(io::ErrorKind::PermissionDenied, "nope")))
            .await
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(err.to_string(), "nope");
    }

    #[tokio::test]
    async fn file_write_job_panic_maps_to_a_write_task_error() {
        let err = run_file_write(|| panic!("boom")).await.unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(
            err.to_string().starts_with("write task failed:"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn persist_download_creates_parents_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested").join("log.txt");

        persist_download(b"first\n".to_vec(), dest.clone())
            .await
            .unwrap();
        persist_download(b"second\n".to_vec(), dest.clone())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"second\n");
        assert_eq!(sorted_names(dest.parent().unwrap()), ["log.txt"]);
    }

    #[tokio::test]
    async fn persist_download_reports_an_unwritable_parent() {
        let dir = tempfile::tempdir().unwrap();
        let (res, _) = unwritable_dirs(dir.path());

        persist_download(b"x".to_vec(), res.join("log.txt"))
            .await
            .unwrap_err();

        assert!(!res.exists());
    }
}
