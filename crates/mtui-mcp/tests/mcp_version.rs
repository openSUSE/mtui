//! Smoke test for the `mtui-mcp` binary's `--version` surface (P8.1).
//!
//! Mirrors `mtui-cli/tests/cli_smoke.rs::version_prints_provenance_block_and_exits_zero`
//! for the second binary: drive the built `mtui-mcp` via `CARGO_BIN_EXE_mtui-mcp`
//! and assert the `mtui-mcp <ver> (<ref>, <profile>, <target>)` provenance block
//! and a clean exit. We assert shape/presence only — never the exact SHA (changes
//! every commit) nor exact versions (brittle on bumps).
//!
//! Gated behind the `mcp` feature: without it the `[[bin]]` links a stub that
//! exits 2 (the MCP SDK, and thus clap's `--version` handling in `McpArgs`, is
//! compiled in only under `mcp`). Run with `cargo test -p mtui-mcp --features mcp`.

#![cfg(feature = "mcp")]

use std::process::Command;

fn mtui_mcp() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mtui-mcp"))
}

#[test]
fn version_prints_provenance_block_and_exits_zero() {
    let out = mtui_mcp().arg("--version").output().expect("run --version");
    assert!(out.status.success(), "--version must exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // clap renders `<bin-name> <long_version>`; the bin name is `mtui-mcp`. Assert
    // the crate version via env! rather than a literal so a version bump (or a
    // tag-relative `git describe` ref like `v0.9.0-0-g…`) doesn't break this.
    assert!(
        stdout.contains(&format!("mtui-mcp {}", env!("CARGO_PKG_VERSION"))),
        "expected version string, got: {stdout:?}"
    );
    // The provenance block is rendered as `mtui-mcp <ver> (<...>)`; assert the
    // paren is present so this is the stamped `MTUI_LONG_VERSION`, not a bare stub.
    assert!(
        stdout.contains('('),
        "expected build-provenance block in --version, got: {stdout:?}"
    );
    // `-V` renders identically to `--version`.
    let short = mtui_mcp().arg("-V").output().expect("run -V");
    assert!(short.status.success(), "-V must exit 0");
    assert_eq!(
        String::from_utf8_lossy(&short.stdout),
        stdout,
        "-V and --version must render identically"
    );
}
