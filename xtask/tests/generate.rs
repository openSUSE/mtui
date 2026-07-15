//! Offline smoke test for the `xtask gen` artifact generation.
//!
//! Generates completions + man pages into an isolated temp dir (never the
//! checked-in `dist/`) and asserts every expected file exists, is non-empty, and
//! references its binary name. Structure — not exact bytes — is asserted, since
//! clap's generated output legitimately churns across clap minor versions.

use std::path::Path;

use xtask::generate_into;

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
