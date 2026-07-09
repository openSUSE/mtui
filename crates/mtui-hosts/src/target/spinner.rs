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
//! threads. mtui-rs fans out with `tokio` tasks, so [`TtySpinner`] paints from a
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

/// Process-wide paint coordinator: the lock every frame erase/paint takes, and
/// the registry of spinners currently painting, keyed by id and carrying each
/// spinner's own frame sink so [`suspend`] can erase through the *same* sink the
/// spinner paints to (in production stderr; in tests an in-memory buffer). When
/// the registry is empty [`suspend`] is a strict no-op — in particular off a
/// TTY, where spinners never register.
struct PaintCoord {
    /// Serialises every erase/paint; also guards the active-sink registry.
    lock: Mutex<Vec<(u64, Sink)>>,
    /// When set, paint tasks skip repainting — an interactive read is in
    /// progress (see [`SuspendAsync`]). Checked under the paint lock so a read
    /// can never race a frame back onto the terminal it just cleared.
    paused: AtomicBool,
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
    _guard: std::sync::MutexGuard<'static, Vec<(u64, Sink)>>,
}

/// Pauses spinner painting and erases any visible frame for the returned guard's
/// lifetime.
///
/// A no-op (beyond taking the paint lock) when no spinner is active. Off a TTY
/// no spinner ever registers, so this is effectively free there. The erase is
/// written through each active spinner's own sink so it lands on the same stream
/// the frames were painted to.
pub fn suspend() -> Suspend {
    let guard = coord()
        .lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (_, sink) in guard.iter() {
        // Best-effort erase; a failed write must never poison the guard.
        let mut w = sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = w.write_all(ERASE.as_bytes());
        let _ = w.flush();
    }
    Suspend { _guard: guard }
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
    for (_, sink) in guard.iter() {
        let mut w = sink
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
    handle: Option<JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    registered: bool,
}

impl TtySpinner {
    /// Builds a spinner painting to stderr, enabled only when stderr is a TTY.
    #[must_use]
    pub fn new(desc: impl Into<String>) -> Self {
        Self::build(desc.into(), std::io::stderr().is_terminal(), stderr_sink())
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
        // Register this spinner's sink so `suspend` erases through the same
        // stream we paint to.
        coord()
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((self.id, Arc::clone(&self.sink)));
        self.registered = true;
        let desc = self.desc.clone();
        let sink = Arc::clone(&self.sink);
        let stop = Arc::clone(&self.stop);
        self.handle = Some(tokio::spawn(async move {
            let mut i: usize = 0;
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
                        let frame = format!("\r[{}] {}", FRAMES[i % 4], desc);
                        let mut w = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let _ = w.write_all(frame.as_bytes());
                        let _ = w.flush();
                        i += 1;
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
        reg.retain(|(id, _)| *id != self.id);
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
}

impl Drop for TtySpinner {
    fn drop(&mut self) {
        self.stop();
    }
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
