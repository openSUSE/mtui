//! Shared tracing-capture helper for the integration suite.
//!
//! One copy for the whole `it` binary: `set_global_default` succeeds only once
//! per process. The subscriber is global and only the sink is thread-local, so
//! a callsite first reached from another thread cannot be cached as disabled,
//! and concurrent tests cannot see each other's events.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

thread_local! {
    static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct CaptureLayer;

struct FieldVisitor(String);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        SINK.with(|s| {
            if let Some(buf) = s.borrow_mut().as_mut() {
                let mut v = FieldVisitor(String::new());
                event.record(&mut v);
                buf.push(v.0);
            }
        });
    }
}

fn start() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
    });
    SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
}

fn finish() -> String {
    SINK.with(|s| s.borrow_mut().take())
        .unwrap_or_default()
        .join("\n")
}

/// Every event `f` emits on this thread, all fields included, one per line.
pub fn capture_events(f: impl FnOnce()) -> String {
    start();
    f();
    finish()
}

/// [`capture_events`] for a future, which must run on this thread (the
/// default current-thread `#[tokio::test]` runtime).
pub async fn capture_events_async(f: impl std::future::Future<Output = ()>) -> String {
    start();
    f.await;
    finish()
}
