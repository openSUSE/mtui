//! Guard for the `packaging.yml` CI job (`cargo package --workspace --exclude xtask
//! --locked --no-verify`): every shippable manifest pins the metadata a registry
//! publish requires, internal path deps pin the exact workspace version, the
//! root facade's `include` allowlist keeps repo-root clutter out of its tarball,
//! and no packaged path carries a `:` (which `cargo package` rejects outright).
//!
//! This is the fast local counterpart of that job: strip a description (or a
//! version pin) and this goes red the same way the CI job would. (The `=`-pin
//! lockstep itself is local policy the `--no-verify` job does not check;
//! only this test pins it.)

use std::path::{Path, PathBuf};

/// (manifest path relative to the workspace root, package name).
///
/// Descriptions are checked for presence, not pinned by value: each shippable
/// manifest carries its own `description` (a registry requirement), and this
/// guard asserts it is present and non-empty — plus, for the root facade,
/// short enough to survive as the `.deb` synopsis (see below). Pinning the
/// ten strings here a second time would mean every description edit touches
/// the test without buying any extra publish safety.
const SHIPPABLE: &[(&str, &str)] = &[
    ("Cargo.toml", "mtui"),
    ("crates/mtui-types/Cargo.toml", "mtui-types"),
    ("crates/mtui-config/Cargo.toml", "mtui-config"),
    ("crates/mtui-hosts/Cargo.toml", "mtui-hosts"),
    ("crates/mtui-datasources/Cargo.toml", "mtui-datasources"),
    ("crates/mtui-testreport/Cargo.toml", "mtui-testreport"),
    ("crates/mtui-core/Cargo.toml", "mtui-core"),
    ("crates/mtui-tui/Cargo.toml", "mtui-tui"),
    ("crates/mtui-cli/Cargo.toml", "mtui-cli"),
    ("crates/mtui-mcp/Cargo.toml", "mtui-mcp"),
];

/// Every `mtui-*` path dep in `[workspace.dependencies]`.
const INTERNAL: &[&str] = &[
    "mtui-cli",
    "mtui-config",
    "mtui-core",
    "mtui-datasources",
    "mtui-hosts",
    "mtui-mcp",
    "mtui-testreport",
    "mtui-tui",
    "mtui-types",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("workspace root")
}

fn manifest(rel: &str) -> toml::Value {
    let path = workspace_root().join(rel);
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    toml::from_str(&body).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

fn pkg<'a>(doc: &'a toml::Value, rel: &str) -> &'a toml::map::Map<String, toml::Value> {
    doc.get("package")
        .unwrap_or_else(|| panic!("{rel}: no [package]"))
        .as_table()
        .unwrap_or_else(|| panic!("{rel}: [package] is not a table"))
}

#[test]
fn every_shippable_manifest_has_its_description() {
    // Exhaustiveness: `members = ["crates/*", "xtask"]` is a glob, so a new
    // crate would silently bypass the loops below unless listed here
    // (`xtask` lives outside `crates/` and stays unpublished by
    // `xtask_stays_unpublished`).
    let root = workspace_root();
    let mut members: Vec<String> = ["Cargo.toml".to_owned()]
        .into_iter()
        .chain(
            std::fs::read_dir(root.join("crates"))
                .expect("reading crates/")
                .map(|e| e.expect("crates entry").path())
                .filter(|dir| dir.join("Cargo.toml").is_file())
                .map(|dir| {
                    dir.join("Cargo.toml")
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                }),
        )
        .collect();
    members.sort();
    let mut listed: Vec<String> = SHIPPABLE.iter().map(|(rel, _)| rel.to_string()).collect();
    listed.sort();
    assert_eq!(members, listed, "workspace member missing from SHIPPABLE");
    for (rel, name) in SHIPPABLE {
        let doc = manifest(rel);
        let pkg = pkg(&doc, rel);
        assert_eq!(
            pkg.get("name").and_then(|v| v.as_str()),
            Some(*name),
            "{rel}: unexpected package name"
        );
        let description = pkg
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{rel}: description missing"));
        assert!(
            !description.trim().is_empty(),
            "{rel}: description is empty"
        );
        // The facade description doubles as the `.deb` synopsis for both
        // distro packages (cargo-deb has no per-variant override): past 79
        // bytes cargo-deb word-wraps it (`split_by_chars(79)` in its control
        // renderer) and the overflow becomes the first line of the long
        // description, so the root text stays binary-neutral and short.
        if *rel == "Cargo.toml" {
            let bytes = description.len();
            assert!(
                bytes <= 79,
                "{rel}: description is {bytes} bytes, over the 79-byte cargo-deb synopsis wrap"
            );
        }
    }
}

#[test]
fn every_shippable_manifest_inherits_publish_metadata() {
    for (rel, _) in SHIPPABLE {
        let doc = manifest(rel);
        let pkg = pkg(&doc, rel);
        for key in [
            "version",
            "license",
            "homepage",
            "documentation",
            "readme",
            "repository",
            "keywords",
            "categories",
        ] {
            let inherited = pkg
                .get(key)
                .and_then(|v| v.as_table())
                .and_then(|t| t.get("workspace"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            assert!(
                inherited,
                "{rel}: [package] {key} is not workspace-inherited"
            );
        }
        assert!(
            doc.get("package").and_then(|p| p.get("publish")).is_none(),
            "{rel}: shippable crate sets publish"
        );
    }
}

#[test]
fn workspace_package_carries_common_metadata() {
    let doc = manifest("Cargo.toml");
    let ws = doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.as_table())
        .expect("root: no [workspace.package]");
    let str_field = |key: &str| {
        ws.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("root: [workspace.package] {key} missing"))
            .to_owned()
    };
    let version = str_field("version");
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "root: version {version:?} is not numeric"
    );
    assert_eq!(
        str_field("license"),
        "GPL-3.0-or-later",
        "root: license drifted"
    );
    for url in [
        str_field("repository"),
        str_field("homepage"),
        str_field("documentation"),
    ] {
        assert!(
            url.starts_with("https://"),
            "root: metadata URL {url:?} is not https"
        );
    }
    let readme = workspace_root().join(str_field("readme"));
    assert!(readme.is_file(), "{} is not a file", readme.display());
    assert_eq!(
        ws.get("keywords")
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("root: keywords missing"))
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>(),
        ["suse", "maintenance", "testing", "ssh", "qa"],
        "root: keywords drifted"
    );
    assert_eq!(
        ws.get("categories")
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("root: categories missing"))
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>(),
        ["command-line-utilities"],
        "root: categories drifted"
    );

    let deps = doc
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table())
        .expect("root: no [workspace.dependencies]");
    let want = format!("={version}");
    for key in deps.keys().filter(|k| k.starts_with("mtui-")) {
        assert!(
            INTERNAL.contains(&key.as_str()),
            "root: {key} missing from the test's INTERNAL list"
        );
    }
    for name in INTERNAL {
        let dep = deps
            .get(*name)
            .unwrap_or_else(|| panic!("root: [workspace.dependencies] has no {name}"));
        let table = dep
            .as_table()
            .unwrap_or_else(|| panic!("root: {name} dep is not a table"));
        assert!(
            table
                .get("path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| p.starts_with("crates/")),
            "root: {name} dep lost its crates/ path"
        );
        assert_eq!(
            table.get("version").and_then(|v| v.as_str()),
            Some(want.as_str()),
            "root: {name} dep is not pinned to {want}"
        );
    }
}

#[test]
fn xtask_stays_unpublished() {
    let doc = manifest("xtask/Cargo.toml");
    assert_eq!(
        pkg(&doc, "xtask/Cargo.toml")
            .get("publish")
            .and_then(|v| v.as_bool()),
        Some(false),
        "xtask: dev-only crate must keep publish = false"
    );
}

/// `cargo package` rejects `:` in packaged paths outright, so none may exist
/// anywhere a shippable manifest could pull it in. Walks the workspace root
/// (minus `.git`, `target/`, and the generated `docs/book/`, none of which
/// cargo packages) so a new top-level file cannot bypass the check.
#[test]
fn no_packaged_path_contains_colon() {
    fn walk(root: &Path, dir: &Path, excluded: &[PathBuf], bad: &mut Vec<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if excluded.contains(&path) {
                continue;
            }
            // Gitignored runtime template checkouts (`SUSE:*/` in `.gitignore`,
            // unanchored, so it matches at any depth): RRIDs contain `:` by
            // design, but `cargo package` never sees them, so neither does
            // this guard. Skipped at any depth since `template_dir` defaults
            // to `.` (the caller's CWD), so a run from a subdirectory leaves
            // a nested checkout behind.
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("SUSE:"))
            {
                continue;
            }
            // Repo-relative: the absolute checkout path itself may contain a
            // `:` (and Windows drive prefixes always do), which cargo does not
            // package and must not fail this guard.
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if rel.to_string_lossy().contains(':') {
                bad.push(rel.to_path_buf());
            }
            if path.is_dir() {
                walk(root, &path, excluded, bad);
            }
        }
    }

    let root = workspace_root();
    let excluded = [".git", "target", "docs/book"].map(|t| root.join(t));
    let mut bad = Vec::new();
    walk(&root, &root, &excluded, &mut bad);
    assert!(bad.is_empty(), "paths containing ':': {bad:#?}");
}

/// The root `mtui` facade lives at the repo root, so without an `include`
/// allowlist `cargo package` sweeps `.github/`, `.clusterfuzzlite/`,
/// `.cargo/`, `dist/`, `docs/` and friends into its tarball. The manifest
/// allowlists `src/**`, `tests/**`, `LICENSE`, `README.md`, `CHANGELOG.md`;
/// this runs the same `cargo package --list` the publish would and fails on
/// any emitted path outside that allowlist (plus cargo's automatic
/// `Cargo.toml` / `Cargo.toml.orig` / `Cargo.lock` / `.cargo_vcs_info.json`).
#[test]
fn root_package_ships_only_allowlisted_files() {
    let root = workspace_root();
    // `--allow-dirty`: the guard must stay green in a dev checkout with
    // unrelated uncommitted edits; the listing only reflects the manifest's
    // `include`, not the dirt. `--locked` mirrors the CI job so a drifted
    // lockfile fails here instead of silently updating locally.
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "package",
            "--list",
            "--allow-dirty",
            "--locked",
            "-p",
            "mtui",
        ])
        .current_dir(&root)
        .output()
        .expect("running cargo package --list");
    assert!(
        out.status.success(),
        "cargo package --list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = String::from_utf8(out.stdout).expect("package list is utf-8");
    assert!(
        !list.trim().is_empty(),
        "cargo package --list printed nothing"
    );
    let mut bad = Vec::new();
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if matches!(
            line,
            "Cargo.toml" | "Cargo.toml.orig" | "Cargo.lock" | ".cargo_vcs_info.json"
        ) {
            continue;
        }
        let allowed = line.starts_with("src/")
            || line.starts_with("tests/")
            || matches!(line, "LICENSE" | "README.md" | "CHANGELOG.md");
        if !allowed {
            bad.push(line.to_owned());
        }
    }
    assert!(
        bad.is_empty(),
        "root package leaks non-allowlisted files: {bad:#?}"
    );
    for want in [
        "src/bin/mtui.rs",
        "src/bin/mtui-mcp.rs",
        "tests/it.rs",
        "README.md",
        "LICENSE",
        "CHANGELOG.md",
    ] {
        assert!(
            list.lines().map(str::trim).any(|l| l == want),
            "root package is missing allowlisted {want}"
        );
    }
}
