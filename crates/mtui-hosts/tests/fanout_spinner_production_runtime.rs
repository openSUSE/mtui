//! Reproduce the REPL's EXACT runtime setup — `tokio::runtime::Runtime::new()`
//! with `block_on` — driving a long interactive fan-out, and assert the spinner
//! paints DURING the await, not only via an on-drop repaint.
//!
//! The REPL (`mtui-cli/src/main.rs`) builds its runtime with
//! `tokio::runtime::Runtime::new()` and drives every command via `block_on`.
//! The paint task is `tokio::spawn`ed inside `run_parallel`. If that runtime
//! does not actually schedule the spawned paint task concurrently with the
//! `block_on`-driven fan-out (e.g. it resolves to a current-thread runtime, or
//! the workers are starved), the spinner never paints during a slow SSH command
//! — exactly the reported symptom (blinking cursor, no frame, >10s between logs).

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mtui_hosts::{HostsGroup, MockConnection, Sink, Target, set_test_sink};
use mtui_types::enums::{ExecutionMode, TargetState};
use serial_test::serial;

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

#[test]
#[serial(test_sink)]
fn production_runtime_block_on_paints_spinner_during_slow_command() {
    let (sink, store) = shared_sink();
    set_test_sink(Some(Arc::clone(&sink)));

    // EXACTLY the REPL's runtime construction.
    let runtime = tokio::runtime::Runtime::new().expect("runtime");

    runtime.block_on(async {
        let conn = MockConnection::new("h1").with_run_delay(Duration::from_millis(800));
        let target = Target::with_connection(
            "h1",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        );
        let mut group = HostsGroup::new(vec![target], true);
        group.run("zypper up").await;
    });

    set_test_sink(None);

    let out = String::from_utf8_lossy(
        &store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();

    // Count distinct animation frames: a spinner painting during the 800ms await
    // (100ms interval) must produce SEVERAL frames, not just one repaint.
    let frame_writes = out.matches("\r[").count();
    assert!(
        frame_writes >= 3,
        "spinner painted only {frame_writes} frame(s) during an 800ms command on \
         the production Runtime::new()+block_on path — the paint task is not being \
         scheduled concurrently.\ncaptured: {out:?}"
    );
}
