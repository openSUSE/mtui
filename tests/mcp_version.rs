//! Smoke test for the `mtui-mcp` binary's `--version` surface.
//!
//! The second-binary counterpart of
//! `cli_smoke.rs::version_prints_provenance_block_and_exits_zero`: drive the
//! built `mtui-mcp` via `CARGO_BIN_EXE_mtui-mcp` and assert the
//! `mtui-mcp <ver> (<ref>, <profile>, <target>)` block and a clean exit. Shape
//! and presence only — never the exact SHA or version, which churn.
//!
//! Gated behind the `mcp` feature: the `mtui-mcp` binary only exists then.

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
    // clap renders `<bin-name> <long_version>`. Assert the crate version via
    // env! rather than a literal, so a bump does not break this.
    assert!(
        stdout.contains(&format!("mtui-mcp {}", env!("CARGO_PKG_VERSION"))),
        "expected version string, got: {stdout:?}"
    );
    // The paren proves this is the stamped `MTUI_LONG_VERSION`, not a bare stub.
    assert!(
        stdout.contains('('),
        "expected build-provenance block in --version, got: {stdout:?}"
    );
    // With a git ref present (`<ref>, <profile>, <target>`), it must not name a
    // tag outside this crate's `MAJOR.MINOR` series; a bare SHA is always fine.
    let block = stdout
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(block, _)| block)
        .expect("provenance block");
    let fields: Vec<&str> = block.split(", ").collect();
    if let [git_ref, _, _] = fields[..] {
        assert!(
            !git_ref.contains('/'),
            "ref names a foreign tag: {git_ref:?}"
        );
        if git_ref.contains("-g") {
            let series = format!(
                "{}.{}.",
                env!("CARGO_PKG_VERSION_MAJOR"),
                env!("CARGO_PKG_VERSION_MINOR")
            );
            assert!(
                git_ref.starts_with(&series),
                "tag-form ref must be in the {series}* series, got: {git_ref:?}"
            );
        }
    }
    // `-V` renders identically to `--version`.
    let short = mtui_mcp().arg("-V").output().expect("run -V");
    assert!(short.status.success(), "-V must exit 0");
    assert_eq!(
        String::from_utf8_lossy(&short.stdout),
        stdout,
        "-V and --version must render identically"
    );
}
