//! Shared tracing-capture helper for the integration suite.
//!
//! The subscriber is installed **globally** and the sink is thread-local:
//! `tracing` caches callsite interest process-wide, so a thread-local default
//! would let a callsite first reached without a subscriber stay silent for
//! every later capture. See `crates/mtui-datasources/tests/log_capture.rs`.

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
        let _ = write!(self.0, "{}={value:?} ", field.name());
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

fn install() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
    });
}

/// Capture every field of every tracing event `f` emits on this thread,
/// newline-joined.
pub fn capture_logs(f: impl FnOnce()) -> String {
    install();
    SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
    f();
    SINK.with(|s| s.borrow_mut().take())
        .unwrap_or_default()
        .join("\n")
}
