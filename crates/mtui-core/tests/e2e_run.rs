//! Golden end-to-end test for the `run` command.
//!
//! Drives `run "uname -a"` across two mock reference hosts through the **real
//! dispatch path**: the process-wide [`register_all`] registry and the
//! line-dispatch [`dispatch_line`] engine both the REPL and MCP use. Asserts the
//! aggregated, per-host output shape (`host:-> <cmd> [<exit>]` + stdout).

mod support;

use mtui_core::display::{ColorMode, CommandPromptDisplay};
use mtui_core::{Session, dispatch_line, register_all};
use support::{Buffer, FakeReport};

/// Builds a headless session with a captured display and one loaded report
/// whose hosts script `uname -a`.
fn session_with(buf: &Buffer, rrid: &str, hosts: &[&str]) -> Session {
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    let mut session = Session::with_display(mtui_config::Config::default(), false, display);
    session.templates.add(
        FakeReport::with_scripted_hosts(rrid, hosts, "uname -a", "Linux ref 6.4.0 x86_64").boxed(),
    );
    session
}

#[tokio::test]
async fn run_uname_across_two_hosts_aggregates_colored_output_shape() {
    let registry = register_all();
    let buf = Buffer::default();
    let mut session = session_with(&buf, "SUSE:Maintenance:1:1", &["ref-host-1", "ref-host-2"]);

    dispatch_line(&registry, &mut session, "run uname -a")
        .await
        .expect("run should succeed across both hosts");

    let out = buf.contents();

    // Identical clean blocks share one combined banner and body.
    assert!(
        out.contains("ref-host-1, ref-host-2:-> uname -a [0]"),
        "missing combined banner:\n{out}"
    );
    assert!(
        out.contains("…[output identical on 2 hosts folded]"),
        "missing fold marker:\n{out}"
    );
    assert_eq!(
        out.matches("Linux ref 6.4.0 x86_64").count(),
        1,
        "shared body once:\n{out}"
    );
    // No stderr block when stderr is empty.
    assert!(!out.contains("stderr:"), "unexpected stderr block:\n{out}");
    // Verdict stays at the head so truncation preserves it.
    let verdict = out.find("run completed on").expect("missing verdict");
    let body = out.find("ref-host-1, ref-host-2:->").expect("missing body");
    assert!(verdict < body, "verdict must precede the body:\n{out}");
}

#[tokio::test]
async fn run_with_no_loaded_report_errors() {
    let registry = register_all();
    let buf = Buffer::default();
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    let mut session = Session::with_display(mtui_config::Config::default(), false, display);

    // No template loaded → the run resolves to the null report with no hosts.
    let err = dispatch_line(&registry, &mut session, "run true")
        .await
        .expect_err("run with no refhosts must error");
    assert!(err.to_string().contains("No refhosts defined"), "{err}");
}

#[tokio::test]
async fn run_targets_a_single_host_with_dash_t() {
    let registry = register_all();
    let buf = Buffer::default();
    let mut session = session_with(&buf, "SUSE:Maintenance:1:1", &["ref-host-1", "ref-host-2"]);

    dispatch_line(&registry, &mut session, "run -t ref-host-2 uname -a")
        .await
        .expect("targeted run should succeed");

    let out = buf.contents();
    assert!(out.contains("ref-host-2:-> uname -a [0]"), "{out}");
    assert!(
        !out.contains("ref-host-1:->"),
        "unselected host must not run:\n{out}"
    );
}
