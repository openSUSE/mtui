//! A tiny TTY spinner for long-running interactive fan-outs.
//!
//! Ported from upstream `mtui/support/spinner.py`. Repaints a `|/-\` frame on
//! **stderr** while work is in flight, but only when stderr is a TTY. Off a TTY
//! (tests, redirected output, log files, the `mtui-mcp` transport) it is a
//! strict no-op, so test output and log files stay clean and the MCP layer can
//! surface progress through its own channel instead.
//!
//! Frame painting is serialised through a process-wide paint lock so other
//! writers on the same terminal can coordinate with a live spinner: wrap the
//! write in [`suspend`] to erase the current frame, keep the spinner from
//! repainting while the guard is held, and write from column 0. The
//! serialised interactive prompter ([`crate::prompter::Prompter`]) holds a
//! [`suspend`] guard for the whole read so a live spinner erases its frame and
//! stops repainting over the prompt until the user has answered.
//!
//! ## Async, not threads
//!
//! Upstream drives the spinner from an OS thread because its workers are
//! threads. mtui fans out with `tokio` tasks, so [`TtySpinner`] paints from a
//! spawned task on a `tokio::time` interval. The paint lock is a
//! [`std::sync::Mutex`] guarding only the short erase/paint critical sections
//! (never held across `.await`), so it is sound to acquire it from both the
//! paint task and an async [`suspend`] guard.
//!
//! ## Testability
//!
//! The frame sink is injectable (`TtySpinner::with_sink`). Unit tests capture
//! frames into an in-memory buffer instead of the real terminal, so the whole
//! module runs offline and deterministically. The default constructor paints to
//! stderr and is enabled only when `stderr` is a terminal.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::task::JoinHandle;

/// The animation frames, matching upstream's `|/-\`.
const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
/// Repaint interval, matching upstream's 0.1s.
const INTERVAL: Duration = Duration::from_millis(100);
/// Erase-current-line escape: carriage return + clear-to-end-of-line.
const ERASE: &str = "\r\x1b[K";

/// A spinner registered with the [`PaintCoord`]: its id, frame sink, label, and
/// a shared frame counter so a repaint (from the tick task *or* from a dropped
/// [`suspend`] guard) can render the current `|/-\` frame consistently.
struct Registered {
    id: u64,
    sink: Sink,
    desc: String,
    /// Advanced on every paint; `% 4` selects the animation frame.
    frame: Arc<AtomicU64>,
}

/// Process-wide paint coordinator: the lock every frame erase/paint takes, and
/// the registry of spinners currently painting, each carrying its own frame sink
/// so [`suspend`] can erase through the *same* sink the spinner paints to (in
/// production stderr; in tests an in-memory buffer) and immediately **repaint**
/// through it when the guard drops. When the registry is empty [`suspend`] is a
/// strict no-op — in particular off a TTY, where spinners never register.
struct PaintCoord {
    /// Serialises every erase/paint; also guards the active-spinner registry.
    lock: Mutex<Vec<Registered>>,
    /// When set, paint tasks skip repainting — an interactive read is in
    /// progress (see [`SuspendAsync`]). Checked under the paint lock so a read
    /// can never race a frame back onto the terminal it just cleared.
    paused: AtomicBool,
}

/// Renders the current frame of every registered spinner through its own sink.
///
/// Called under the paint lock both by the tick task and — the key to
/// perceptibility — immediately when a [`Suspend`] guard drops after a log line,
/// so the frame reappears on the fresh line at once instead of waiting up to a
/// full tick interval. Skips painting while a read holds the terminal
/// ([`SuspendAsync`]'s paused flag).
fn repaint_all(reg: &[Registered]) {
    if coord().paused.load(Ordering::SeqCst) {
        return;
    }
    for r in reg {
        let i = r.frame.load(Ordering::SeqCst) as usize;
        let frame = format!("\r[{}] {}", FRAMES[i % 4], r.desc);
        let mut w = r
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = w.write_all(frame.as_bytes());
        let _ = w.flush();
    }
}

/// Test-only serialiser: every test that registers a spinner into the
/// process-global coordinator takes this first, so leaked/aborted paint tasks
/// from one test cannot contend the shared state another test observes. A
/// `tokio` mutex so it is sound to hold `.await`-across in async tests.
#[cfg(test)]
pub(crate) static TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The single shared coordinator.
fn coord() -> &'static PaintCoord {
    static COORD: OnceLock<PaintCoord> = OnceLock::new();
    COORD.get_or_init(|| PaintCoord {
        lock: Mutex::new(Vec::new()),
        paused: AtomicBool::new(false),
    })
}

/// Monotonic id source so each spinner can deregister exactly its own sink.
fn next_id() -> u64 {
    static IDS: AtomicU64 = AtomicU64::new(0);
    IDS.fetch_add(1, Ordering::SeqCst)
}

/// A guard that pauses spinner painting for its lifetime and erases any visible
/// frame up front.
///
/// Holds the process-wide paint lock, so a live spinner cannot repaint while the
/// caller writes to the terminal. If a spinner is active, the current frame is
/// erased first so the caller's output starts on a clean line. The spinner
/// repaints on its next tick after the guard drops. A no-op (beyond taking the
/// lock) when no spinner is active.
///
/// The guard is not `Send`-held across `.await` by callers other than the
/// prompter, whose read is synchronous under the guard by construction; the
/// underlying [`std::sync::MutexGuard`] is intentionally scoped so it never
/// straddles an await point inside this module.
#[must_use = "the spinner stays suspended only while the guard is alive"]
pub struct Suspend {
    guard: std::sync::MutexGuard<'static, Vec<Registered>>,
}

impl Drop for Suspend {
    fn drop(&mut self) {
        // Immediately repaint the current frame(s) through their sinks, still
        // under the paint lock this guard holds, so the spinner reappears on the
        // caller's fresh line at once — not up to a full tick interval later.
        // This is what makes the spinner *perceptible* during a fan-out whose
        // hosts each emit a log line: without it, four near-simultaneous
        // `info:` lines erase every brief frame and the animation never shows.
        repaint_all(&self.guard);
    }
}

/// Pauses spinner painting and erases any visible frame for the returned guard's
/// lifetime, then repaints the frame the instant the guard drops.
///
/// A no-op (beyond taking the paint lock) when no spinner is active. Off a TTY
/// no spinner ever registers, so this is effectively free there. The erase — and
/// the on-drop repaint — are written through each active spinner's own sink so
/// they land on the same stream the frames were painted to.
pub fn suspend() -> Suspend {
    let guard = coord()
        .lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for r in guard.iter() {
        // Best-effort erase; a failed write must never poison the guard.
        let mut w = r
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = w.write_all(ERASE.as_bytes());
        let _ = w.flush();
    }
    Suspend { guard }
}

/// A `Send` guard that pauses spinner painting across an `.await` (e.g. an
/// interactive stdin read) without holding a non-`Send` [`std::sync::MutexGuard`].
///
/// [`suspend_async`] erases any active frame and sets a paused flag the paint
/// tasks honour; dropping this guard clears the flag so painting resumes on the
/// next tick. Unlike [`Suspend`], nothing lock-guard-shaped is held for the
/// guard's lifetime, so it is safe to keep alive across the await points of an
/// async reader inside a `Send` future (the connection `run` loop, the prompter).
#[must_use = "painting resumes as soon as the guard is dropped"]
pub struct SuspendAsync {
    _private: (),
}

/// Pauses spinner painting and erases any visible frame until the returned guard
/// drops — the `Send`, await-safe counterpart to [`suspend`].
pub fn suspend_async() -> SuspendAsync {
    // Erase under the paint lock, then flip the paused flag (still under the
    // lock) so no in-flight paint can repaint between the erase and the flag.
    let guard = coord()
        .lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    coord().paused.store(true, Ordering::SeqCst);
    for r in guard.iter() {
        let mut w = r
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = w.write_all(ERASE.as_bytes());
        let _ = w.flush();
    }
    drop(guard);
    SuspendAsync { _private: () }
}

impl Drop for SuspendAsync {
    fn drop(&mut self) {
        coord().paused.store(false, Ordering::SeqCst);
    }
}

/// A frame sink: where the spinner paints its `\r[{frame}] {desc}` line. The
/// real spinner writes to stderr; tests inject an in-memory buffer.
pub type Sink = Arc<Mutex<dyn Write + Send>>;

/// A `|/-\` spinner driven by one `tokio` task; a no-op off a TTY.
///
/// Safe to [`stop`](TtySpinner::stop) more than once. Dropping the handle also
/// stops the paint task.
pub struct TtySpinner {
    id: u64,
    desc: String,
    enabled: bool,
    sink: Sink,
    /// Shared animation counter, advanced by the paint task and read by any
    /// repaint (tick or on-`suspend`-drop) so the frame stays consistent.
    frame: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    registered: bool,
}

/// A process-global sink+enabled override for [`TtySpinner::new`], set only by
/// tests via [`set_test_sink`]. Lets an integration test observe the *exact*
/// production `run_parallel` fan-out spinner (which builds its spinner with
/// `TtySpinner::new`, not the injectable `with_sink`) by redirecting frames into
/// an in-memory buffer and forcing the spinner enabled without a real TTY. In
/// production this is always `None`, so `new()` paints to stderr gated on
/// `is_terminal()` exactly as before.
static TEST_SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Installs (or clears with `None`) the global test sink used by
/// [`TtySpinner::new`]. When set, every default spinner is forced enabled and
/// paints into `sink` instead of stderr. Test-only; guard concurrent use with
/// [`TEST_SERIAL`](crate::target::spinner::TEST_SERIAL) in unit tests, or a
/// per-file convention in integration tests.
pub fn set_test_sink(sink: Option<Sink>) {
    *TEST_SINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = sink;
}

/// Resolves whether a default (stderr) spinner should paint.
///
/// The spinner writes to **stderr**, but the enable decision deliberately looks
/// at **either** stderr *or* stdout being a terminal: an interactive REPL that
/// owns a controlling terminal is the right place to animate, and some launchers
/// (notably `cargo run`, some IDE terminals, and `tmux` panes) leave one of the
/// two streams looking non-TTY even in a genuinely interactive session. Checking
/// both recovers the spinner in those setups while still staying a strict no-op
/// when output is truly redirected/piped (both non-TTY) or over `mtui-mcp`.
///
/// Overrides (highest precedence first):
/// 1. `MTUI_FORCE_SPINNER` set to a non-empty, non-`0`/`false`/`no` value → on.
/// 2. `MTUI_NO_SPINNER` (or `MTUI_FORCE_SPINNER=0|false|no`) → off.
/// 3. Auto: stderr *or* stdout is a terminal.
fn spinner_enabled() -> bool {
    let force = std::env::var_os("MTUI_FORCE_SPINNER");
    resolve_spinner_enabled(
        force.as_deref(),
        std::env::var_os("MTUI_NO_SPINNER").is_some(),
        std::io::stderr().is_terminal(),
        std::io::stdout().is_terminal(),
    )
}

/// Pure enable decision, split out so the env/TTY precedence is unit-testable
/// without mutating process-global state. See [`spinner_enabled`] for the
/// precedence contract.
fn resolve_spinner_enabled(
    force: Option<&std::ffi::OsStr>,
    no_spinner: bool,
    stderr_tty: bool,
    stdout_tty: bool,
) -> bool {
    if let Some(v) = force {
        let s = v.to_string_lossy();
        let s = s.trim();
        if !s.is_empty() {
            return !matches!(
                s.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            );
        }
    }
    if no_spinner {
        return false;
    }
    stderr_tty || stdout_tty
}

impl TtySpinner {
    /// Builds a spinner painting to stderr, enabled when the session is
    /// interactive (stderr or stdout is a TTY) or forced via `MTUI_FORCE_SPINNER`.
    ///
    /// When a test sink is installed via [`set_test_sink`], the spinner is
    /// forced enabled and paints into that sink instead — the seam that lets an
    /// integration test observe the production `run_parallel` fan-out spinner
    /// without a real terminal. Off that override (production) the enable
    /// decision is [`spinner_enabled`], which tolerates launchers (`cargo run`,
    /// tmux, IDE terminals) that leave one stream looking non-TTY.
    #[must_use]
    pub fn new(desc: impl Into<String>) -> Self {
        if let Some(sink) = TEST_SINK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Self::build(desc.into(), true, sink);
        }
        Self::build(desc.into(), spinner_enabled(), stderr_sink())
    }

    /// Builds a spinner painting to an injected `sink`, forced `enabled`.
    ///
    /// For unit tests: capture frames into an in-memory buffer without a real
    /// terminal.
    #[must_use]
    pub fn with_sink(desc: impl Into<String>, enabled: bool, sink: Sink) -> Self {
        Self::build(desc.into(), enabled, sink)
    }

    fn build(desc: String, enabled: bool, sink: Sink) -> Self {
        Self {
            id: next_id(),
            desc,
            enabled,
            sink,
            frame: Arc::new(AtomicU64::new(0)),
            handle: None,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            registered: false,
        }
    }

    /// Starts the paint task (no-op when disabled / off a TTY).
    pub fn start(&mut self) {
        if !self.enabled || self.handle.is_some() {
            return;
        }
        // Register this spinner (sink + label + shared frame counter) so
        // `suspend` erases and repaints through the same stream we paint to.
        coord()
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Registered {
                id: self.id,
                sink: Arc::clone(&self.sink),
                desc: self.desc.clone(),
                frame: Arc::clone(&self.frame),
            });
        self.registered = true;
        let desc = self.desc.clone();
        let sink = Arc::clone(&self.sink);
        let stop = Arc::clone(&self.stop);
        let frame = Arc::clone(&self.frame);
        self.handle = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(INTERVAL);
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                {
                    // Re-check under the paint lock: a long `suspend` hold (e.g.
                    // an interactive prompt) can outlive `stop`; never repaint a
                    // frame that `stop` has already erased.
                    let _paint = coord()
                        .lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    // Skip a paint while an interactive read holds the terminal
                    // (`SuspendAsync`); checked under the paint lock so the
                    // read's erase + flag flip cannot race a frame back on.
                    if !coord().paused.load(Ordering::SeqCst) {
                        let i = frame.load(Ordering::SeqCst) as usize;
                        let line = format!("\r[{}] {}", FRAMES[i % 4], desc);
                        let mut w = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let _ = w.write_all(line.as_bytes());
                        let _ = w.flush();
                        // Advance the shared counter so an on-`suspend`-drop
                        // repaint renders the same-or-next frame consistently.
                        frame.fetch_add(1, Ordering::SeqCst);
                    }
                }
                tick.tick().await;
            }
        }));
    }

    /// Stops the paint task and erases the spinner line. Idempotent.
    pub fn stop(&mut self) {
        // Flag the stop first so the `is_stopped` predicate works even off a TTY
        // (where the paint task never ran).
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if !self.enabled || !self.registered {
            return;
        }
        // Erase the spinner line so the next caller writes from column 0, and
        // deregister the sink — both under the paint lock so neither races a
        // frame repaint.
        let mut reg = coord()
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.retain(|r| r.id != self.id);
        self.registered = false;
        let mut w = self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = w.write_all(ERASE.as_bytes());
        let _ = w.flush();
    }

    /// True once [`stop`](TtySpinner::stop) has been called (set even off a TTY).
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Whether this spinner actually paints — `true` only when built for a TTY
    /// (or forced enabled). A diagnostic seam: `enabled=false` is the usual
    /// reason a spinner is invisible on an otherwise interactive session.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl Drop for TtySpinner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An RAII guard that runs a labelled [`TtySpinner`] for its lifetime.
///
/// The Rust equivalent of upstream's `@contextmanager spinner(desc)`: build one
/// with [`spinner`], hold it across a long-running (non-fan-out) operation, and
/// dropping it stops the spinner and erases the frame. A strict no-op off a TTY
/// (tests, redirected output, `mtui-mcp`), like [`TtySpinner`] itself.
#[must_use = "the spinner stops as soon as the guard is dropped"]
pub struct SpinnerGuard {
    inner: TtySpinner,
}

impl SpinnerGuard {
    /// True once the underlying spinner has been stopped (its guard dropped).
    ///
    /// The cooperative-cancellation predicate mirroring upstream's `is_stopped`
    /// yield: a long-running callee can poll this to bail out promptly when the
    /// guard is being torn down.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.inner.is_stopped()
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        self.inner.stop();
    }
}

/// Runs a TTY spinner labelled `desc` for the returned guard's lifetime.
///
/// The high-level counterpart to the fan-out spinner in
/// [`run_parallel`](super::actions::run_parallel): wrap a long-running
/// *non-fan-out* operation (e.g. `regenerate`) by holding the returned
/// [`SpinnerGuard`] across it. A strict no-op off a TTY, so it is safe in tests
/// and over `mtui-mcp`. Dropping the guard stops the spinner and erases its
/// frame.
pub fn spinner(desc: impl Into<String>) -> SpinnerGuard {
    let mut inner = TtySpinner::new(desc);
    inner.start();
    SpinnerGuard { inner }
}

/// The default stderr sink, shared so every default spinner writes to the same
/// handle (stderr is process-global anyway).
fn stderr_sink() -> Sink {
    struct StderrWriter;
    impl Write for StderrWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::io::stderr().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            std::io::stderr().flush()
        }
    }
    Arc::new(Mutex::new(StderrWriter))
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::TEST_SERIAL as GLOBAL;

    /// The enable precedence (pure core, no env mutation): force wins over
    /// everything, then `MTUI_NO_SPINNER`, then stderr-OR-stdout TTY. The
    /// stdout-OR-stderr auto path is what recovers the spinner under launchers
    /// (`cargo run`, tmux, IDE terminals) that leave one stream non-TTY.
    #[test]
    fn resolve_enable_precedence() {
        use std::ffi::OsStr;

        // Force on beats no-TTY and MTUI_NO_SPINNER.
        assert!(resolve_spinner_enabled(
            Some(OsStr::new("1")),
            true,
            false,
            false
        ));
        // Force off beats a real TTY.
        assert!(!resolve_spinner_enabled(
            Some(OsStr::new("0")),
            false,
            true,
            true
        ));
        for off in ["false", "no", "off", "0"] {
            assert!(
                !resolve_spinner_enabled(Some(OsStr::new(off)), false, true, true),
                "{off} should force off"
            );
        }
        // Empty force value is ignored → falls through to auto.
        assert!(resolve_spinner_enabled(
            Some(OsStr::new("")),
            false,
            false,
            true
        ));
        // MTUI_NO_SPINNER off-switch when no force.
        assert!(!resolve_spinner_enabled(None, true, true, true));
        // Auto: either stream being a TTY enables (the cargo-run / tmux fix).
        assert!(resolve_spinner_enabled(None, false, true, false));
        assert!(resolve_spinner_enabled(None, false, false, true));
        assert!(!resolve_spinner_enabled(None, false, false, false));
    }

    /// A `Vec<u8>` sink shared with the test so it can inspect painted frames.
    fn buf_sink() -> (Sink, Arc<Mutex<Vec<u8>>>) {
        // A newtype so the same buffer is both the writer and inspectable.
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let store = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink: Sink = Arc::new(Mutex::new(SharedBuf(Arc::clone(&store))));
        (sink, store)
    }

    fn rendered(store: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(
            &store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }

    #[tokio::test]
    async fn disabled_spinner_is_a_noop_and_never_writes() {
        let (sink, store) = buf_sink();
        let mut s = TtySpinner::with_sink("working", false, sink);
        s.start();
        // No paint task should exist; give any erroneously-spawned task a tick.
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.stop();
        assert!(
            rendered(&store).is_empty(),
            "off-TTY spinner wrote: {:?}",
            rendered(&store)
        );
    }

    #[tokio::test]
    async fn is_stopped_flips_after_stop_even_when_disabled() {
        let (sink, _store) = buf_sink();
        let mut s = TtySpinner::with_sink("x", false, sink);
        assert!(!s.is_stopped());
        s.stop();
        assert!(s.is_stopped());
        // Idempotent.
        s.stop();
        assert!(s.is_stopped());
    }

    #[tokio::test]
    async fn enabled_spinner_paints_frames_then_erases_on_stop() {
        let _serial = GLOBAL.lock().await;
        let (sink, store) = buf_sink();
        let mut s = TtySpinner::with_sink("installing", true, sink);
        s.start();
        // Let it paint at least one frame.
        tokio::time::sleep(Duration::from_millis(150)).await;
        s.stop();
        let out = rendered(&store);
        assert!(out.contains("installing"), "no frame painted: {out:?}");
        assert!(out.contains('['), "frame missing bracket: {out:?}");
        assert!(out.ends_with(ERASE), "stop did not erase: {out:?}");
    }

    #[tokio::test]
    async fn spinner_guard_paints_then_erases_on_drop() {
        let _serial = GLOBAL.lock().await;
        let (sink, store) = buf_sink();
        // Build the guard directly over the injected sink (forced enabled) so
        // the test does not depend on a real TTY, exercising the same
        // start/stop path `spinner()` uses.
        let mut inner = TtySpinner::with_sink("Regenerating x", true, sink);
        inner.start();
        let guard = SpinnerGuard { inner };
        assert!(!guard.is_stopped());
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(guard);
        let out = rendered(&store);
        assert!(out.contains("Regenerating x"), "no frame painted: {out:?}");
        assert!(out.ends_with(ERASE), "drop did not erase: {out:?}");
    }

    #[tokio::test]
    async fn spinner_guard_is_stopped_after_drop_off_tty() {
        // Off a TTY the paint task never runs, but the guard must still report
        // stopped once dropped (cooperative-cancel parity), and write nothing.
        let (sink, store) = buf_sink();
        let inner = TtySpinner::with_sink("x", false, sink);
        let guard = SpinnerGuard { inner };
        assert!(!guard.is_stopped());
        drop(guard);
        assert!(rendered(&store).is_empty(), "off-TTY guard wrote frames");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn suspend_erases_active_frame_and_blocks_repaint() {
        let _serial = GLOBAL.lock().await;
        let (sink, store) = buf_sink();
        let mut s = TtySpinner::with_sink("busy", true, sink);
        s.start();
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Take the suspend guard, capture what it erased, then block a
        // would-be repaint by spinning the paint task's clock forward while the
        // guard is still held. We must NOT `.await` while holding the std guard
        // (it would stall the current-thread runtime's paint task without
        // proving anything), so use a blocking sleep on a worker thread.
        let (before, after) = tokio::task::spawn_blocking(move || {
            let _guard = suspend();
            let before = rendered(&store);
            // Give the paint task real wall-clock time to (fail to) repaint;
            // it is parked on the coordinator lock this guard holds.
            std::thread::sleep(Duration::from_millis(150));
            let after = rendered(&store);
            (before, after)
        })
        .await
        .unwrap();
        assert!(before.ends_with(ERASE), "suspend did not erase: {before:?}");
        assert_eq!(before, after, "spinner repainted while suspended");
        s.stop();
    }
}
