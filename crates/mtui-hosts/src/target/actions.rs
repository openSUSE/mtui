//! Parallel fan-out primitives over a group of [`Target`]s.
//!
//! `run_parallel` drives caller-supplied futures concurrently, `run_command` dispatches
//! a command (one string, or a per-host map) to every host, and `sftp_put_all` /
//! `sftp_get_all` fan transfers out.
//!
//! Each [`Target`] owns its own [`HostLog`](mtui_types::hostlog::HostLog) behind
//! `&mut self`, so concurrent tasks hold disjoint mutable borrows and no output lock
//! is needed.
//!
//! `run_parallel` runs a [`TtySpinner`](mod@super::spinner) for the fan-out when a
//! `desc` is given (a strict no-op off a TTY), erased on stop and by the prompter's
//! `suspend` guard during an interactive read.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use super::Target;

/// Fallback fan-out width when a caller passes `0`; mirrors the `[connection]
/// max_parallel` default without a dependency on `mtui-config`.
const DEFAULT_MAX_PARALLEL: usize = 50;

/// Upper bound (bytes) for reading an upload payload once into a shared buffer.
///
/// At or below it the fan-out shares one `Arc<[u8]>` across hosts (peak RSS
/// ~O(payload)); above it each host streams the file itself to stay bounded. mtui
/// uploads are small test scripts, so the shared path is the common case.
const SHARED_UPLOAD_CAP: u64 = 8 * 1024 * 1024;

/// Drives every future in `futures` to completion concurrently; an empty input
/// returns immediately.
///
/// When `desc` is `Some`, a labelled [`TtySpinner`](super::spinner::TtySpinner)
/// paints for the fan-out (a no-op off a TTY) and is stopped when the batch
/// completes.
///
/// The per-target futures never fail: [`Target::run`] and the SFTP methods swallow
/// and log their own errors (the `-1`-sentinel / log-not-propagate contract), so one
/// bad host never aborts the fan-out and this returns `()`.
///
/// `max_parallel` bounds how many futures are polled at once; `0` falls back to
/// [`DEFAULT_MAX_PARALLEL`]. Completion order is irrelevant to every caller (results
/// attach to disjoint `&mut Target` borrows), so `buffer_unordered` is observably
/// equivalent to an unbounded `join_all`.
///
/// **A fan-out is never cancelled part-way.** Cancellation is checked at explicit
/// call-site boundaries (see `HostsGroup::cancel_requested`), never here: a host
/// skipped mid-batch is indistinguishable downstream from one that ran, so its stale
/// `last*` snapshot would pass the post-run checks and report the update as applied.
async fn run_parallel<I, F>(futures: I, desc: Option<&str>, max_parallel: usize)
where
    I: IntoIterator<Item = F>,
    F: std::future::Future<Output = ()>,
{
    let futs: Vec<F> = futures.into_iter().collect();
    if futs.is_empty() {
        return;
    }
    let bound = if max_parallel == 0 {
        DEFAULT_MAX_PARALLEL
    } else {
        max_parallel
    };
    // A labelled spinner for the batch; a no-op off a TTY.
    let mut spinner = desc.map(|d| {
        let mut s = super::spinner::TtySpinner::new(d);
        s.start();
        // Report the resolved paint state so `mtui -d` shows why a spinner is
        // (in)visible: `enabled=false` means stderr was not detected as a TTY (piped,
        // tmux-detached, IDE terminal).
        tracing::debug!(
            action = d,
            count = futs.len(),
            enabled = s.is_enabled(),
            "running in parallel (fan-out spinner)"
        );
        s
    });
    stream::iter(futs)
        .buffer_unordered(bound)
        .for_each(|()| async {})
        .await;
    if let Some(mut s) = spinner.take() {
        s.stop();
    }
}

/// A boxed per-target future, borrowing the target for `'a`.
///
/// [`run_fanout`]'s `op` returns this because a bare `Fn(&mut Target) -> impl Future`
/// bound cannot tie the future's lifetime to the borrow on stable Rust (`AsyncFn` is
/// unstable); callers wrap their async block in [`Box::pin`]. The allocation is
/// negligible next to the SSH round trip each op performs.
pub(crate) type BoxTargetFut<'a, T = ()> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Boxes a per-target future for [`run_fanout`]; the function signature drives
/// the unsizing coercion that a bare `Box::pin` inside a closure does not.
pub(crate) fn boxed<'a, T>(
    fut: impl std::future::Future<Output = T> + Send + 'a,
) -> BoxTargetFut<'a, T> {
    Box::pin(fut)
}

/// Drives a per-target async `op` across a group in parallel: the single fan-out
/// primitive every per-host I/O method on [`HostsGroup`](super::HostsGroup) routes
/// through.
///
/// `desc` labels the optional TTY spinner. `op` is invoked once per non-skipped
/// target with `&mut Target`; the disjoint borrows make the batch sound without a
/// lock. `should_run` lets a caller skip a target (e.g. one a [`Command::PerHost`]
/// map does not cover).
pub(crate) async fn run_fanout<'t, S, F>(
    targets: &'t mut BTreeMap<String, Target>,
    is_repl: bool,
    max_parallel: usize,
    desc: Option<&str>,
    mut should_run: S,
    op: F,
) where
    S: FnMut(&Target) -> bool,
    F: Fn(&'t mut Target) -> BoxTargetFut<'t>,
{
    let parallel: Vec<&'t mut Target> = targets
        .values_mut()
        .filter(|target| should_run(target))
        .collect();

    // Diagnostic (`mtui -d`): `is_repl=false` or `parallel=0` suppresses the fan-out
    // spinner even on a TTY / with `MTUI_FORCE_SPINNER`.
    tracing::debug!(
        is_repl,
        desc = desc.unwrap_or("<none>"),
        parallel = parallel.len(),
        "fan-out dispatch"
    );

    // Each future borrows a distinct target, so no shared lock is needed.
    let parallel_futs: Vec<_> = parallel.into_iter().map(&op).collect();
    run_parallel(
        parallel_futs,
        if is_repl { desc } else { None },
        max_parallel,
    )
    .await;
}

/// A command to run across a group: one string for every host, or a per-host map
/// keyed by hostname.
///
/// [`PerHost`](Command::PerHost) lets a caller target a subset (e.g. a rollback of
/// only hosts with a recorded previous version).
#[derive(Debug, Clone)]
pub enum Command {
    /// The same command string is run on every (non-skipped) host.
    All(String),
    /// A per-host command map; hosts absent from the map are skipped.
    PerHost(BTreeMap<String, String>),
}

impl Command {
    /// Resolves the command for `hostname`, or `None` when a
    /// [`PerHost`](Command::PerHost) map does not cover it.
    fn for_host(&self, hostname: &str) -> Option<&str> {
        match self {
            Command::All(cmd) => Some(cmd.as_str()),
            Command::PerHost(map) => map.get(hostname).map(String::as_str),
        }
    }
}

impl From<&str> for Command {
    fn from(s: &str) -> Self {
        Command::All(s.to_owned())
    }
}

impl From<String> for Command {
    fn from(s: String) -> Self {
        Command::All(s)
    }
}

impl From<BTreeMap<String, String>> for Command {
    fn from(m: BTreeMap<String, String>) -> Self {
        Command::PerHost(m)
    }
}

/// Runs a [`Command`] across a group of targets in parallel.
///
/// When a [`PerHost`](Command::PerHost) map
/// is given, hosts it does not cover are skipped entirely.
pub(crate) async fn run_command(
    targets: &mut BTreeMap<String, Target>,
    command: impl Into<Command>,
    is_repl: bool,
    max_parallel: usize,
) {
    let command = command.into();
    run_fanout(
        targets,
        is_repl,
        max_parallel,
        Some("run"),
        |t: &Target| command.for_host(t.hostname()).is_some(),
        |t: &mut Target| {
            let cmd = command
                .for_host(t.hostname())
                .expect("host was filtered to be covered")
                .to_owned();
            boxed(async move { t.run(&cmd).await })
        },
    )
    .await;
}

/// Uploads `local` to `remote` on every target in parallel.
///
/// Errors are swallowed and logged by
/// [`Target::sftp_put`]; this helper never fails.
pub(crate) async fn sftp_put_all(
    targets: &mut BTreeMap<String, Target>,
    local: &Path,
    remote: &Path,
    is_repl: bool,
    max_parallel: usize,
) {
    let remote = remote.to_path_buf();

    // Read a size-bounded payload once and share the bytes across hosts (one disk
    // read, not N). A larger or unreadable file falls back to per-host streaming,
    // which records each host's own failure for the `put` aggregation.
    let shared: Option<Arc<[u8]>> = match tokio::fs::metadata(local).await {
        Ok(meta) if meta.len() <= SHARED_UPLOAD_CAP => tokio::fs::read(local)
            .await
            .ok()
            .map(|bytes| Arc::from(bytes.into_boxed_slice())),
        _ => None,
    };

    match shared {
        Some(bytes) => {
            run_fanout(
                targets,
                is_repl,
                max_parallel,
                Some("FileUpload"),
                |_| true,
                |t| {
                    let (bytes, remote) = (Arc::clone(&bytes), remote.clone());
                    // The outcome is recorded in `last_upload` for the REPL `put`
                    // aggregation; the returned copy is for map-returning callers.
                    boxed(async move {
                        let _ = t.sftp_put_bytes(&bytes, &remote).await;
                    })
                },
            )
            .await;
        }
        None => {
            let local = local.to_path_buf();
            run_fanout(
                targets,
                is_repl,
                max_parallel,
                Some("FileUpload"),
                |_| true,
                |t| {
                    let (local, remote) = (local.clone(), remote.clone());
                    boxed(async move { t.sftp_put(&local, &remote).await })
                },
            )
            .await;
        }
    }
}

/// Downloads `remote` into `local` (per-host suffixed) from every target in
/// parallel.
///
/// Errors are swallowed and logged
/// by [`Target::sftp_get`]; this helper never fails.
pub(crate) async fn sftp_get_all(
    targets: &mut BTreeMap<String, Target>,
    remote: &str,
    local: &Path,
    is_repl: bool,
    max_parallel: usize,
) {
    run_fanout(
        targets,
        is_repl,
        max_parallel,
        Some("FileDownload"),
        |_| true,
        |t| {
            let (remote, local) = (remote.to_owned(), local.to_path_buf());
            boxed(async move { t.sftp_get(&remote, &local).await })
        },
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::{MockConnection, MockSftpOp};

    /// Builds an enabled target wired to `conn`.
    fn target(hostname: &str, conn: MockConnection) -> Target {
        Target::with_connection(hostname, TargetState::Enabled, Box::new(conn))
    }

    /// A mock that echoes `stdout` for any command.
    fn echo(hostname: &str, stdout: &str) -> MockConnection {
        MockConnection::new(hostname).with_default(CommandLog::new("", stdout, "", 0, 0))
    }

    fn group(targets: Vec<Target>) -> BTreeMap<String, Target> {
        targets
            .into_iter()
            .map(|t| (t.hostname().to_owned(), t))
            .collect()
    }

    // --- run_parallel -------------------------------------------------------

    #[tokio::test]
    async fn run_parallel_empty_is_noop() {
        // An empty future set returns immediately without panicking.
        let futs: Vec<std::future::Ready<()>> = Vec::new();
        run_parallel(futs, Some("desc"), 0).await;
    }

    #[tokio::test]
    async fn run_parallel_drives_every_future() {
        let counter = Arc::new(AtomicUsize::new(0));
        let futs: Vec<_> = (0..5)
            .map(|_| {
                let c = Arc::clone(&counter);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            })
            .collect();
        run_parallel(futs, None, 0).await;
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    /// Peak in-flight concurrency never exceeds the configured bound, and every
    /// future still runs. Each future increments a live counter, records the
    /// running peak, yields so siblings can enter, then decrements — so with an
    /// unbounded scheduler the peak would reach N.
    #[tokio::test]
    async fn run_parallel_caps_peak_concurrency() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        let n = 50usize;
        let bound = 4usize;
        let futs: Vec<_> = (0..n)
            .map(|_| {
                let (live, peak, ran) = (Arc::clone(&live), Arc::clone(&peak), Arc::clone(&ran));
                async move {
                    let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);
                    // Yield repeatedly so all admitted futures overlap before any
                    // completes — maximising the observed peak for the assert.
                    for _ in 0..8 {
                        tokio::task::yield_now().await;
                    }
                    ran.fetch_add(1, Ordering::SeqCst);
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            })
            .collect();
        run_parallel(futs, None, bound).await;
        assert_eq!(ran.load(Ordering::SeqCst), n, "every future must run");
        assert!(
            peak.load(Ordering::SeqCst) <= bound,
            "peak {} exceeded bound {bound}",
            peak.load(Ordering::SeqCst)
        );
    }

    /// A zero bound falls back to the conservative default rather than stalling.
    #[tokio::test]
    async fn run_parallel_zero_bound_uses_default_and_runs_all() {
        let ran = Arc::new(AtomicUsize::new(0));
        let futs: Vec<_> = (0..DEFAULT_MAX_PARALLEL + 10)
            .map(|_| {
                let ran = Arc::clone(&ran);
                async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                }
            })
            .collect();
        run_parallel(futs, None, 0).await;
        assert_eq!(ran.load(Ordering::SeqCst), DEFAULT_MAX_PARALLEL + 10);
    }

    /// Dropping the fan-out future mid-flight cancels the un-admitted and
    /// in-flight work: futures beyond the bound never start, and the whole batch
    /// does not complete. Proven by counting how many futures *started* vs `n`.
    #[tokio::test]
    async fn run_parallel_cancellation_stops_pending_work() {
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let n = 40usize;
        let bound = 4usize;
        let futs: Vec<_> = (0..n)
            .map(|_| {
                let (started, completed) = (Arc::clone(&started), Arc::clone(&completed));
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    // Never resolves within the test window.
                    futures::future::pending::<()>().await;
                    completed.fetch_add(1, Ordering::SeqCst);
                }
            })
            .collect();
        // Drive the fan-out only briefly, then drop it (cancel).
        let fut = run_parallel(futs, None, bound);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(20), fut).await;
        // At most `bound` futures were ever admitted; none completed (all pend).
        assert!(
            started.load(Ordering::SeqCst) <= bound,
            "more than the bound ({bound}) futures started: {}",
            started.load(Ordering::SeqCst)
        );
        assert_eq!(completed.load(Ordering::SeqCst), 0, "none should complete");
    }

    // --- Command resolution -------------------------------------------------

    #[test]
    fn command_all_covers_every_host() {
        let cmd = Command::from("uptime");
        assert_eq!(cmd.for_host("anything"), Some("uptime"));
    }

    #[test]
    fn command_per_host_skips_uncovered() {
        let mut map = BTreeMap::new();
        map.insert("h1".to_owned(), "cmd1".to_owned());
        let cmd = Command::from(map);
        assert_eq!(cmd.for_host("h1"), Some("cmd1"));
        assert_eq!(cmd.for_host("h2"), None);
    }

    // --- RunCommand ---------------------------------------------------------

    #[tokio::test]
    async fn run_command_string_dispatches_to_all_hosts() {
        let (m1, m2) = (echo("h1", "a"), echo("h2", "b"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2)]);

        run_command(&mut g, "uptime", false, 0).await;

        assert_eq!(h1.commands(), vec!["uptime".to_owned()]);
        assert_eq!(h2.commands(), vec!["uptime".to_owned()]);
    }

    #[tokio::test]
    async fn run_command_per_host_only_touches_covered_hosts() {
        // A subset command must not KeyError/panic on the uncovered host.
        let (m1, m2) = (echo("h1", "a"), echo("h2", "b"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2)]);

        let mut map = BTreeMap::new();
        map.insert("h1".to_owned(), "only-h1".to_owned());
        run_command(&mut g, map, false, 0).await;

        assert_eq!(h1.commands(), vec!["only-h1".to_owned()]);
        assert!(h2.commands().is_empty(), "uncovered host must be skipped");
    }

    // --- SFTP fan-out -------------------------------------------------------

    #[tokio::test]
    async fn sftp_put_all_uploads_to_every_host() {
        let (m1, m2) = (echo("h1", ""), echo("h2", ""));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2)]);

        sftp_put_all(
            &mut g,
            Path::new("/local/f"),
            Path::new("/remote/f"),
            false,
            0,
        )
        .await;

        for h in [&h1, &h2] {
            assert!(matches!(
                h.sftp_ops().as_slice(),
                [MockSftpOp::Put { local, remote }]
                    if local == Path::new("/local/f") && remote == Path::new("/remote/f")
            ));
        }
    }

    #[tokio::test]
    async fn sftp_put_all_reads_payload_once_and_shares_bytes() {
        // A real, size-bounded file takes the read-once shared path: every host
        // receives a `PutBytes` of the same length, and the file is read from
        // disk a single time regardless of fleet size.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("payload");
        std::fs::write(&local, b"hello world").unwrap();

        let (m1, m2, m3) = (echo("h1", ""), echo("h2", ""), echo("h3", ""));
        let (h1, h2, h3) = (m1.clone(), m2.clone(), m3.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2), target("h3", m3)]);

        sftp_put_all(&mut g, &local, Path::new("/remote/f"), false, 0).await;

        for h in [&h1, &h2, &h3] {
            assert!(
                matches!(
                    h.sftp_ops().as_slice(),
                    [MockSftpOp::PutBytes { len, data: _, remote }]
                        if *len == b"hello world".len() && remote == Path::new("/remote/f")
                ),
                "each host must receive the shared payload via PutBytes, got {:?}",
                h.sftp_ops()
            );
        }
    }

    #[tokio::test]
    async fn sftp_put_all_streams_per_host_when_over_cap() {
        // A file larger than the shared cap falls back to per-host streaming
        // (`sftp_put(path)` → `Put`), keeping memory bounded for huge uploads.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("big");
        let big = vec![0u8; (SHARED_UPLOAD_CAP as usize) + 1];
        std::fs::write(&local, &big).unwrap();

        let (m1, m2) = (echo("h1", ""), echo("h2", ""));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2)]);

        sftp_put_all(&mut g, &local, Path::new("/remote/f"), false, 0).await;

        for h in [&h1, &h2] {
            assert!(
                matches!(h.sftp_ops().as_slice(), [MockSftpOp::Put { .. }]),
                "over-cap upload must stream per-host, got {:?}",
                h.sftp_ops()
            );
        }
    }

    #[tokio::test]
    async fn sftp_put_all_missing_payload_uploads_to_no_host() {
        // A payload that can't be stat'd/read must not partially dispatch:
        // the fan-out aborts before any host op. (Read-once error handling.)
        let (m1, m2) = (echo("h1", ""), echo("h2", ""));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2)]);

        // Nonexistent path: metadata() → None branch → streaming `sftp_put`,
        // whose per-host read fails and is swallowed. No PutBytes is dispatched.
        sftp_put_all(
            &mut g,
            Path::new("/does/not/exist"),
            Path::new("/r"),
            false,
            0,
        )
        .await;

        for h in [&h1, &h2] {
            assert!(
                !h.sftp_ops()
                    .iter()
                    .any(|op| matches!(op, MockSftpOp::PutBytes { .. })),
                "missing payload must never dispatch shared bytes, got {:?}",
                h.sftp_ops()
            );
        }
    }

    #[tokio::test]
    async fn sftp_put_all_unreadable_payload_still_records_every_host() {
        // A directory stats fine but fails to read: every host must still be
        // attempted, so `put` sees a per-host outcome instead of nothing.
        let dir = tempfile::tempdir().unwrap();
        let (m1, m2) = (echo("h1", ""), echo("h2", ""));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![target("h1", m1), target("h2", m2)]);

        sftp_put_all(&mut g, dir.path(), Path::new("/r"), false, 0).await;

        for h in [&h1, &h2] {
            assert!(
                matches!(h.sftp_ops().as_slice(), [MockSftpOp::Put { .. }]),
                "got {:?}",
                h.sftp_ops()
            );
        }
        assert!(g.values().all(|t| t.last_upload().is_some()));
    }

    #[tokio::test]
    async fn sftp_get_all_downloads_with_per_host_suffix() {
        let m1 = echo("h1", "");
        let h1 = m1.clone();
        let mut g = group(vec![target("h1", m1)]);

        sftp_get_all(&mut g, "/remote/f", Path::new("/local/f"), false, 0).await;

        // Target::sftp_get appends `.{hostname}` to a non-folder local path.
        assert!(matches!(
            h1.sftp_ops().as_slice(),
            [MockSftpOp::Get { remote, local }]
                if remote == Path::new("/remote/f") && local == Path::new("/local/f.h1")
        ));
    }
}
