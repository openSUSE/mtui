//! Regression: a live TTY spinner survives concurrent "logging" writes.
//!
//! The visible symptom this guards against (mtui-rs-wbo): the spinner is
//! started by `run_parallel`/`run_fanout`, but during a fan-out the worker log
//! lines and command output were written straight to the terminal, clobbering
//! the `\r[|] desc` frame so it never rendered cleanly. Upstream wraps every log
//! record in `spinner_suspended()` (`SpinnerAwareStreamHandler`); the Rust port
//! wires the same `mtui_hosts::suspend()` guard into the tracing writer and the
//! command display.
//!
//! This test drives an enabled [`TtySpinner`] into an in-memory sink (so no real
//! TTY is needed) while concurrently emitting several "log lines" *through the
//! `suspend()` guard*, exactly as the production writers do. It asserts:
//!
//! 1. every guarded write is preceded by the frame-erase sequence (`\r\x1b[K`),
//!    so a log line lands on a clean line rather than on top of the frame; and
//! 2. the frame label (`desc`) still appears in the output after the writes —
//!    the spinner survived the concurrent logging and repainted.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mtui_hosts::suspend;
use mtui_hosts::target::spinner::{Sink, TtySpinner};

/// The erase-current-line escape the spinner and `suspend` emit: carriage
/// return + clear-to-end-of-line.
const ERASE: &str = "\r\x1b[K";

/// A shared in-memory sink so the test can inspect everything painted/written.
fn shared_sink() -> (Sink, Arc<Mutex<Vec<u8>>>) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_survive_concurrent_logging() {
    let (sink, store) = shared_sink();
    // Force the spinner enabled and pointed at the shared sink so the test does
    // not depend on a real TTY, while exercising the same paint/erase path the
    // production stderr spinner uses.
    let mut spin = TtySpinner::with_sink("running", true, Arc::clone(&sink));
    spin.start();

    // Let the spinner paint at least one frame.
    tokio::time::sleep(Duration::from_millis(120)).await;

    // Emit several "log lines" through the SAME sink, each guarded by
    // `suspend()` exactly as the tracing writer / display do. The guard erases
    // the live frame first and blocks a repaint for the duration of the write.
    for i in 0..5 {
        // Blocking write under the (non-Send) suspend guard on a worker thread,
        // mirroring how the synchronous production writers hold the guard only
        // around the write and never across an await.
        let sink = Arc::clone(&sink);
        tokio::task::spawn_blocking(move || {
            let _quiet = suspend();
            let mut w = sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = writeln!(w, "info: log line {i}");
            let _ = w.flush();
        })
        .await
        .unwrap();
        // A short gap so the spinner gets a chance to repaint between lines.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // Give the spinner a final tick to repaint after the last log line, then stop.
    tokio::time::sleep(Duration::from_millis(120)).await;
    spin.stop();

    let out = rendered(&store);

    // 1. Every log line is preceded by an erase, so it lands flush-left rather
    //    than on top of a frame. Assert the erase-then-log ordering for each.
    for i in 0..5 {
        let line = format!("info: log line {i}");
        let needle = format!("{ERASE}{line}");
        assert!(
            out.contains(&needle),
            "log line {i} not preceded by frame erase; suspend() not wired.\noutput: {out:?}"
        );
    }

    // 2. The spinner survived the concurrent logging: its label appears (it
    //    painted frames), and after the last log line it repainted at least once
    //    before stop — i.e. logging did not permanently kill the spinner.
    assert!(
        out.contains("running"),
        "spinner never painted its label: {out:?}"
    );
    let last_log = out.rfind("info: log line 4").expect("last log present");
    let tail = &out[last_log..];
    assert!(
        tail.contains("[") && tail.contains("running"),
        "spinner did not repaint after the final log line: {tail:?}"
    );

    // 3. The perceptibility fix (the user's symptom): EVERY guarded log line is
    //    *immediately* followed by a repainted frame — the frame reappears on the
    //    fresh line the instant the suspend guard drops, not up to a tick later.
    //    Without on-drop repaint, near-simultaneous per-host `info:` lines erase
    //    every brief frame and the spinner is never seen.
    for i in 0..5 {
        let line = format!("info: log line {i}\n");
        let at = out.find(&line).expect("each log line present");
        let after = &out[at + line.len()..];
        assert!(
            after.starts_with("\r["),
            "log line {i} not immediately followed by a repainted frame \
             (on-drop repaint missing); the spinner would be invisible for fast \
             fan-outs.\nafter: {after:?}"
        );
    }
}
