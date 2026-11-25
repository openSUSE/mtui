//! `set_log_level` changes the server's one process-wide `tracing` filter, from
//! any session.
//!
//! The subscriber is installed globally with the production reload layer, and
//! capture is scoped to a thread-local sink — the pattern written up in
//! `mtui-datasources/tests/log_capture.rs`. The filter is process state, so the
//! tests that move it are `#[serial(log_level)]`.

#![cfg(feature = "mcp")]

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use mtui_config::Config;
use mtui_core::{LogLevel, register_all};
use mtui_mcp::{LogLevelControl, McpSession, SessionRegistry, reloadable_log_filter};
use serial_test::serial;
use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

thread_local! {
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

/// The control of the global reload layer, installed once for this binary.
fn control() -> LogLevelControl {
    static CONTROL: OnceLock<LogLevelControl> = OnceLock::new();
    Arc::clone(CONTROL.get_or_init(|| {
        let (filter, control) = reloadable_log_filter(EnvFilter::new("info"));
        tracing::subscriber::set_global_default(
            Registry::default().with(filter).with(CaptureLayer),
        )
        .expect("no other global subscriber in this binary");
        control
    }))
}

fn sessions() -> SessionRegistry {
    SessionRegistry::new(Arc::new(register_all()), Config::default()).with_log_level(control())
}

async fn set_level(session: &McpSession, level: &str) {
    session
        .run_command(&register_all(), "set_log_level", &[level.to_owned()])
        .await
        .expect("set_log_level succeeds");
}

fn start_capture() {
    SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
}

fn finish_capture() -> Vec<String> {
    SINK.with(|s| s.borrow_mut().take()).unwrap_or_default()
}

#[tokio::test]
#[serial(log_level)]
async fn set_log_level_raises_and_lowers_the_server_level() {
    let session = sessions().make_session();
    control()(LogLevel::Info);

    start_capture();
    tracing::debug!(target: "mtui_mcp::probe", "before");
    set_level(&session, "debug").await;
    tracing::debug!(target: "mtui_mcp::probe", "raised");
    set_level(&session, "info").await;
    tracing::debug!(target: "mtui_mcp::probe", "lowered");
    let lines = finish_capture();

    assert!(lines.iter().any(|l| l == "raised"), "{lines:?}");
    assert!(!lines.iter().any(|l| l == "before"), "{lines:?}");
    assert!(!lines.iter().any(|l| l == "lowered"), "{lines:?}");
}

#[tokio::test]
#[serial(log_level)]
async fn sessions_of_one_http_server_share_the_level() {
    let registry = sessions();
    let alice = registry.make_session();
    let bob = registry.make_session();
    control()(LogLevel::Info);

    start_capture();
    set_level(&alice, "debug").await;
    tracing::debug!(target: "mtui_mcp::probe", "raised by alice");
    set_level(&bob, "info").await;
    tracing::debug!(target: "mtui_mcp::probe", "lowered by bob");
    let lines = finish_capture();

    assert!(lines.iter().any(|l| l == "raised by alice"), "{lines:?}");
    assert!(!lines.iter().any(|l| l == "lowered by bob"), "{lines:?}");
}
