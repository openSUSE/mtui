//! Smoke tests for the `mtui` binary skeleton (P6.1).
//!
//! These drive the built binary via `CARGO_BIN_EXE_mtui` and assert the four
//! top-level surfaces: `--version` (provenance block), `--help` (real `Args`
//! flags — not the old empty stub), an unknown-flag usage error, and the
//! not-yet-implemented REPL bail. Arg-parsing internals are already covered in
//! `mtui-core::args`; this only exercises the binary's own wiring.

use std::process::Command;

fn mtui() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mtui"))
}

#[test]
fn version_prints_provenance_block_and_exits_zero() {
    let out = mtui().arg("--version").output().expect("run --version");
    assert!(out.status.success(), "--version must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("mtui 0.1.0"),
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
fn no_args_bails_with_repl_message_and_nonzero_exit() {
    let out = mtui().output().expect("run with no args");
    assert!(
        !out.status.success(),
        "a bare invocation must fail until the REPL exists"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("REPL not yet implemented"),
        "expected the not-yet-implemented REPL message, got: {stderr:?}"
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
