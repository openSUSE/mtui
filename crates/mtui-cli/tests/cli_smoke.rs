//! Smoke tests for the `mtui` binary (P6.1 skeleton + P6.2 REPL entry).
//!
//! These drive the built binary via `CARGO_BIN_EXE_mtui` and assert the
//! top-level surfaces: `--version` (provenance block), `--help` (real `Args`
//! flags — not the old empty stub), an unknown-flag usage error, that a bare
//! invocation enters the interactive REPL, and that a piped (non-TTY) stdin
//! reaches the REPL and terminates cleanly instead of hanging. Arg-parsing
//! internals are already covered in `mtui-core::args`; this only exercises the
//! binary's own wiring. The REPL loop's dispatch logic is unit-tested off the
//! TTY seam in `repl::tests` (the `step` function), and the startup seeding
//! logic (`-a`/`-k` update load + `--sut`, incl. the explicit-update exit-1
//! path) hermetically in `startup::tests` (the `seed_session` function).
//!
//! There is deliberately **no** non-interactive / single-command CLI mode to
//! e2e here: like upstream `mtui` (`mtui/main.py`, only `cmdloop`), the `mtui`
//! binary's one driving surface is the REPL — headless single-command dispatch
//! is an `mtui-mcp`/`run_once` concern, covered in `mtui-core`/`mtui-mcp`. The
//! closest binary-level analogue is the piped-stdin path exercised below: it
//! drives the real `reedline` editor (not the `step` unit seam), so it is the
//! only test that reaches `repl::Repl::run` itself — the intro banner and the
//! loop's exit-on-`read_line`-failure arm.

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
    // Assert `mtui <crate-version>` without hardcoding the number (tracks bumps).
    assert!(
        stdout.contains(&format!("mtui {}", env!("CARGO_PKG_VERSION"))),
        "expected version string, got: {stdout:?}"
    );
    // The provenance block is rendered as `mtui <ver> (<...>)`; assert the paren
    // is present so this is the mtui-core `Args` version, not a bare stub.
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
    // `--auto-review-id` only exists on the real `mtui_core::Args` parser, never
    // on the old empty Phase-0 `Cli {}` stub — proves the rewiring landed.
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
    // A bare invocation now drops into the REPL (P6.2) instead of bailing. The
    // test harness has no controlling TTY, so `reedline::read_line` fails and
    // the process exits non-zero — but the DEBUG breadcrumb proves we reached
    // the REPL entry rather than an earlier error, and stderr must NOT carry the
    // old "not yet implemented" bail.
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
    // Drive the real binary with a piped (non-TTY) stdin. This is the only test
    // that reaches `repl::Repl::run` itself (the unit tests exercise the `step`
    // seam, which bypasses the `reedline` editor). Two things must hold:
    //
    //   1. The REPL is entered — proven by the intro banner
    //      ("Maintenance Test Update Installer", printed once at the top of
    //      `run` before the first prompt).
    //   2. The process terminates on its own — `reedline::read_line` cannot use
    //      a non-TTY stdin, so it returns an editor I/O error which `run`
    //      propagates; `main` exits non-zero. The key property is that it does
    //      NOT hang waiting for a controlling terminal.
    //
    // We do not assert a specific exit code: a bare `wait()` after closing the
    // pipe deadlocks only if the child hangs, so reaching the assertions at all
    // is itself the liveness proof. (A pty harness would let us drive real input
    // lines and assert exit 0 on EOF, but that is out of scope for P6.8 — the
    // 80% bar is met without adding a pty dependency.)
    let mut child = mtui()
        .arg("-d")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mtui with piped stdin");

    // Feed one command line, then close the pipe (EOF).
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
    // Without RUST_LOG, `-d` must surface the DEBUG startup breadcrumb that the
    // default (info) run suppresses. Clear RUST_LOG so the test is hermetic.
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
