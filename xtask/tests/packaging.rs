//! Guard for the `packaging.yml` CI job (`cargo package --workspace --exclude xtask
//! --locked --no-verify`): every shippable manifest pins the metadata a registry
//! publish requires, internal path deps pin the exact workspace version, the
//! root facade's `include` allowlist keeps repo-root clutter out of its tarball,
//! every publishable name is one we own (`suse-mtui` or `mtui-*`),
//! and no packaged path carries a `:` (which `cargo package` rejects outright).
//!
//! This is the fast local counterpart of that job: strip a description (or a
//! version pin) and this goes red the same way the CI job would. (The `=`-pin
//! lockstep itself is local policy the `--no-verify` job does not check;
//! only this test pins it.)
//!
//! `distro_package_names_stay_split` is the fast counterpart of the same
//! file's `distro-packages` job (which builds both deb variants and both rpms
//! and asserts their `Name:` fields): rename a distro package and this goes
//! red in seconds instead of after a full tool install plus build.
//! `build_scripts_ignore_foreign_git_repos` pins the `build.rs` version-stamp
//! guard the same way: a packaged crate must never stamp a foreign checkout's
//! SHA into `--version`.
//! `crates_with_foreign_test_fixtures_exclude_tests` plus
//! `shipped_sources_reference_only_shipped_files` pin the tarball
//! self-containment P3.2 settled: integration tests that `include_str!`
//! fixtures from outside their package do not ship, and every other shipped
//! source only references files that ship with it — whether read at compile
//! time (`include_str!`) or at run time (`CARGO_MANIFEST_DIR` joins) — so
//! `cargo test` from a crates.io tarball compiles. (`cargo publish`
//! verification only builds lib and bins, so this never blocked publishing.)

use std::path::{Component, Path, PathBuf};

/// (manifest path relative to the workspace root, package name).
///
/// Descriptions are checked for presence, not pinned by value: each shippable
/// manifest carries its own `description` (a registry requirement), and this
/// guard asserts it is present and non-empty — plus, for the root facade,
/// short enough to survive as the `.deb` synopsis (see below). Pinning the
/// ten strings here a second time would mean every description edit touches
/// the test without buying any extra publish safety.
const SHIPPABLE: &[(&str, &str)] = &[
    ("Cargo.toml", "suse-mtui"),
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

/// Every publishable crate owns its crates.io name: `suse-mtui` for the facade
/// (`mtui` belongs to an unrelated project), `mtui-*` for the libraries.
/// Offline allowlist, not a live crates.io ownership query — a networked check
/// would flake the gate on registry availability.
#[test]
fn every_publishable_crate_has_an_owned_name() {
    for rel in SHIPPABLE
        .iter()
        .map(|(rel, _)| *rel)
        .chain(["xtask/Cargo.toml"])
    {
        let doc = manifest(rel);
        let pkg = pkg(&doc, rel);
        if pkg
            .get("publish")
            .and_then(|v| v.as_bool())
            .is_some_and(|p| !p)
        {
            continue;
        }
        let name = pkg
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{rel}: [package] name missing"));
        assert!(
            name == "suse-mtui" || name.starts_with("mtui-"),
            "{rel}: publishable name {name:?} is not one we own"
        );
    }
}

/// The `[package.metadata.deb]` / `[package.metadata.generate-rpm]` distro
/// names stay `mtui` / `mtui-mcp`: the crates.io rename only changed the
/// registry name, and a slip here fails the release after the tag exists —
/// hence the PR-time `distro-packages` job. This is its seconds-fast local
/// counterpart: rename any of the four and this goes red.
#[test]
fn distro_package_names_stay_split() {
    let doc = manifest("Cargo.toml");
    let meta = doc
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.as_table())
        .expect("root: no [package.metadata]");
    let table = |tool: &str| {
        meta.get(tool)
            .and_then(|t| t.as_table())
            .unwrap_or_else(|| panic!("root: no [package.metadata.{tool}]"))
    };
    let variant = |tool: &str, name: &str| {
        table(tool)
            .get("variants")
            .and_then(|v| v.as_table())
            .and_then(|v| v.get("mcp"))
            .and_then(|m| m.as_table())
            .and_then(|m| m.get(name))
            .unwrap_or_else(|| panic!("root: [{tool}.variants.mcp] has no {name}"))
    };
    for tool in ["deb", "generate-rpm"] {
        assert_eq!(
            table(tool).get("name").and_then(|v| v.as_str()),
            Some("mtui"),
            "root: [{tool}] name drifted from the mtui distro package"
        );
        assert_eq!(
            variant(tool, "name").as_str(),
            Some("mtui-mcp"),
            "root: [{tool}.variants.mcp] name drifted from the mtui-mcp distro package"
        );
    }
}

/// `mtui-core` and `mtui-mcp` stamp `git describe` into `--version` at build
/// time. Built from a crates.io tarball unpacked inside an unrelated git
/// checkout (a dotfiles `$HOME`), the lookup would stamp that repository's
/// SHA — and `rerun-if-changed=../../.git/HEAD` points outside the package,
/// rerunning the script on every build. Both build scripts therefore skip the
/// git lookup when `.cargo_vcs_info.json` sits beside the manifest (cargo
/// adds it to every packaged crate) and when git's toplevel is not the
/// workspace root.
///
/// Proven the way CI cannot (`cargo package --no-verify` never compiles the
/// tarball): each real `build.rs` is copied into a scratch crate at the same
/// `crates/<name>/` depth and built under a fake `$HOME`-that-is-a-git-repo,
/// both with and without the vcs marker, plus once in its own checkout as a
/// control. The `ws-packaged` case is silenced by either guard, so it cannot
/// pin the marker: the `""` case builds at the foreign checkout's own
/// toplevel, where only the marker silences the lookup. Break either guard
/// and the foreign cases go red.
#[test]
fn build_scripts_ignore_foreign_git_repos() {
    for krate in ["mtui-core", "mtui-mcp"] {
        let foreign = FakeHome::new();
        // Packaged crate inside a foreign checkout: the vcs marker alone
        // must silence the lookup.
        let out = foreign.build_scratch(krate, "ws-packaged", true);
        foreign.assert_unstamped(krate, &out);
        foreign.assert_no_rerun_directive(krate, &foreign.build_directives("ws-packaged"));
        // Unpackaged sources inside a foreign checkout (a vendored tree, a
        // tarball with the marker stripped): the toplevel check must silence it.
        let out = foreign.build_scratch(krate, "ws-bare", false);
        foreign.assert_unstamped(krate, &out);
        foreign.assert_watches_git_head(krate, &foreign.build_directives("ws-bare"));
        // Packaged crate at the foreign checkout's own toplevel: git's
        // toplevel equals the build root, so only the marker can silence the
        // lookup — disable it and this case stamps the foreign SHA.
        let out = foreign.build_scratch(krate, "", true);
        foreign.assert_unstamped(krate, &out);
        foreign.assert_no_rerun_directive(krate, &foreign.build_directives(""));
    }
}

/// Control for the above: in its own checkout the lookup still stamps, so the
/// guards cannot be "fixed" by deleting the git call.
#[test]
fn build_scripts_stamp_their_own_checkout() {
    for krate in ["mtui-core", "mtui-mcp"] {
        let home = FakeHome::new();
        // Scratch crate straight under the repo root, so git's toplevel is
        // the build root itself.
        let out = home.build_scratch(krate, "", false);
        assert!(
            out.contains(&home.short),
            "{krate}: own checkout no longer stamps (got {out:?})"
        );
        home.assert_watches_git_head(krate, &home.build_directives(""));
    }
}

/// A git repository with one commit, standing in for a foreign dotfiles
/// `$HOME` (or an own checkout when `new_at` puts the repo at the build root).
struct FakeHome {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    full: String,
    short: String,
}

impl FakeHome {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_owned();
        std::fs::create_dir_all(&root).expect("mkdir fake home");
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("marker"), "foreign dotfiles repo").expect("marker");
        git(&root, &["add", "marker"]);
        git(
            &root,
            &[
                "-c",
                "user.name=guard",
                "-c",
                "user.email=guard@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "foreign",
            ],
        );
        let full = git_out(&root, &["rev-parse", "HEAD"]);
        let short = git_out(&root, &["rev-parse", "--short=12", "HEAD"]);
        Self {
            _tmp: tmp,
            root,
            full,
            short,
        }
    }

    /// Copy the repo's real `crates/<krate>/build.rs` into a scratch crate at
    /// the same `crates/<name>/` depth under `<root>/<ws>/`, build it, and
    /// return the binary's printed `MTUI_LONG_VERSION`.
    fn build_scratch(&self, krate: &str, ws: &str, packaged: bool) -> String {
        let member = self.root.join(ws).join("crates").join("pkg");
        std::fs::create_dir_all(member.join("src")).expect("mkdir scratch crate");
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("scratch manifest");
        std::fs::write(
            member.join("src/main.rs"),
            "fn main() { println!(\"{}\", env!(\"MTUI_LONG_VERSION\")); }\n",
        )
        .expect("scratch main");
        let real = workspace_root().join("crates").join(krate).join("build.rs");
        std::fs::copy(&real, member.join("build.rs")).expect("copying build.rs");
        if packaged {
            std::fs::write(member.join(".cargo_vcs_info.json"), "{}\n").expect("vcs marker");
        }
        // The member dir is reused across krate iterations with a different
        // `build.rs` each time: start from a clean target dir so the
        // `build_directives` lookup below finds exactly one script output.
        let _ = std::fs::remove_dir_all(member.join("target"));
        let out = std::process::Command::new(env!("CARGO"))
            .args(["build", "--offline"])
            .current_dir(&member)
            // Isolate the nested build from the outer cargo invocation
            // (jobserver fds, wrappers, target-dir redirects) and the network.
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("MAKEFLAGS")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env("CARGO_NET_OFFLINE", "true")
            .env("CARGO_TARGET_DIR", member.join("target"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("nested cargo build");
        assert!(
            out.status.success(),
            "{krate}: scratch build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bin = member
            .join("target")
            .join("debug")
            .join(format!("pkg{}", std::env::consts::EXE_SUFFIX));
        let run = std::process::Command::new(&bin)
            .output()
            .expect("running scratch binary");
        assert!(run.status.success(), "{krate}: scratch binary failed");
        String::from_utf8(run.stdout)
            .expect("version is utf-8")
            .trim()
            .to_owned()
    }

    /// The stamp must be the bare `0.0.0 (<profile>, <target>)` form, never
    /// the foreign checkout's SHA.
    fn assert_unstamped(&self, krate: &str, out: &str) {
        assert!(
            out.starts_with("0.0.0 ("),
            "{krate}: unexpected version shape {out:?}"
        );
        assert!(
            !out.contains(&self.short) && !out.contains(&self.full),
            "{krate}: stamped the foreign repo {out:?}"
        );
    }

    /// The build script's captured stdout (`cargo:` directives) for the
    /// scratch crate built by [`build_scratch`](Self::build_scratch): cargo
    /// saves it as an `output` file under `target/debug/build/`. (Recent
    /// cargo splits the compiled script and its execution output across two
    /// hashed dirs, so this collects every `output` file rather than
    /// assuming a single `pkg-*` dir.)
    fn build_directives(&self, ws: &str) -> String {
        let build = self.root.join(ws).join("crates/pkg/target/debug/build");
        let mut found = Vec::new();
        let mut stack: Vec<PathBuf> = std::fs::read_dir(&build)
            .unwrap_or_else(|e| panic!("reading {}: {e}", build.display()))
            .map(|e| e.expect("build entry").path())
            .collect();
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(
                    std::fs::read_dir(&path)
                        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
                        .map(|e| e.expect("build entry").path()),
                );
            } else if path.file_name().is_some_and(|n| n == "output") {
                found.push(path);
            }
        }
        assert!(!found.is_empty(), "{ws}: no build script output found");
        let mut directives = String::new();
        for path in &found {
            directives.push_str(
                &std::fs::read_to_string(path)
                    .unwrap_or_else(|e| panic!("reading {}: {e}", path.display())),
            );
        }
        directives
    }

    /// A packaged build must not watch `../../.git/HEAD`: beside the manifest
    /// that path points outside the package, rerunning the script on every
    /// build — the marker's second job, alongside silencing the lookup.
    fn assert_no_rerun_directive(&self, krate: &str, directives: &str) {
        assert!(
            !directives.contains("rerun-if-changed"),
            "{krate}: packaged build still watches a foreign path: {directives:?}"
        );
    }

    /// The unpackaged build watches the checkout's HEAD for the `-dirty`
    /// suffix: this is the directive the marker skips above.
    fn assert_watches_git_head(&self, krate: &str, directives: &str) {
        assert!(
            directives.contains("cargo:rerun-if-changed=../../.git/HEAD"),
            "{krate}: unpackaged build lost its HEAD watch: {directives:?}"
        );
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("running git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("running git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout)
        .expect("sha is utf-8")
        .trim()
        .to_owned()
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

/// The root `suse-mtui` facade lives at the repo root, so without an `include`
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
            "suse-mtui",
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

/// Crates whose integration tests `include_str!` fixtures from outside the
/// package (`../../mtui-types/tests/...`): those tests compile in the
/// workspace but not from a crates.io tarball, so they carry
/// `exclude = ["tests/**"]` and don't ship. These are the only members with
/// foreign test refs (`src/` unit tests cannot be excluded and use
/// `mtui_types::fixtures` instead — see below). The root facade's `tests/`
/// only reference files inside the root package, so it ships.
const TESTS_EXCLUDED: &[&str] = &[
    "crates/mtui-datasources/Cargo.toml",
    "crates/mtui-mcp/Cargo.toml",
    "crates/mtui-testreport/Cargo.toml",
    "crates/mtui-tui/Cargo.toml",
];

/// The `cargo package --list` output for one shippable crate: the exact file
/// set its tarball would carry.
fn package_list(name: &str) -> Vec<String> {
    let out = std::process::Command::new(env!("CARGO"))
        .args(["package", "--list", "--allow-dirty", "--locked", "-p", name])
        .current_dir(workspace_root())
        .output()
        .expect("running cargo package --list");
    assert!(
        out.status.success(),
        "cargo package --list -p {name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("package list is utf-8")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every `TESTS_EXCLUDED` manifest excludes exactly `tests/**` (nothing more
/// must go missing silently), its tarball lists no `tests/` entry, and the
/// set is exhaustive both ways: a crate with foreign refs in `tests/` that
/// is not listed here breaks tarball `cargo test`, a listed crate whose
/// refs are gone carries a stale exclusion, and a crate carrying an `exclude`
/// without being listed here silently drops shipped files from its tarball —
/// so every shippable member is checked for a listing-less `exclude` too.
#[test]
fn crates_with_foreign_test_fixtures_exclude_tests() {
    let mut found: Vec<&str> = Vec::new();
    for (rel, name) in SHIPPABLE {
        let dir = workspace_root().join(rel).parent().unwrap().to_path_buf();
        let has_foreign = rs_files(&dir.join("tests")).iter().any(|file| {
            include_literals(&std::fs::read_to_string(file).expect("reading test"))
                .iter()
                .filter(|lit| lit.contains(".."))
                .any(|lit| !normalize(&file.parent().unwrap().join(lit)).starts_with(&dir))
        });
        if has_foreign {
            found.push(rel);
        }
        let doc = manifest(rel);
        let excluded: Vec<String> = pkg(&doc, rel)
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .map(|v| {
                        v.as_str()
                            .unwrap_or_else(|| panic!("{rel}: exclude is not strings"))
                            .to_owned()
                    })
                    .collect()
            })
            .unwrap_or_default();
        if TESTS_EXCLUDED.contains(rel) {
            assert_eq!(
                excluded,
                ["tests/**"],
                "{rel}: exclude drifted past tests/**"
            );
            let list = package_list(name);
            let leaked: Vec<&String> = list.iter().filter(|l| l.starts_with("tests/")).collect();
            assert!(
                leaked.is_empty(),
                "{rel}: tarball still ships tests: {leaked:#?}"
            );
        } else {
            assert!(
                excluded.is_empty(),
                "{rel}: carries exclude {excluded:?} without being listed in TESTS_EXCLUDED"
            );
        }
    }
    // Exhaustive both ways: a crate with foreign refs in `tests/` that is
    // not listed breaks tarball `cargo test`; a listed crate whose refs are
    // gone carries a stale exclusion (each arm is asserted above — this names
    // the drift on either side).
    found.sort();
    let mut listed: Vec<&str> = TESTS_EXCLUDED.to_vec();
    listed.sort();
    assert_eq!(
        found, listed,
        "TESTS_EXCLUDED drifted from the crates with foreign test refs"
    );
}

/// Every fixture path in a shipped source must resolve to a file that ships
/// with the same tarball — whether read at compile time (`include_str!` /
/// `include_bytes!`) or at run time (`CARGO_MANIFEST_DIR` joins, `concat!`):
/// the `src/` unit tests that used to reach outside their package now use
/// `mtui_types::fixtures`, and the excluded `tests/` trees are not scanned
/// (they don't ship — see above). Each `tests/fixtures` literal is resolved
/// both against its file's directory (the compile-time read base) and
/// against the crate root (the run-time `CARGO_MANIFEST_DIR` base); either
/// may hit, and a literal resolving to a shipped directory (a prefix of
/// shipped files) passes. Resolving against `cargo package --list` rather
/// than the worktree means an over-broad future `exclude` goes red here
/// instead of breaking tarball `cargo test` silently.
#[test]
fn shipped_sources_reference_only_shipped_files() {
    for (rel, name) in SHIPPABLE {
        let dir = workspace_root().join(rel).parent().unwrap().to_path_buf();
        let list = package_list(name);
        for entry in &list {
            if !entry.ends_with(".rs") {
                continue;
            }
            let file = dir.join(entry);
            let body = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
            let parent = file.parent().unwrap();
            for lit in include_literals(&body) {
                if !lit.contains("..") {
                    continue;
                }
                let resolved = normalize(&parent.join(&lit));
                let shipped = resolved.strip_prefix(&dir).unwrap_or_else(|_| {
                    panic!("{rel}: {entry} references {lit:?}, outside the package")
                });
                let shipped = shipped.to_string_lossy().replace('\\', "/");
                assert!(
                    list.iter().any(|l| l == &shipped),
                    "{rel}: {entry} references {lit:?} ({shipped}), which the tarball excludes"
                );
            }
            for lit in literals_containing(&body, "tests/fixtures") {
                let file_relative = normalize(&parent.join(&lit));
                let crate_relative = normalize(&dir.join(lit.trim_start_matches('/')));
                let mut ok = false;
                for resolved in [&file_relative, &crate_relative] {
                    let Ok(rel_path) = resolved.strip_prefix(&dir) else {
                        continue;
                    };
                    let rel_path = rel_path.to_string_lossy().replace('\\', "/");
                    if list
                        .iter()
                        .any(|l| l == &rel_path || l.starts_with(&format!("{rel_path}/")))
                    {
                        ok = true;
                        break;
                    }
                }
                assert!(
                    ok,
                    "{rel}: {entry} references {lit:?}, which ships with no tarball file"
                );
            }
        }
    }
}

/// All `.rs` files under `dir`, skipping `target/` build output.
fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in
            std::fs::read_dir(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The string operands of `include_str!`/`include_bytes!` in `body`.
fn include_literals(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for macro_name in ["include_str!", "include_bytes!"] {
        let mut rest = body;
        while let Some(at) = rest.find(macro_name) {
            rest = &rest[at + macro_name.len()..];
            let Some(open) = rest.find('"') else { break };
            rest = &rest[open + 1..];
            let Some(close) = rest.find('"') else { break };
            out.push(rest[..close].to_owned());
            rest = &rest[close + 1..];
        }
    }
    out
}

/// Every double-quoted string literal in `body` containing `marker` — the
/// run-time half of the shipped-sources guard below: fixture paths built at
/// run time (`CARGO_MANIFEST_DIR` joins, `concat!`) never pass through
/// `include_str!`, so `include_literals` cannot see them.
/// Raw strings delimit their content with `"` the same way, and fixture
/// paths never contain a quote, so naive pairing is safe; `\` escapes are
/// honoured so a quoted quote does not end the literal early.
fn literals_containing(body: &str, marker: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut closed = false;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => {
                    closed = true;
                    break;
                }
                _ => j += 1,
            }
        }
        if closed {
            let lit = &body[i + 1..j];
            if lit.contains(marker) {
                out.push(lit.to_owned());
            }
            i = j + 1;
        } else {
            break;
        }
    }
    out
}

/// Lexical `..`/`.` resolution, without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                } else {
                    parts.push(component);
                }
            }
            Component::CurDir => {}
            _ => parts.push(component),
        }
    }
    parts.iter().collect()
}
