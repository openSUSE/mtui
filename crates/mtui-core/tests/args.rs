//! Integration coverage for the top-level [`Args`](mtui_core::Args) parser.
//!
//! Unit-level parsing (flag acceptance, `-a`/`-k` exclusion, `Sut` formatting)
//! lives colocated in `src/args.rs`. Here we lock the two *text contracts* a
//! downstream shell and bug reporter depend on: the `--help` output (snapshotted
//! with `insta`) and the `--version` line (asserted against the crate version so
//! it never goes stale with a bump).

use clap::Parser;
use mtui_core::Args;

/// Renders the message clap produces for a help/version request, which it
/// carries as an "error" from `try_parse_from` (kind `DisplayHelp` /
/// `DisplayVersion`) rather than exiting the process.
fn rendered(argv: &[&str]) -> String {
    let mut full = vec!["mtui"];
    full.extend_from_slice(argv);
    match Args::try_parse_from(full) {
        Ok(_) => panic!("expected help/version display, got a successful parse"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn help_text_contract() {
    // Snapshot the whole --help surface: flag names, value hints, and about
    // text. A change here is a deliberate CLI-contract change and must be
    // reviewed via `cargo insta review`.
    insta::assert_snapshot!("help", rendered(&["--help"]));
}

#[test]
fn version_is_app_version_only() {
    // Intentional deviation from upstream's multi-dependency version block: this
    // prints only `mtui <version>`. The curated dep-version block is a follow-up.
    let expected = format!("mtui {}\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(rendered(&["--version"]), expected);
}
