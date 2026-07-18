//! Offline smoke test for the `xtask gen` artifact generation.
//!
//! Generates completions + man pages into an isolated temp dir (never the
//! checked-in `dist/`) and asserts every expected file exists, is non-empty, and
//! references its binary name. Structure — not exact bytes — is asserted, since
//! clap's generated output legitimately churns across clap minor versions.

use std::path::Path;

use xtask::{CLI_REFERENCE_FILE, generate_docs_into, generate_into, render_cli_reference};

/// The two binaries and their per-shell completion file names, plus the man page.
/// clap names bash `<bin>.bash`, fish `<bin>.fish`, and zsh `_<bin>`.
struct Expected {
    bin: &'static str,
    bash: &'static str,
    fish: &'static str,
    zsh: &'static str,
}

const BINS: [Expected; 2] = [
    Expected {
        bin: "mtui",
        bash: "mtui.bash",
        fish: "mtui.fish",
        zsh: "_mtui",
    },
    Expected {
        bin: "mtui-mcp",
        bash: "mtui-mcp.bash",
        fish: "mtui-mcp.fish",
        zsh: "_mtui-mcp",
    },
];

fn assert_nonempty_contains(path: &Path, needle: &str) {
    let body =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    assert!(!body.trim().is_empty(), "{} is empty", path.display());
    assert!(
        body.contains(needle),
        "{} does not mention {needle:?}",
        path.display()
    );
}

#[test]
fn generate_into_emits_all_completions_and_man_pages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dist = dir.path();

    generate_into(dist).expect("generation succeeds");

    let completions = dist.join("completions");
    let man = dist.join("man");
    for e in &BINS {
        assert_nonempty_contains(&completions.join("bash").join(e.bash), e.bin);
        assert_nonempty_contains(&completions.join("fish").join(e.fish), e.bin);
        assert_nonempty_contains(&completions.join("zsh").join(e.zsh), e.bin);
        // Man page carries the pinned crate version, not build-provenance.
        let man_page = man.join(format!("{}.1", e.bin));
        assert_nonempty_contains(&man_page, e.bin);
        let body = std::fs::read_to_string(&man_page).unwrap();
        assert!(
            body.contains(env!("CARGO_PKG_VERSION")),
            "{} should carry the stable crate version",
            man_page.display()
        );
        // The build-provenance version (`MTUI_LONG_VERSION`) wraps the sha/target
        // in parens and marks a dirty tree with `-dirty`; neither must leak into
        // the committed man page. (The `--debug` *flag* legitimately appears, so
        // we key on the provenance-specific `-dirty` / `, <profile>,` shapes.)
        assert!(
            !body.contains("\\-dirty")
                && !body.contains(", debug,")
                && !body.contains(", release,"),
            "{} must not embed build-provenance (sha/dirty/profile)",
            man_page.display()
        );
    }
}

#[test]
fn generate_into_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dist = dir.path();

    generate_into(dist).expect("first run");
    let first = std::fs::read(dist.join("man").join("mtui.1")).unwrap();
    generate_into(dist).expect("second run");
    let second = std::fs::read(dist.join("man").join("mtui.1")).unwrap();

    assert_eq!(first, second, "re-running gen must be byte-identical");
}

/// The checked-in CLI reference, resolved relative to this crate's manifest.
fn checked_in_cli_reference() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest dir has a parent (workspace root)")
        .join("docs")
        .join("src")
        .join(CLI_REFERENCE_FILE)
}

#[test]
fn cli_reference_lists_known_commands_with_aliases() {
    let doc = render_cli_reference();
    // A representative command from each wave appears as a section.
    for name in ["run", "update", "checkout", "openqa_overview", "config"] {
        assert!(
            doc.contains(&format!("## `{name}`")),
            "cli reference should document `{name}`"
        );
    }
    // `quit`'s REPL aliases are surfaced.
    assert!(
        doc.contains("*Aliases:*") && doc.contains("`exit`") && doc.contains("`EOF`"),
        "cli reference should list command aliases"
    );
    // The shared template flags are documented once, in the preamble.
    assert!(doc.contains("`-T/--template <RRID>`") && doc.contains("`--all-templates`"));
}

#[test]
fn generate_docs_into_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate_docs_into(dir.path()).expect("first run");
    let first = std::fs::read(dir.path().join(CLI_REFERENCE_FILE)).unwrap();
    generate_docs_into(dir.path()).expect("second run");
    let second = std::fs::read(dir.path().join(CLI_REFERENCE_FILE)).unwrap();
    assert_eq!(first, second, "re-running gen-docs must be byte-identical");
}

/// Drift guard: the committed `docs/src/cli.md` must match what the registry
/// generates. If this fails, the command surface changed — run
/// `cargo xtask gen-docs` and commit the result.
#[test]
fn checked_in_cli_reference_is_up_to_date() {
    let path = checked_in_cli_reference();
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let generated = render_cli_reference();
    assert_eq!(
        on_disk,
        generated,
        "{} is stale; run `cargo xtask gen-docs` and commit the result",
        path.display()
    );
}
