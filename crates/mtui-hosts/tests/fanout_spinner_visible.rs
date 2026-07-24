//! Decisive regression: the fan-out TTY spinner actually paints during a
//! long-running `HostsGroup::run` on an *interactive* group.
//!
//! This exercises the exact production path a `run` / `update` fan-out takes —
//! `HostsGroup::run` → `RunCommand` → `run_fanout` → `run_parallel`, which builds
//! its spinner with `TtySpinner::new` — rather than a hand-built spinner. A
//! process-global test sink (`set_test_sink`) redirects that production spinner's
//! frames into an in-memory buffer and forces it enabled without a real TTY, and
//! a delayed [`MockConnection`] models a multi-second command so the paint task
//! has a window to tick.
//!
//! If the spinner is wired correctly, the buffer contains at least one animated
//! frame (`[|] run` / `[/] run` / …) painted while the command was in flight.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mtui_hosts::{HostsGroup, MockConnection, Sink, Target, set_test_sink};
use mtui_types::enums::TargetState;
use serial_test::serial;

/// A shared in-memory sink so the test can read the painted frames.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(test_sink)]
async fn interactive_fanout_run_paints_a_spinner() {
    let (sink, store) = shared_sink();
    // Route the production `run_parallel` spinner (built via `TtySpinner::new`)
    // into our buffer, forced enabled — the seam that lets us observe it without
    // a real terminal. Cleared at the end so sibling tests are unaffected.
    set_test_sink(Some(Arc::clone(&sink)));

    // A connected, enabled, PARALLEL host whose command takes ~500ms — the model
    // of a real `zypper` update where the fan-out awaits long enough to paint.
    let conn = MockConnection::new("h1").with_run_delay(Duration::from_millis(500));
    let target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));

    // interactive = true — exactly what the REPL builds; this is the flag that
    // gates whether `run_parallel` receives a `desc` and starts the spinner.
    let mut group = HostsGroup::new(vec![target], true);

    // Drive the real fan-out. `run` returns only after the 500ms command, giving
    // the 100ms paint interval several ticks.
    group.run("zypper up").await;

    set_test_sink(None);

    let out = String::from_utf8_lossy(
        &store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();

    // The production fan-out labels its spinner "run" (RunCommand's desc). Assert
    // at least one animated frame was painted during the command window.
    assert!(
        out.contains("] run"),
        "no fan-out spinner frame painted during an interactive long `run`; \
         the spinner is not reaching the terminal.\ncaptured: {out:?}"
    );
    let painted_a_frame = ['|', '/', '-', '\\']
        .iter()
        .any(|f| out.contains(&format!("[{f}] run")));
    assert!(
        painted_a_frame,
        "spinner label present but no |/-\\ animation frame: {out:?}"
    );
}
