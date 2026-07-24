//! Parallel fan-out primitives over a group of [`Target`]s.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/actions.py`. Upstream models each
//! fan-out action as a `ThreadedTargetGroup` subclass that builds
//! `(callable, args)` pairs and submits them to a thread pool via
//! `run_parallel`, plus a `RunCommand` class that dispatches a command to
//! every host.
//!
//! This port keeps the same behavioural surface but is async and idiomatic:
//!
//! * [`run_parallel`] drives a set of caller-supplied futures to completion
//!   concurrently (the async replacement for the thread pool).
//! * [`RunCommand`] dispatches a command (one string for all hosts, or a
//!   per-host map) to every host in parallel.
//! * [`sftp_put_all`] / [`sftp_get_all`] are the async equivalents of
//!   upstream's `FileUpload` / `FileDownload`.
//!
//! ### Why no output lock
//!
//! Upstream passes a `threading.Lock` into `Target.run(cmd, lock)` to serialize
//! writes to a shared per-worker output stream. In this port each [`Target`]
//! owns its own [`HostLog`](mtui_types::hostlog::HostLog) behind `&mut self`, so
//! concurrent tasks hold **disjoint** mutable borrows — there is no shared
//! output to guard and the lock is unnecessary.
//!
//! ### The TTY spinner
//!
//! Upstream's `run_parallel` optionally drives a `|/-\` TTY spinner labelled
//! with a `desc`. [`run_parallel`] starts a [`TtySpinner`](super::spinner) for
//! the duration of the fan-out when a `desc` is given; it is a strict no-op off
//! a TTY (tests, redirected output, `mtui-mcp`), so behaviour with `desc=None`
//! (or off a terminal) is identical to the plain `join_all`. The spinner erases
//! its frame on stop, and the serialised prompter's `suspend` guard erases it
//! during an interactive read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::{self, StreamExt};

use super::Target;

/// Fallback fan-out width when a caller passes `0` (unconfigured). Mirrors the
/// `[connection] max_parallel` default in `mtui-config`; kept as a local
/// constant so `mtui-hosts` needs no dependency on `mtui-config`.
const DEFAULT_MAX_PARALLEL: usize = 50;

/// Upper bound (bytes) for reading an upload payload once into a shared buffer.
///
/// At or below this size the fan-out reads `local` a single time and dispatches
/// the shared `Arc<[u8]>` to every host, so peak RSS is ~O(payload) rather than
/// O(payload × hosts). Above it, each host streams the file from disk itself
/// (per-host re-read) to keep memory bounded for very large uploads. mtui
/// uploads are small test scripts, so the shared path is the common case.
const SHARED_UPLOAD_CAP: u64 = 8 * 1024 * 1024;

/// Drives every future in `futures` to completion concurrently.
///
/// The async replacement for upstream's `run_parallel` thread pool. An empty
/// input returns immediately. When `desc` is `Some`, a labelled
/// [`TtySpinner`](super::spinner::TtySpinner) paints for the duration of the
/// fan-out (a no-op off a TTY, so tests and `mtui-mcp` stay clean) and is
/// stopped — erasing its frame — when the batch completes.
///
/// Unlike upstream — whose first worker exception re-raises and cancels the rest
/// — the per-target futures built by [`RunCommand`] and the `sftp_*_all`
/// helpers never fail: [`Target::run`] and the SFTP methods swallow and log
/// their own errors (the upstream `-1`-sentinel / log-not-propagate contract),
/// so one bad host can never abort the fan-out. `run_parallel` therefore has no
/// error to propagate and returns `()`.
///
/// `max_parallel` bounds how many futures are polled at once (peak
/// sockets/tasks/RSS and remote load); `0` falls back to
/// [`DEFAULT_MAX_PARALLEL`]. Completion order is irrelevant to every caller (the
/// group is a sorted `BTreeMap` and per-host results attach to disjoint `&mut
/// Target` borrows), so a bounded, out-of-order scheduler
/// (`buffer_unordered`) is observably equivalent to the previous unbounded
/// `join_all`.
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
    // Drive a labelled spinner for the batch; `TtySpinner` is a no-op off a TTY,
    // so this stays silent in tests / MCP. Dropping it (or the explicit `stop`)
    // erases the frame.
    let mut spinner = desc.map(|d| {
        let mut s = super::spinner::TtySpinner::new(d);
        s.start();
        // Report the resolved paint state so `mtui -d` reveals *why* a spinner is
        // (in)visible: `enabled=false` means stderr was not detected as a TTY
        // (piped / tmux-detached / IDE terminal), the usual cause of a missing
        // spinner even on an interactive-looking session.
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
/// [`run_fanout`]'s `op` returns this so the "future borrows the `&mut Target`
/// it was handed" relationship is expressible on stable Rust: a bare
/// `Fn(&mut Target) -> impl Future` bound cannot tie the returned future's
/// lifetime to the borrow (it would have to outlive it), and the `AsyncFn` /
/// `CallRefFuture` machinery that could is still unstable. Boxing the future
/// carries its own `'a` lifetime, so callers just wrap their async block in
/// [`Box::pin`]. The per-call allocation is negligible next to the SSH round
/// trip each op performs.
pub(crate) type BoxTargetFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

/// Drives a per-target async `op` across a group in parallel.
///
/// This is the single fan-out primitive every per-host I/O method on
/// [`HostsGroup`](super::HostsGroup) routes through. Every (non-skipped)
/// target is driven concurrently via [`run_parallel`].
///
/// `desc` labels the optional TTY spinner (a no-op off a TTY). `op` is invoked
/// once per (non-skipped) target with `&mut Target`; the disjoint mutable
/// borrows make the parallel batch sound without a shared lock.
///
/// `should_run` lets a caller skip a target entirely (e.g. a
/// [`Command::PerHost`] map that does not cover it); return `false` to omit it.
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

    // Diagnostic (`mtui -d`): report the resolved fan-out shape so a missing
    // spinner is explainable. `is_repl=false` or `parallel=0` both suppress the
    // fan-out spinner even on a TTY / with `MTUI_FORCE_SPINNER`.
    tracing::debug!(
        is_repl,
        desc = desc.unwrap_or("<none>"),
        parallel = parallel.len(),
        "fan-out dispatch"
    );

    // Each future borrows a distinct target mutably, so the borrows are
    // disjoint and need no shared lock.
    let parallel_futs: Vec<_> = parallel.into_iter().map(&op).collect();
    run_parallel(
        parallel_futs,
        if is_repl { desc } else { None },
        max_parallel,
    )
    .await;
}

/// A command to run across a group: one string for every host, or a per-host
/// map keyed by hostname.
///
/// Mirrors upstream `RunCommand`'s `command: str | dict[str, Any]`. The
/// [`PerHost`](Command::PerHost) form lets a caller build a command for only a
/// subset of the group (e.g. a rollback that targets only hosts with a recorded
/// previous version); hosts not present in the map are simply skipped, matching
/// upstream's dict-subset filter.
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
/// Ported from upstream `RunCommand`. When a [`PerHost`](Command::PerHost) map
/// is given, hosts it does not cover are skipped entirely.
pub(crate) struct RunCommand<'a> {
    targets: &'a mut BTreeMap<String, Target>,
    command: Command,
    is_repl: bool,
    max_parallel: usize,
}

impl<'a> RunCommand<'a> {
    /// Builds a run over `targets` with `command`.
    ///
    /// The parallel-batch width defaults to unconfigured (`0` →
    /// [`DEFAULT_MAX_PARALLEL`]); set it with
    /// [`with_max_parallel`](Self::with_max_parallel).
    #[must_use]
    pub(crate) fn new(
        targets: &'a mut BTreeMap<String, Target>,
        command: impl Into<Command>,
        is_repl: bool,
    ) -> Self {
        Self {
            targets,
            command: command.into(),
            is_repl,
            max_parallel: 0,
        }
    }

    /// Sets the parallel-batch concurrency bound (builder-style). `0` falls back
    /// to [`DEFAULT_MAX_PARALLEL`].
    #[must_use]
    pub(crate) fn with_max_parallel(mut self, max_parallel: usize) -> Self {
        self.max_parallel = max_parallel;
        self
    }

    /// Executes the command across every (non-skipped) host in parallel.
    ///
    /// Delegates to the shared [`run_fanout`] primitive; the only
    /// command-specific logic here is resolving the per-host command string
    /// and skipping hosts a [`PerHost`](Command::PerHost) map does not cover.
    pub(crate) async fn run(self) {
        let Self {
            targets,
            command,
            is_repl,
            max_parallel,
        } = self;

        run_fanout(
            targets,
            is_repl,
            max_parallel,
            Some("run"),
            // `for_host` returning None means "skip" (upstream dict subset).
            |t: &Target| command.for_host(t.hostname()).is_some(),
            |t: &mut Target| {
                // Resolve before the async block so `command` isn't borrowed
                // across it; the host was filtered to be covered above.
                let cmd = command
                    .for_host(t.hostname())
                    .expect("host was filtered to be covered")
                    .to_owned();
                Box::pin(async move { t.run(&cmd).await }) as BoxTargetFut<'_>
            },
        )
        .await;
    }
}

/// Uploads `local` to `remote` on every target in parallel.
///
/// Async equivalent of upstream `FileUpload`. Errors are swallowed and logged by
/// [`Target::sftp_put`]; this helper never fails.
pub(crate) async fn sftp_put_all(
    targets: &mut BTreeMap<String, Target>,
    local: &Path,
    remote: &Path,
    is_repl: bool,
    max_parallel: usize,
) {
    let desc = if is_repl { Some("FileUpload") } else { None };
    let remote = remote.to_path_buf();

    // Read the immutable payload once when it is size-bounded, and dispatch the
    // shared bytes to every host — a fan-out of N hosts then performs one disk
    // read, not N. A file larger than the cap (or one whose size can't be
    // stat'd) falls back to per-host streaming from disk.
    let shared: Option<Arc<[u8]>> = match tokio::fs::metadata(local).await {
        Ok(meta) if meta.len() <= SHARED_UPLOAD_CAP => match tokio::fs::read(local).await {
            Ok(bytes) => Some(Arc::from(bytes.into_boxed_slice())),
            Err(e) => {
                tracing::error!(local = %local.display(), error = %e, "failed to read upload payload");
                return;
            }
        },
        _ => None,
    };

    match shared {
        Some(bytes) => {
            let futs = targets.values_mut().map(|t| {
                let (bytes, remote) = (Arc::clone(&bytes), remote.clone());
                async move { t.sftp_put_bytes(&bytes, &remote).await }
            });
            run_parallel(futs, desc, max_parallel).await;
        }
        None => {
            let local = local.to_path_buf();
            let futs = targets.values_mut().map(|t| {
                let (local, remote) = (local.clone(), remote.clone());
                async move { t.sftp_put(&local, &remote).await }
            });
            run_parallel(futs, desc, max_parallel).await;
        }
    }
}

/// Downloads `remote` into `local` (per-host suffixed) from every target in
/// parallel.
///
/// Async equivalent of upstream `FileDownload`. Errors are swallowed and logged
/// by [`Target::sftp_get`]; this helper never fails.
pub(crate) async fn sftp_get_all(
    targets: &mut BTreeMap<String, Target>,
    remote: &str,
    local: &Path,
    is_repl: bool,
    max_parallel: usize,
) {
    let desc = if is_repl { Some("FileDownload") } else { None };
    let remote = remote.to_owned();
    let local: PathBuf = local.to_path_buf();
    let futs = targets.values_mut().map(|t| {
        let (remote, local) = (remote.clone(), local.clone());
        async move { t.sftp_get(&remote, &local).await }
    });
    run_parallel(futs, desc, max_parallel).await;
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

        RunCommand::new(&mut g, "uptime", false).run().await;

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
        RunCommand::new(&mut g, map, false).run().await;

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
                    [MockSftpOp::PutBytes { len, remote }]
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
