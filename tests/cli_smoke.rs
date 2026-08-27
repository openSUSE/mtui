//! Smoke tests for the `mtui` binary.
//!
//! These drive the built binary via `CARGO_BIN_EXE_mtui` and assert only its
//! own top-level wiring: `--version`, `--help`, an unknown-flag usage error, and
//! that a bare or piped (non-TTY) invocation reaches the REPL and terminates
//! cleanly. Arg-parsing internals live in `mtui-core::args`, the dispatch loop in
//! `repl::tests` (the `step` seam), and startup seeding in `startup::tests`.
//!
//! There is deliberately no single-command CLI mode to e2e — headless dispatch
//! is an `mtui-mcp` concern. The piped-stdin test below is the only one that
//! drives the real `reedline` editor, so it is the only reach into
//! `repl::Repl::run` itself.
//!
//! Gated behind the `cli` feature: the `mtui` binary only exists then.

#![cfg(feature = "cli")]

use std::io::Write;
use std::process::{Command, Stdio};

fn mtui() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mtui"))
}

#[test]
fn version_prints_provenance_block_and_exits_zero() {
    let out = mtui().arg("--version").output().expect("run --version");
    assert!(out.status.success(), "--version must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Not hardcoded, so this tracks version bumps.
    assert!(
        stdout.contains(&format!("mtui {}", env!("CARGO_PKG_VERSION"))),
        "expected version string, got: {stdout:?}"
    );
    // The provenance block renders as `mtui <ver> (<...>)`, so the paren proves
    // this is the mtui-core `Args` version and not a bare stub.
    assert!(
        stdout.contains('('),
        "expected build-provenance block in --version, got: {stdout:?}"
    );
}

#[test]
fn help_lists_real_args_and_exits_zero() {
    let out = mtui().arg("--help").output().expect("run --help");
    assert!(out.status.success(), "--help must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `--auto-review-id` exists only on the real `mtui_core::Args` parser.
    assert!(
        stdout.contains("--auto-review-id"),
        "expected real Args flags in --help, got: {stdout:?}"
    );
    assert!(
        stdout.contains("--kernel-review-id") && stdout.contains("--color"),
        "expected the full top-level flag set, got: {stdout:?}"
    );
}

#[test]
fn unknown_flag_is_usage_error_exit_two() {
    let out = mtui().arg("--nope").output().expect("run bad flag");
    assert_eq!(
        out.status.code(),
        Some(2),
        "clap usage errors must exit 2, got: {:?}",
        out.status.code()
    );
}

#[test]
fn no_args_enters_the_interactive_repl() {
    // The harness has no controlling TTY, so `reedline::read_line` fails and
    // the process exits non-zero; the DEBUG breadcrumb is what proves the REPL
    // entry was reached rather than an earlier error.
    let out = mtui()
        .arg("-d")
        .env_remove("RUST_LOG")
        .output()
        .expect("run with no args");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mtui starting"),
        "a bare invocation must reach the REPL entry, got: {stderr:?}"
    );
    assert!(
        !stderr.contains("not yet implemented"),
        "the REPL bail placeholder must be gone, got: {stderr:?}"
    );
}

#[test]
fn piped_stdin_reaches_repl_and_exits_without_hanging() {
    // Two properties: the intro banner proves the REPL was entered, and the
    // process must terminate on its own rather than hang waiting for a
    // controlling terminal (`read_line` fails on a non-TTY stdin and `run`
    // propagates the error). No exit code is asserted — `wait()` after closing
    // the pipe deadlocks only if the child hangs, so reaching the assertions at
    // all is the liveness proof. Driving real input lines would need a pty.
    let mut child = mtui()
        .arg("-d")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mtui with piped stdin");

    // One command line, then EOF.
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(b"help\n")
        .expect("write to child stdin");

    let out = child.wait_with_output().expect("wait for mtui");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("Maintenance Test Update Installer"),
        "piped stdin must still reach the REPL (intro banner), got stdout: \
         {stdout:?} stderr: {stderr:?}"
    );
    assert!(
        !stderr.contains("panicked"),
        "the REPL must terminate cleanly (no panic) on a non-TTY stdin, got \
         stderr: {stderr:?}"
    );
}

#[test]
fn debug_flag_raises_tracing_level() {
    // `RUST_LOG` is cleared so the comparison is hermetic.
    let with_debug = mtui()
        .arg("-d")
        .env_remove("RUST_LOG")
        .output()
        .expect("run -d");
    let default = mtui().env_remove("RUST_LOG").output().expect("run default");

    let dbg_err = String::from_utf8_lossy(&with_debug.stderr);
    let def_err = String::from_utf8_lossy(&default.stderr);
    assert!(
        dbg_err.contains("mtui starting"),
        "-d must emit the DEBUG breadcrumb, got: {dbg_err:?}"
    );
    assert!(
        !def_err.contains("mtui starting"),
        "default run must not emit the DEBUG breadcrumb, got: {def_err:?}"
    );
}
