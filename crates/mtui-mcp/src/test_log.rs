//! The crate's one `tracing` capture for unit tests.
//!
//! The subscriber is **global** because `tracing` caches callsite interest
//! process-wide: a callsite first reached from a thread with no subscriber is
//! cached `Interest::never()` and stays silent for every later capture, so a
//! thread-local default makes a log assertion pass or fail by test order.
//! Scoping moves to the thread-local sink instead, and only events emitted on
//! the capturing thread are collected — a `spawn_blocking` hop lands elsewhere.
//!
//! Only `mtui*` targets are collected. The assertions here are about what mtui
//! logs; `hyper_util`'s pool chatter (suppressed in production by
//! `runner::default_directives`) carries the socket address that a
//! "never log the endpoint" check forbids, and a background export landing
//! inside a capture window would fail that check with a foreign crate's line.
//!
//! It lives in its own module rather than in one test module because
//! `set_global_default` succeeds once: a second module installing its own layer
//! would lose the race and capture nothing, by test order.
//!
//! Accepted cost: the unfiltered `Registry` reports no `max_level_hint`, so
//! `LevelFilter::current()` is `TRACE` for this test binary. The workspace's
//! fourth copy of the pattern — a `#[cfg(test)]` module cannot share an
//! integration test's file; the fullest write-up is
//! `mtui-datasources/tests/log_capture.rs`.

thread_local! {
    /// Buffer for the capture in progress on this thread, or `None` when no
    /// capture is active — events from a thread without one are dropped.
    static CAPTURE_SINK: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `fut` with this thread's tracing events collected, returning its output
/// and the captured lines (`message` first, then the event's own fields as
/// `name=value`), newline-joined.
pub(crate) async fn capture_logs<T>(fut: impl std::future::Future<Output = T>) -> (T, String) {
    start();
    let out = fut.await;
    (out, finish())
}

/// [`capture_logs`] for a blocking body, so an event the call emits on this
/// thread is collected rather than raced onto the blocking pool.
pub(crate) fn capture_logs_blocking<T>(body: impl FnOnce() -> T) -> (T, String) {
    start();
    let out = body();
    (out, finish())
}

fn start() {
    install_capture_subscriber();
    CAPTURE_SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
}

fn finish() -> String {
    CAPTURE_SINK
        .with(|s| s.borrow_mut().take())
        .unwrap_or_default()
        .join("\n")
}

/// Install the permissive global subscriber backing the captures, once per test
/// binary.
fn install_capture_subscriber() {
    use std::fmt::Write as _;
    use std::sync::OnceLock;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    struct CaptureLayer;

    #[derive(Default)]
    struct MessageVisitor {
        message: String,
        fields: String,
    }
    impl Visit for MessageVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                let _ = write!(self.message, "{value:?}");
            } else {
                let _ = write!(self.fields, " {}={value:?}", field.name());
            }
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if !event.metadata().target().starts_with("mtui") {
                return;
            }
            CAPTURE_SINK.with(|s| {
                if let Some(buf) = s.borrow_mut().as_mut() {
                    let mut visitor = MessageVisitor::default();
                    event.record(&mut visitor);
                    buf.push(format!("{}{}", visitor.message, visitor.fields));
                }
            });
        }
    }

    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn foreign_targets_never_reach_the_capture() {
        // The transport line is the leak shape from hyper-util's pool, the one
        // that made `no endpoint leaks` fail on a background export.
        let (_, logs) = super::capture_logs_blocking(|| {
            tracing::debug!(
                target: "hyper_util::client::legacy::pool",
                "pooling idle connection for (\"http\", 127.0.0.1:44731)"
            );
            tracing::warn!(target: "mtui_mcp::probe", "mtui warn reaches the capture");
        });
        assert_eq!(logs, "mtui warn reaches the capture");
    }
}
