//! `set_log_level` changes the server's one process-wide stderr filter, from
//! any session, and leaves the OTLP diagnostics layer at the startup level.
//!
//! The subscriber is installed globally through the production
//! `install_log_subscriber`, with a capture layer standing in for each of the
//! stderr log and the diagnostics forwarder, and capture is scoped to a
//! thread-local sink — the pattern written up in
//! `mtui-datasources/tests/log_capture.rs`. The filter is process state, so the
//! tests that move it are `#[serial(log_level)]`.

#![cfg(feature = "mcp")]

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use mtui_config::Config;
use mtui_core::{LogLevel, register_all};
use mtui_mcp::{
    LogLevelControl, McpSession, SessionRegistry, http_sessions, install_log_subscriber,
};
use serial_test::serial;
use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sink {
    Log,
    Diag,
}

thread_local! {
    static SINK: RefCell<Option<Vec<(Sink, String)>>> = const { RefCell::new(None) };
}

struct CaptureLayer(Sink);

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
                buf.push((self.0, v.0));
            }
        });
    }
}

fn install() -> Option<LogLevelControl> {
    install_log_subscriber(
        EnvFilter::new("info"),
        CaptureLayer(Sink::Log),
        EnvFilter::new("info"),
        CaptureLayer(Sink::Diag),
    )
}

/// The control of the global subscriber, installed once for this binary.
fn control() -> LogLevelControl {
    static CONTROL: OnceLock<LogLevelControl> = OnceLock::new();
    Arc::clone(
        CONTROL.get_or_init(|| install().expect("no other global subscriber in this binary")),
    )
}

fn sessions() -> SessionRegistry {
    http_sessions(Config::default(), Some(control()))
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

fn finish_capture() -> Vec<(Sink, String)> {
    SINK.with(|s| s.borrow_mut().take()).unwrap_or_default()
}

fn seen(lines: &[(Sink, String)], sink: Sink) -> Vec<&str> {
    lines
        .iter()
        .filter(|(s, _)| *s == sink)
        .map(|(_, l)| l.as_str())
        .collect()
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

    let log = seen(&lines, Sink::Log);
    assert!(log.contains(&"raised"), "{lines:?}");
    assert!(!log.contains(&"before"), "{lines:?}");
    assert!(!log.contains(&"lowered"), "{lines:?}");
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

    let log = seen(&lines, Sink::Log);
    assert!(log.contains(&"raised by alice"), "{lines:?}");
    assert!(!log.contains(&"lowered by bob"), "{lines:?}");
}

#[tokio::test]
#[serial(log_level)]
async fn set_log_level_leaves_the_diagnostics_at_the_startup_level() {
    let session = sessions().make_session();
    control()(LogLevel::Info);

    start_capture();
    set_level(&session, "error").await;
    tracing::warn!(target: "mtui_mcp::probe", "warned");
    set_level(&session, "debug").await;
    tracing::debug!(target: "mtui_mcp::probe", "chatty");
    let lines = finish_capture();
    control()(LogLevel::Info);

    let log = seen(&lines, Sink::Log);
    assert!(!log.contains(&"warned"), "{lines:?}");
    assert!(log.contains(&"chatty"), "{lines:?}");
    let diag = seen(&lines, Sink::Diag);
    assert!(diag.contains(&"warned"), "{lines:?}");
    assert!(!diag.contains(&"chatty"), "{lines:?}");
}

#[test]
fn a_second_install_hands_out_no_control() {
    let _ = control();
    assert!(
        install().is_none(),
        "a control over a subscriber that was never installed would reload nothing"
    );
}
