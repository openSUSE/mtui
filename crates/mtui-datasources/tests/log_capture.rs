//! Shared tracing-capture helper for the integration suite.
//!
//! One copy for the whole `it` binary, because the subscriber it installs is
//! **global** and `set_global_default` succeeds only once per process.
//!
//! ## Why global rather than `tracing::subscriber::set_default`
//!
//! `tracing` caches callsite *interest* process-wide. A callsite first reached
//! from a thread with no subscriber installed is cached as `Interest::never()`
//! and then stays silent for every later capture, no matter which subscriber is
//! active — and a thread-local default does not invalidate that cache. With
//! libtest running tests in parallel, whether a given callsite was "poisoned"
//! before a capturing test reached it is a race, so a log assertion fails (or,
//! worse, passes vacuously) depending on test order.
//!
//! A subscriber that is always installed keeps interest pinned to `always`.
//! Scoping moves to a thread-local sink: events on a thread with no active
//! capture are dropped, so concurrent tests cannot see each other's lines.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

thread_local! {
    /// The active capture buffer for this thread, if any.
    static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct CaptureLayer;

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        SINK.with(|s| {
            if let Some(buf) = s.borrow_mut().as_mut() {
                let mut v = MessageVisitor(String::new());
                event.record(&mut v);
                buf.push(v.0);
            }
        });
    }
}

/// Install the forwarding subscriber once for this test binary.
fn install() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // A pre-existing global default (another harness, a future test)
        // is not an error here: the capture simply yields nothing, which the
        // callers' anti-vacuity panics report loudly.
        let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
    });
}

/// Capture the `message` field of every tracing event emitted by `f` on this
/// thread, newline-joined.
pub async fn capture_logs<F, Fut>(f: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    install();
    SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
    f().await;
    SINK.with(|s| s.borrow_mut().take())
        .unwrap_or_default()
        .join("\n")
}
