//! Parallel / serial fan-out primitives over a group of [`Target`]s.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/actions.py`. Upstream models each
//! fan-out action as a `ThreadedTargetGroup` subclass that builds
//! `(callable, args)` pairs and submits them to a thread pool via
//! `run_parallel`, plus a `RunCommand` class that splits hosts into a parallel
//! pool and a serial (one-at-a-time) barrier.
//!
//! This port keeps the same behavioural surface but is async and idiomatic:
//!
//! * [`run_parallel`] drives a set of caller-supplied futures to completion
//!   concurrently (the async replacement for the thread pool).
//! * [`RunCommand`] partitions the group into parallel and serial hosts by
//!   [`ExecutionMode`] and dispatches a command (one string for all hosts, or a
//!   per-host map) to each.
//! * [`sftp_put_all`] / [`sftp_get_all`] / [`sftp_remove_all`] are the async
//!   equivalents of upstream's `FileUpload` / `FileDownload` / `FileDelete`.
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

use futures::future::join_all;
use mtui_types::enums::ExecutionMode;

use super::Target;
use crate::prompter::Prompter;

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
pub async fn run_parallel<I, F>(futures: I, desc: Option<&str>)
where
    I: IntoIterator<Item = F>,
    F: std::future::Future<Output = ()>,
{
    let futs: Vec<F> = futures.into_iter().collect();
    if futs.is_empty() {
        return;
    }
    // Drive a labelled spinner for the batch; `TtySpinner` is a no-op off a TTY,
    // so this stays silent in tests / MCP. Dropping it (or the explicit `stop`)
    // erases the frame.
    let mut spinner = desc.map(|d| {
        tracing::debug!(action = d, count = futs.len(), "running in parallel");
        let mut s = super::spinner::TtySpinner::new(d);
        s.start();
        s
    });
    join_all(futs).await;
    if let Some(mut s) = spinner.take() {
        s.stop();
    }
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

/// Runs a [`Command`] across a group of targets: parallel hosts concurrently,
/// then serial hosts one at a time.
///
/// Ported from upstream `RunCommand`. Hosts whose [`ExecutionMode`] is
/// [`Serial`](ExecutionMode::Serial) run *after* the parallel batch, sequentially
/// and in (sorted) hostname order, mirroring upstream's serial barrier. When a
/// [`PerHost`](Command::PerHost) map is given, hosts it does not cover are
/// skipped entirely.
///
/// The upstream serial barrier prompts the user (`press Enter key to proceed
/// with <host>`) before each serial host. When a serialised [`Prompter`] is
/// supplied and `interactive` is set (the REPL), [`run`](RunCommand::run) asks
/// that prompt before each serial host; headless callers (`mtui-mcp`) pass
/// `None` and serial hosts run back-to-back. No stdin/TTY code lives in this
/// crate — the reader is injected via the [`Prompter`].
pub struct RunCommand<'a> {
    targets: &'a mut BTreeMap<String, Target>,
    command: Command,
    interactive: bool,
    prompter: Option<Prompter>,
}

impl<'a> RunCommand<'a> {
    /// Builds a run over `targets` with `command`.
    ///
    /// `prompter` is the session-level serialised [`Prompter`] (or `None` when
    /// headless); when present and `interactive` is set, the serial barrier
    /// prompts before each serial host.
    #[must_use]
    pub fn new(
        targets: &'a mut BTreeMap<String, Target>,
        command: impl Into<Command>,
        interactive: bool,
        prompter: Option<Prompter>,
    ) -> Self {
        Self {
            targets,
            command: command.into(),
            interactive,
            prompter,
        }
    }

    /// Executes the command: parallel hosts concurrently, then serial hosts
    /// sequentially.
    pub async fn run(self) {
        let Self {
            targets,
            command,
            interactive,
            prompter,
        } = self;

        // Split into parallel and serial, skipping hosts a PerHost map does not
        // cover. `for_host` returning None means "skip" (upstream dict subset).
        let mut parallel: Vec<&mut Target> = Vec::new();
        let mut serial: Vec<&mut Target> = Vec::new();
        for target in targets.values_mut() {
            if command.for_host(target.hostname()).is_none() {
                continue;
            }
            if target.mode() == ExecutionMode::Serial {
                serial.push(target);
            } else {
                parallel.push(target);
            }
        }

        // Parallel batch. Each future borrows a distinct target mutably, so the
        // borrows are disjoint and need no shared lock.
        let desc = if interactive { Some("run") } else { None };
        let parallel_futs = parallel.into_iter().map(|t| {
            // Resolve before the async move so `command` isn't borrowed across it.
            let cmd = command
                .for_host(t.hostname())
                .expect("host was filtered to be covered")
                .to_owned();
            async move { t.run(&cmd).await }
        });
        run_parallel(parallel_futs, desc).await;

        // Serial barrier: one host at a time. Under an interactive session with
        // a serialised prompter, ask the user to press Enter before each serial
        // host (upstream `prompt_user("press Enter key to proceed with …")`);
        // headless callers (no prompter) run them back-to-back. The answer is
        // discarded — any input (incl. empty) proceeds, matching upstream.
        for t in serial {
            let cmd = command
                .for_host(t.hostname())
                .expect("host was filtered to be covered")
                .to_owned();
            if interactive && let Some(prompter) = prompter.as_ref() {
                let text = format!("press Enter key to proceed with {} ", t.hostname());
                let _ = prompter.ask(&text).await;
            }
            t.run(&cmd).await;
        }
    }
}

/// Uploads `local` to `remote` on every target in parallel.
///
/// Async equivalent of upstream `FileUpload`. Errors are swallowed and logged by
/// [`Target::sftp_put`]; this helper never fails.
pub async fn sftp_put_all(
    targets: &mut BTreeMap<String, Target>,
    local: &Path,
    remote: &Path,
    interactive: bool,
) {
    let desc = if interactive {
        Some("FileUpload")
    } else {
        None
    };
    let local = local.to_path_buf();
    let remote = remote.to_path_buf();
    let futs = targets.values_mut().map(|t| {
        let (local, remote) = (local.clone(), remote.clone());
        async move { t.sftp_put(&local, &remote).await }
    });
    run_parallel(futs, desc).await;
}

/// Downloads `remote` into `local` (per-host suffixed) from every target in
/// parallel.
///
/// Async equivalent of upstream `FileDownload`. Errors are swallowed and logged
/// by [`Target::sftp_get`]; this helper never fails.
pub async fn sftp_get_all(
    targets: &mut BTreeMap<String, Target>,
    remote: &str,
    local: &Path,
    interactive: bool,
) {
    let desc = if interactive {
        Some("FileDownload")
    } else {
        None
    };
    let remote = remote.to_owned();
    let local: PathBuf = local.to_path_buf();
    let futs = targets.values_mut().map(|t| {
        let (remote, local) = (remote.clone(), local.clone());
        async move { t.sftp_get(&remote, &local).await }
    });
    run_parallel(futs, desc).await;
}

/// Deletes `path` on every target in parallel.
///
/// Async equivalent of upstream `FileDelete`. Errors are swallowed and logged by
/// [`Target::sftp_remove`]; this helper never fails.
pub async fn sftp_remove_all(
    targets: &mut BTreeMap<String, Target>,
    path: &Path,
    interactive: bool,
) {
    let desc = if interactive {
        Some("FileDelete")
    } else {
        None
    };
    let path = path.to_path_buf();
    let futs = targets.values_mut().map(|t| {
        let path = path.clone();
        async move { t.sftp_remove(&path).await }
    });
    run_parallel(futs, desc).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    use super::*;
    use crate::connection::{MockConnection, MockSftpOp};

    /// Builds an enabled parallel target wired to `conn`.
    fn target(hostname: &str, mode: ExecutionMode, conn: MockConnection) -> Target {
        Target::with_connection(hostname, TargetState::Enabled, mode, Box::new(conn))
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
        run_parallel(futs, Some("desc")).await;
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
        run_parallel(futs, None).await;
        assert_eq!(counter.load(Ordering::SeqCst), 5);
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
        let mut g = group(vec![
            target("h1", ExecutionMode::Parallel, m1),
            target("h2", ExecutionMode::Parallel, m2),
        ]);

        RunCommand::new(&mut g, "uptime", false, None).run().await;

        assert_eq!(h1.commands(), vec!["uptime".to_owned()]);
        assert_eq!(h2.commands(), vec!["uptime".to_owned()]);
    }

    #[tokio::test]
    async fn run_command_per_host_only_touches_covered_hosts() {
        // A subset command must not KeyError/panic on the uncovered host.
        let (m1, m2) = (echo("h1", "a"), echo("h2", "b"));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![
            target("h1", ExecutionMode::Parallel, m1),
            target("h2", ExecutionMode::Parallel, m2),
        ]);

        let mut map = BTreeMap::new();
        map.insert("h1".to_owned(), "only-h1".to_owned());
        RunCommand::new(&mut g, map, false, None).run().await;

        assert_eq!(h1.commands(), vec!["only-h1".to_owned()]);
        assert!(h2.commands().is_empty(), "uncovered host must be skipped");
    }

    #[tokio::test]
    async fn run_command_runs_parallel_and_serial_hosts() {
        // Both a parallel and a serial host receive the command; the serial
        // barrier runs after the parallel batch (no prompter here, so no prompt).
        let (mp, ms) = (echo("par", "p"), echo("ser", "s"));
        let (hp, hs) = (mp.clone(), ms.clone());
        let mut g = group(vec![
            target("par", ExecutionMode::Parallel, mp),
            target("ser", ExecutionMode::Serial, ms),
        ]);

        RunCommand::new(&mut g, "cmd", true, None).run().await;

        assert_eq!(hp.commands(), vec!["cmd".to_owned()]);
        assert_eq!(hs.commands(), vec!["cmd".to_owned()]);
    }

    /// A recording [`Prompter`] that appends every prompt text to a shared vec
    /// and returns an empty answer (Enter). Used to assert the serial-barrier
    /// prompt is reached (interactive) or skipped (headless).
    fn recording_prompter(seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Prompter {
        Prompter::new(std::sync::Arc::new(move |text: String| {
            let seen = std::sync::Arc::clone(&seen);
            Box::pin(async move {
                seen.lock().unwrap().push(text);
                Ok(String::new())
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        }))
    }

    #[tokio::test]
    async fn serial_barrier_prompts_before_each_serial_host_when_interactive() {
        let _serial = super::super::spinner::TEST_SERIAL.lock().await;
        let (ma, mb) = (echo("s-a", "a"), echo("s-b", "b"));
        let (ha, hb) = (ma.clone(), mb.clone());
        let mut g = group(vec![
            target("s-a", ExecutionMode::Serial, ma),
            target("s-b", ExecutionMode::Serial, mb),
        ]);

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prompter = recording_prompter(std::sync::Arc::clone(&seen));

        RunCommand::new(&mut g, "cmd", true, Some(prompter))
            .run()
            .await;

        // Both serial hosts ran, prompted in sorted hostname order.
        assert_eq!(ha.commands(), vec!["cmd".to_owned()]);
        assert_eq!(hb.commands(), vec!["cmd".to_owned()]);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "press Enter key to proceed with s-a ".to_owned(),
                "press Enter key to proceed with s-b ".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn serial_barrier_does_not_prompt_when_headless() {
        let _serial = super::super::spinner::TEST_SERIAL.lock().await;
        let (ma, mb) = (echo("s-a", "a"), echo("s-b", "b"));
        let (ha, hb) = (ma.clone(), mb.clone());
        let mut g = group(vec![
            target("s-a", ExecutionMode::Serial, ma),
            target("s-b", ExecutionMode::Serial, mb),
        ]);

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prompter = recording_prompter(std::sync::Arc::clone(&seen));

        // Headless: interactive == false → no prompt even with a prompter set.
        RunCommand::new(&mut g, "cmd", false, Some(prompter))
            .run()
            .await;

        assert_eq!(ha.commands(), vec!["cmd".to_owned()]);
        assert_eq!(hb.commands(), vec!["cmd".to_owned()]);
        assert!(
            seen.lock().unwrap().is_empty(),
            "headless run must not prompt"
        );
    }

    // --- SFTP fan-out -------------------------------------------------------

    #[tokio::test]
    async fn sftp_put_all_uploads_to_every_host() {
        let (m1, m2) = (echo("h1", ""), echo("h2", ""));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![
            target("h1", ExecutionMode::Parallel, m1),
            target("h2", ExecutionMode::Parallel, m2),
        ]);

        sftp_put_all(&mut g, Path::new("/local/f"), Path::new("/remote/f"), false).await;

        for h in [&h1, &h2] {
            assert!(matches!(
                h.sftp_ops().as_slice(),
                [MockSftpOp::Put { local, remote }]
                    if local == Path::new("/local/f") && remote == Path::new("/remote/f")
            ));
        }
    }

    #[tokio::test]
    async fn sftp_get_all_downloads_with_per_host_suffix() {
        let m1 = echo("h1", "");
        let h1 = m1.clone();
        let mut g = group(vec![target("h1", ExecutionMode::Parallel, m1)]);

        sftp_get_all(&mut g, "/remote/f", Path::new("/local/f"), false).await;

        // Target::sftp_get appends `.{hostname}` to a non-folder local path.
        assert!(matches!(
            h1.sftp_ops().as_slice(),
            [MockSftpOp::Get { remote, local }]
                if remote == Path::new("/remote/f") && local == Path::new("/local/f.h1")
        ));
    }

    #[tokio::test]
    async fn sftp_remove_all_removes_on_every_host() {
        let (m1, m2) = (echo("h1", ""), echo("h2", ""));
        let (h1, h2) = (m1.clone(), m2.clone());
        let mut g = group(vec![
            target("h1", ExecutionMode::Parallel, m1),
            target("h2", ExecutionMode::Parallel, m2),
        ]);

        sftp_remove_all(&mut g, Path::new("/remote/f"), false).await;

        for h in [&h1, &h2] {
            assert!(matches!(
                h.sftp_ops().as_slice(),
                [MockSftpOp::Remove(p)] if p == Path::new("/remote/f")
            ));
        }
    }
}
