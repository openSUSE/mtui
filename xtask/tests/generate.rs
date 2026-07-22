//! Offline smoke test for the `xtask gen` artifact generation.
//!
//! Generates completions + man pages into an isolated temp dir (never the
//! checked-in `dist/`) and asserts every expected file exists, is non-empty, and
//! references its binary name. Structure — not exact bytes — is asserted, since
//! clap's generated output legitimately churns across clap minor versions.

use std::path::Path;

use xtask::{
    BINARIES, CLI_REFERENCE_FILE, INVOCATION_REFERENCE_FILE, PackageArgs, PackageInputs,
    generate_docs_into, generate_into, package_stem, package_target, render_cli_reference,
    render_invocation_reference, stage_package,
};

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

/// A checked-in generated doc page, resolved relative to this crate's manifest.
fn checked_in_docs_page(file: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest dir has a parent (workspace root)")
        .join("docs")
        .join("src")
        .join(file)
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
    let first_cli = std::fs::read(dir.path().join(CLI_REFERENCE_FILE)).unwrap();
    let first_inv = std::fs::read(dir.path().join(INVOCATION_REFERENCE_FILE)).unwrap();
    generate_docs_into(dir.path()).expect("second run");
    let second_cli = std::fs::read(dir.path().join(CLI_REFERENCE_FILE)).unwrap();
    let second_inv = std::fs::read(dir.path().join(INVOCATION_REFERENCE_FILE)).unwrap();
    assert_eq!(
        first_cli, second_cli,
        "re-running gen-docs must be byte-identical"
    );
    assert_eq!(
        first_inv, second_inv,
        "re-running gen-docs must be byte-identical"
    );
}

/// The invocation reference documents both binaries and their key flags, drawn
/// straight from the clap parsers.
#[test]
fn invocation_reference_documents_both_binaries() {
    let doc = render_invocation_reference();
    assert!(doc.contains("## `mtui`"), "documents the mtui binary");
    assert!(
        doc.contains("## `mtui-mcp`"),
        "documents the mtui-mcp binary"
    );
    // Representative flags from each parser (hyphenated long forms).
    for flag in [
        "--auto-review-id",
        "--kernel-review-id",
        "--sut",
        "--config",
        "--connection-timeout",
    ] {
        assert!(doc.contains(flag), "invocation ref should document {flag}");
    }
    for flag in ["--transport", "--host", "--port"] {
        assert!(doc.contains(flag), "invocation ref should document {flag}");
    }
    // The REPL-only caveat is stated.
    assert!(
        doc.contains("REPL-only"),
        "invocation ref should note mtui has no single-command mode"
    );
}

/// Drift guard: the committed `docs/src/{cli,invocation}.md` must match what the
/// generators produce. If this fails, the command/flag surface changed — run
/// `cargo xtask gen-docs` and commit the result.
#[test]
fn checked_in_generated_docs_are_up_to_date() {
    for (file, generated) in [
        (CLI_REFERENCE_FILE, render_cli_reference()),
        (INVOCATION_REFERENCE_FILE, render_invocation_reference()),
    ] {
        let path = checked_in_docs_page(file);
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        assert_eq!(
            on_disk,
            generated,
            "{} is stale; run `cargo xtask gen-docs` and commit the result",
            path.display()
        );
    }
}

// --- Release packaging --------------------------------------------------------

/// Build a minimal fixture tree (fake binaries, `dist/`, root files) under `root`
/// and return `(bin_dir, dist_dir)`. Mirrors the real layout `stage_package`
/// reads: `target/<triple>/release/{mtui,mtui-mcp}`, `dist/{completions,man,terms}`,
/// and `LICENSE`/`README.md` at the root.
fn make_fixture(root: &Path, target: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let bin_dir = root.join("target").join(target).join("release");
    std::fs::create_dir_all(&bin_dir).unwrap();
    for bin in BINARIES {
        std::fs::write(bin_dir.join(bin), b"#!/bin/sh\n").unwrap();
    }

    let dist = root.join("dist");
    std::fs::create_dir_all(dist.join("completions").join("bash")).unwrap();
    std::fs::write(
        dist.join("completions").join("bash").join("mtui.bash"),
        b"# c",
    )
    .unwrap();
    std::fs::create_dir_all(dist.join("man")).unwrap();
    std::fs::write(dist.join("man").join("mtui.1"), b".TH mtui 1").unwrap();
    std::fs::create_dir_all(dist.join("terms")).unwrap();
    std::fs::write(dist.join("terms").join("term.xterm.sh"), b"#!/bin/sh").unwrap();
    std::fs::create_dir_all(dist.join("vim-plugin").join("ftdetect")).unwrap();
    std::fs::write(
        dist.join("vim-plugin")
            .join("ftdetect")
            .join("testreport.vim"),
        b"\" ftdetect",
    )
    .unwrap();

    std::fs::write(root.join("LICENSE"), b"license").unwrap();
    std::fs::write(root.join("README.md"), b"readme").unwrap();
    (bin_dir, dist)
}

#[test]
fn package_stem_is_versioned_and_targeted() {
    assert_eq!(
        package_stem("v1.2.0", "x86_64-unknown-linux-musl"),
        "mtui-v1.2.0-x86_64-unknown-linux-musl"
    );
}

#[test]
fn stage_package_lays_out_documented_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = "x86_64-unknown-linux-musl";
    let (bin_dir, dist) = make_fixture(root, target);
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let inputs = PackageInputs {
        version: "v9.9.9",
        target,
        bin_dir: &bin_dir,
        dist_dir: &dist,
        root_dir: root,
        out_dir: &out,
    };
    let staging = stage_package(&inputs).expect("stage");

    assert_eq!(staging, out.join("mtui-v9.9.9-x86_64-unknown-linux-musl"));
    for bin in BINARIES {
        assert!(staging.join(bin).is_file(), "{bin} missing from staging");
    }
    assert!(
        staging
            .join("completions")
            .join("bash")
            .join("mtui.bash")
            .is_file()
    );
    assert!(staging.join("man").join("mtui.1").is_file());
    assert!(staging.join("terms").join("term.xterm.sh").is_file());
    assert!(
        staging
            .join("vim-plugin")
            .join("ftdetect")
            .join("testreport.vim")
            .is_file()
    );
    assert!(staging.join("LICENSE").is_file());
    assert!(staging.join("README.md").is_file());
}

#[cfg(unix)]
#[test]
fn stage_package_marks_binaries_executable() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = "aarch64-unknown-linux-musl";
    let (bin_dir, dist) = make_fixture(root, target);
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let inputs = PackageInputs {
        version: "v1.0.0",
        target,
        bin_dir: &bin_dir,
        dist_dir: &dist,
        root_dir: root,
        out_dir: &out,
    };
    let staging = stage_package(&inputs).unwrap();
    for bin in BINARIES {
        let mode = std::fs::metadata(staging.join(bin))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "{bin} is not executable");
    }
}

#[test]
fn stage_package_is_idempotent_and_clears_stale() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = "x86_64-unknown-linux-musl";
    let (bin_dir, dist) = make_fixture(root, target);
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let inputs = PackageInputs {
        version: "v1.0.0",
        target,
        bin_dir: &bin_dir,
        dist_dir: &dist,
        root_dir: root,
        out_dir: &out,
    };

    let staging = stage_package(&inputs).unwrap();
    // Drop a stale file that a second run must remove.
    std::fs::write(staging.join("STALE"), b"x").unwrap();
    let staging2 = stage_package(&inputs).unwrap();
    assert_eq!(staging, staging2);
    assert!(
        !staging2.join("STALE").exists(),
        "stale file survived re-stage"
    );
}

/// `tar` + `sha256sum` are shelled out; both exist on CI runners, openSUSE, and
/// macOS dev boxes, so exercise the full archive path. Skips gracefully if a tool
/// is missing (e.g. a minimal container) rather than failing spuriously.
#[test]
fn package_target_produces_tarball_and_checksum() {
    if !have("tar") || !have("sha256sum") {
        eprintln!("skipping: tar/sha256sum not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = "x86_64-unknown-linux-musl";
    let (bin_dir, dist) = make_fixture(root, target);
    let out = root.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let inputs = PackageInputs {
        version: "v2.3.4",
        target,
        bin_dir: &bin_dir,
        dist_dir: &dist,
        root_dir: root,
        out_dir: &out,
    };
    let tarball = package_target(&inputs).expect("package");
    assert_eq!(
        tarball.file_name().unwrap(),
        "mtui-v2.3.4-x86_64-unknown-linux-musl.tar.gz"
    );
    assert!(tarball.is_file(), "tarball not created");

    let sum = out.join("mtui-v2.3.4-x86_64-unknown-linux-musl.tar.gz.sha256");
    assert!(sum.is_file(), "checksum not created");
    let sum_body = std::fs::read_to_string(&sum).unwrap();
    assert!(
        sum_body.contains("mtui-v2.3.4-x86_64-unknown-linux-musl.tar.gz"),
        "checksum names the tarball"
    );

    // `sha256sum -c` from the artifact dir validates the recorded hash.
    let check = std::process::Command::new("sha256sum")
        .arg("-c")
        .arg(sum.file_name().unwrap())
        .current_dir(&out)
        .output()
        .unwrap();
    assert!(check.status.success(), "sha256sum -c failed: {check:?}");
}

fn have(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn args(items: &[&str]) -> impl Iterator<Item = String> {
    items
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<Vec<_>>()
        .into_iter()
}

#[test]
fn package_args_parse_required_only() {
    let a = PackageArgs::parse(args(&["--version", "v1.0.0", "--target", "t"])).unwrap();
    assert_eq!(a.version, "v1.0.0");
    assert_eq!(a.target, "t");
    assert!(a.bin_dir.is_none());
    assert!(a.out_dir.is_none());
}

#[test]
fn package_args_parse_all_overrides() {
    let a = PackageArgs::parse(args(&[
        "--version",
        "v2",
        "--target",
        "t",
        "--bin-dir",
        "/b",
        "--out-dir",
        "/o",
    ]))
    .unwrap();
    assert_eq!(a.bin_dir.unwrap(), std::path::Path::new("/b"));
    assert_eq!(a.out_dir.unwrap(), std::path::Path::new("/o"));
}

#[test]
fn package_args_missing_required_errors() {
    assert!(PackageArgs::parse(args(&["--target", "t"])).is_err());
    assert!(PackageArgs::parse(args(&["--version", "v"])).is_err());
}

#[test]
fn package_args_unknown_flag_and_missing_value_error() {
    assert!(PackageArgs::parse(args(&["--bogus", "x"])).is_err());
    // `--version` with no following value.
    assert!(PackageArgs::parse(args(&["--version"])).is_err());
}
