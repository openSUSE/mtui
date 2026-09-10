//! Offline smoke test for the `xtask gen` artifact generation.
//!
//! Generates completions + man pages into an isolated temp dir (never the
//! checked-in `dist/`) and asserts every expected file exists, is non-empty, and
//! references its binary name. Structure — not exact bytes — is asserted, since
//! clap's generated output legitimately churns across clap minor versions. The
//! `checked_in_dist_is_up_to_date` drift guard below is the byte-exact
//! counterpart: `Cargo.lock` pins clap, so churn there is an explicit regen.

use std::path::{Path, PathBuf};

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
        // `MTUI_LONG_VERSION` must not leak in. The `--debug` *flag* legitimately
        // appears, so key on the provenance-specific `-dirty` / `, <profile>,`.
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
    for name in ["run", "update", "checkout", "openqa_overview", "config"] {
        assert!(
            doc.contains(&format!("## `{name}`")),
            "cli reference should document `{name}`"
        );
    }
    assert!(
        doc.contains("*Aliases:*") && doc.contains("`exit`") && doc.contains("`EOF`"),
        "cli reference should list command aliases"
    );
    // The shared template flags are documented once, in the preamble.
    assert!(doc.contains("`-T/--template <RRID>`") && doc.contains("`--all-templates`"));
}

/// The preamble's session-level list is hand-written, so pin it to the
/// registry: every command that addresses no template, and each of that
/// command's aliases, has to be named there. The per-command `*Aliases:*` line
/// `cli_reference_lists_known_commands_with_aliases` matches sits below the
/// preamble and cannot catch a gap here.
#[test]
fn cli_reference_preamble_names_every_session_level_command_and_alias() {
    let doc = render_cli_reference();
    let preamble = doc
        .split("\n## `")
        .next()
        .expect("the preamble precedes the first command heading");
    let registry = mtui_core::register_all();
    for name in registry.names() {
        let command = registry
            .get(name)
            .expect("registry.names() yields registered keys");
        if mtui_core::addresses_template(command.as_ref()) {
            continue;
        }
        assert!(
            preamble.contains(&format!("`{name}`")),
            "preamble omits the session-level command `{name}`"
        );
        for alias in command.aliases() {
            assert!(
                preamble.contains(&format!("`{alias}`")),
                "preamble omits `{name}`'s alias `{alias}`"
            );
        }
    }
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

/// The checked-in `dist/` tree, resolved relative to this crate's manifest.
fn checked_in_dist_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest dir has a parent (workspace root)")
        .join("dist")
}

/// Read every file under `root` into `(relative path, bytes)`, sorted.
fn read_tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if entry.file_type().expect("file type").is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("entry under root")
                    .to_path_buf();
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
                out.insert(rel, bytes);
            }
        }
    }
    out
}

/// Renders a drift-guard mismatch as a line diff instead of a byte dump.
/// Artifacts are UTF-8 text; non-UTF-8 input falls back to byte counts.
fn text_diff(old_name: &str, new_name: &str, stale: &[u8], fresh: &[u8]) -> String {
    let (Ok(old_text), Ok(new_text)) = (str::from_utf8(stale), str::from_utf8(fresh)) else {
        return format!(
            "non-UTF-8 artifacts differ ({} vs {} bytes)",
            stale.len(),
            fresh.len()
        );
    };
    let old: Vec<&str> = old_text.lines().collect();
    let new: Vec<&str> = new_text.lines().collect();
    let mut head = 0;
    while head < old.len().min(new.len()) && old[head] == new[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < (old.len() - head).min(new.len() - head)
        && old[old.len() - 1 - tail] == new[new.len() - 1 - tail]
    {
        tail += 1;
    }
    let removed = &old[head..old.len() - tail];
    let added = &new[head..new.len() - tail];
    if removed.is_empty() && added.is_empty() {
        return "line content matches; files differ outside lines (e.g. trailing newline)"
            .to_owned();
    }
    // One head/tail-trimmed hunk with context, each side capped.
    const CONTEXT: usize = 3;
    const MAX_SIDE: usize = 60;
    let cx0 = head.saturating_sub(CONTEXT);
    let old_end = (old.len() - tail + CONTEXT).min(old.len());
    let new_end = (new.len() - tail + CONTEXT).min(new.len());
    let mut out = format!(
        "--- {old_name}\n+++ {new_name}\n@@ -{},{} +{},{} @@\n",
        cx0 + 1,
        old_end - cx0,
        cx0 + 1,
        new_end - cx0,
    );
    for line in &old[cx0..head] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in removed.iter().take(MAX_SIDE) {
        out.push_str(&format!("-{line}\n"));
    }
    if removed.len() > MAX_SIDE {
        out.push_str(&format!(
            "… ({} more removed lines)\n",
            removed.len() - MAX_SIDE
        ));
    }
    for line in added.iter().take(MAX_SIDE) {
        out.push_str(&format!("+{line}\n"));
    }
    if added.len() > MAX_SIDE {
        out.push_str(&format!(
            "… ({} more added lines)\n",
            added.len() - MAX_SIDE
        ));
    }
    for line in &new[new.len() - tail..new_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

/// Drift guard: the committed `dist/completions` + `dist/man` must match what
/// `cargo xtask gen` produces. If this fails, the flag surface changed — run
/// `cargo xtask gen` and commit the result.
#[test]
fn checked_in_dist_is_up_to_date() {
    let dir = tempfile::tempdir().expect("tempdir");
    generate_into(dir.path()).expect("generation succeeds");

    for sub in ["completions", "man"] {
        let fresh = read_tree(&dir.path().join(sub));
        let checked_in = checked_in_dist_dir().join(sub);
        let on_disk = read_tree(&checked_in);
        for (rel, bytes) in &fresh {
            match on_disk.get(rel) {
                None => panic!(
                    "{} is generated but not checked in; run `cargo xtask gen` and commit the result",
                    checked_in.join(rel).display()
                ),
                Some(stale) => {
                    if stale != bytes {
                        panic!(
                            "{} is stale; run `cargo xtask gen` and commit the result\n{}",
                            checked_in.join(rel).display(),
                            text_diff("checked-in", "generated", stale, bytes)
                        );
                    }
                }
            }
        }
        for rel in on_disk.keys() {
            assert!(
                fresh.contains_key(rel),
                "{} is checked in but no longer generated; run `cargo xtask gen` and commit the result",
                checked_in.join(rel).display()
            );
        }
    }
}

/// The drift-guard diff lists the changed lines, not byte decimals.
#[test]
fn text_diff_lists_differing_lines() {
    let diff = text_diff("old", "new", b"a\nb\nc\n", b"a\nB\nc\n");
    assert!(diff.contains("-b"), "removed line missing:\n{diff}");
    assert!(diff.contains("+B"), "added line missing:\n{diff}");
    assert!(!diff.contains("98"), "byte dump leaked:\n{diff}");
}

/// Non-UTF-8 artifacts fall back to byte counts instead of a string diff.
#[test]
fn text_diff_non_utf8_falls_back_to_byte_counts() {
    let diff = text_diff("old", "new", &[0xff, 0xfe], &[0x00]);
    assert!(
        diff.contains("2 vs 1"),
        "byte-count fallback missing:\n{diff}"
    );
}

/// A trailing-newline-only drift still explains itself.
#[test]
fn text_diff_trailing_newline_only_is_explained() {
    let diff = text_diff("old", "new", b"a\n", b"a");
    assert!(diff.contains("trailing newline"), "unexplained:\n{diff}");
}

// --- Release packaging --------------------------------------------------------

/// Build a minimal fixture tree under `root`, returning `(bin_dir, dist_dir)`.
/// Mirrors the layout `stage_package` reads:
/// `target/<triple>/release/{mtui,mtui-mcp}`, `dist/{completions,man,vim-plugin}` and
/// `LICENSE`/`README.md` at the root.
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

/// Exercises the full archive path through the `tar` + `sha256sum` subprocesses,
/// skipping gracefully when one is missing rather than failing spuriously.
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
