//! Integration coverage for the top-level [`Args`](mtui_core::Args) parser.
//!
//! Unit-level parsing (flag acceptance, `-a`/`-k` exclusion, `Sut` formatting)
//! lives colocated in `src/args.rs`. Here we lock the two *text contracts* a
//! downstream shell and bug reporter depend on: the `--help` output (snapshotted
//! with `insta`) and the `--version` line — which carries build provenance
//! (git SHA, profile, target), asserted by shape/presence so it neither goes
//! stale with a version bump nor churns on every commit's SHA.

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
fn version_carries_build_provenance() {
    // `--version` prints `mtui <ver> (<sha>[-dirty], <profile>, <target>)`.
    // Upstream listed separately-installed runtime dep versions; a static binary
    // has no such drift, so this reports build provenance instead — the thing
    // that varies for an out-of-tree build. We assert shape/presence, never the
    // exact SHA (changes every commit) nor exact versions (brittle on bumps).
    let out = rendered(&["--version"]);
    let line = out.trim_end();

    // App version is always first.
    assert!(
        line.starts_with(&format!("mtui {} (", env!("CARGO_PKG_VERSION"))),
        "expected leading `mtui <ver> (`, got {line:?}"
    );
    assert!(line.ends_with(')'), "expected trailing `)`, got {line:?}");

    // The parenthesised provenance block: profile + target are always present;
    // sha is present in a git checkout (as it is in CI) and omitted otherwise —
    // so we assert on the fields that are unconditional.
    let block = line
        .split_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .expect("provenance block delimited by parens");
    let fields: Vec<&str> = block.split(", ").collect();
    assert!(
        fields.len() == 2 || fields.len() == 3,
        "expected `(<sha>, )?<profile>, <target>`, got {block:?}"
    );

    // Profile is the second-to-last field, target the last; both non-empty.
    let target = fields.last().unwrap();
    let profile = fields[fields.len() - 2];
    assert!(!profile.is_empty(), "profile field empty in {block:?}");
    // A target triple looks like `<arch>-<vendor>-<os>[-<abi>]` — contains `-`.
    assert!(
        target.contains('-'),
        "target field {target:?} does not look like a triple"
    );

    // `-V` renders identically to `--version`.
    assert_eq!(rendered(&["-V"]), out);
}
