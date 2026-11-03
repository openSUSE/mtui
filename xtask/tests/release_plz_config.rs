//! Guards for `release-plz.toml`.
//!
//! The tag shape is load-bearing: `_service`'s `versionrewrite` (`v?(…)`) and
//! release.yml's `[0-9]+…` tag filter both key on the unprefixed `{{ version }}`
//! shape, and GitHub releases stay with release.yml's `gh release create`.
//! A second crate enabling tags with the same name would collide on the one
//! shared tag, so exactly one package stanza may enable them.

use std::path::PathBuf;

fn config() -> toml::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("release-plz.toml");
    let text = std::fs::read_to_string(&path).expect("read release-plz.toml");
    toml::from_str(&text).expect("parse release-plz.toml")
}

fn workspace(config: &toml::Value) -> &toml::Table {
    config
        .get("workspace")
        .and_then(toml::Value::as_table)
        .expect("release-plz.toml has a [workspace] table")
}

fn packages(config: &toml::Value) -> Vec<&toml::Table> {
    config
        .get("package")
        .map(|pkgs| {
            pkgs.as_array()
                .expect("[[package]] stanzas parse as an array")
                .iter()
                .map(|pkg| pkg.as_table().expect("each [[package]] stanza is a table"))
                .collect()
        })
        .unwrap_or_default()
}

fn flag(table: &toml::Table, key: &str) -> Option<bool> {
    table.get(key).map(|v| {
        v.as_bool()
            .unwrap_or_else(|| panic!("{key} must be a boolean"))
    })
}

/// The one tag per release is the bare version: a `v` prefix matches neither
/// `_service`'s rewrite nor release.yml's filter, and two taggers would
/// collide pushing the same name.
#[test]
fn release_tag_is_the_bare_version_from_one_package() {
    let cfg = config();
    assert_eq!(
        flag(workspace(&cfg), "git_tag_enable"),
        Some(false),
        "workspace tags off: the single [[package]] stanza below owns the tag"
    );
    let pkgs = packages(&cfg);
    let taggers: Vec<&&toml::Table> = pkgs
        .iter()
        .filter(|pkg| flag(pkg, "git_tag_enable") == Some(true))
        .collect();
    assert_eq!(
        taggers.len(),
        1,
        "exactly one package creates the tag, else the shared name collides"
    );
    let tagger = taggers[0];
    assert_eq!(
        tagger.get("name").and_then(toml::Value::as_str),
        Some("suse-mtui"),
        "the facade crate owns the release tag"
    );
    assert_eq!(
        tagger.get("git_tag_name").and_then(toml::Value::as_str),
        Some("{{ version }}"),
        "tag is unprefixed: `v{{{{ version }}}}` matches no release filter"
    );
}

/// release-plz must not take over GitHub releases: release.yml's matrix +
/// notes-base walk stays the only creator.
#[test]
fn git_releases_stay_with_the_release_workflow() {
    let cfg = config();
    assert_eq!(
        flag(workspace(&cfg), "git_release_enable"),
        Some(false),
        "workspace GitHub releases off"
    );
    for pkg in packages(&cfg) {
        assert!(
            flag(pkg, "git_release_enable") != Some(true),
            "package {:?} must not re-enable GitHub releases",
            pkg.get("name"),
        );
    }
}

/// Publishing happens on Release-PR merges, never on direct `main` pushes.
#[test]
fn release_runs_on_release_pr_merges_only() {
    let cfg = config();
    assert_eq!(
        flag(workspace(&cfg), "release_always"),
        Some(false),
        "release_always off: only the merged Release PR publishes"
    );
    for pkg in packages(&cfg) {
        assert!(
            flag(pkg, "release_always") != Some(true),
            "package {:?} must not re-enable release_always",
            pkg.get("name"),
        );
    }
}

/// No generated changelogs anywhere: the hand-written Keep-a-Changelog root
/// file is the only one, and the Release PR renames its `## [Unreleased]`
/// section instead of inserting a generated one. A later edit re-enabling
/// `changelog_update` would make release-plz fight the hand-written entries.
#[test]
fn no_generated_changelogs() {
    let cfg = config();
    assert_eq!(
        flag(workspace(&cfg), "changelog_update"),
        Some(false),
        "workspace changelogs off: the root file is hand-written"
    );
    for pkg in packages(&cfg) {
        assert!(
            flag(pkg, "changelog_update") != Some(true),
            "package {:?} must not generate a changelog",
            pkg.get("name"),
        );
    }
}

/// No semver-driven major bumps: the year-based line (`26.x`) is not semver,
/// so a cargo-semver-checks finding must never propose `26` → `27`.
/// Hand-fixing such a proposal reads as a Release-PR edit and closes and
/// recreates the PR, so the check stays off and bumps follow the edits alone.
#[test]
fn semver_check_stays_off() {
    let cfg = config();
    assert_eq!(
        flag(workspace(&cfg), "semver_check"),
        Some(false),
        "workspace semver checks off: bumps follow Release-PR edits, not API breaks"
    );
    for pkg in packages(&cfg) {
        assert!(
            flag(pkg, "semver_check") != Some(true),
            "package {:?} must not re-enable semver checks",
            pkg.get("name"),
        );
    }
}

/// Exhaustiveness: every publishable member has a stanza. A member without
/// one still publishes under the workspace defaults but outside the version
/// group, diverging from the lockstep line silently (and release-plz itself
/// only errors on a stanza naming nothing — the reverse gap needs this).
/// `xtask` (`publish = false`) is the only intentional exception.
#[test]
fn every_publishable_member_has_a_stanza() {
    use std::collections::HashSet;
    let cfg = config();
    let stanzas: HashSet<&str> = packages(&cfg)
        .iter()
        .map(|pkg| {
            pkg.get("name")
                .and_then(toml::Value::as_str)
                .expect("each [[package]] stanza names a package")
        })
        .collect();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // Explicit, not globbed: `xtask` is a member outside `crates/` (kept
    // visible so dropping its `publish = false` fails the stanza check
    // below), while `fuzz/` is a detached workspace and stays out.
    let mut manifests = vec![root.join("Cargo.toml"), root.join("xtask/Cargo.toml")];
    let crates = root.join("crates");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&crates)
        .expect("read crates/")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    manifests.extend(dirs.iter().map(|dir| dir.join("Cargo.toml")));
    for manifest in &manifests {
        let text = std::fs::read_to_string(manifest)
            .unwrap_or_else(|_| panic!("read {}", manifest.display()));
        let manifest: toml::Value = toml::from_str(&text).expect("parse member manifest");
        if manifest["package"].get("publish") == Some(&toml::Value::Boolean(false)) {
            let name = manifest["package"]["name"]
                .as_str()
                .expect("member has a package name");
            assert!(
                !stanzas.contains(name),
                "unpublished member {name} must not have a [[package]] stanza"
            );
            continue;
        }
        let name = manifest["package"]["name"]
            .as_str()
            .expect("member has a package name");
        assert!(
            stanzas.contains(name),
            "publishable member {name} has no [[package]] stanza"
        );
    }
}

/// Lockstep line: every stanza shares one version group, so no crate can
/// drift off the shared `version.workspace` — today the workspace arm takes
/// the max across inheritors, and the group still holds a crate that ever
/// drops the inheritance.
#[test]
fn one_version_group_covers_every_stanza() {
    use std::collections::HashSet;
    let cfg = config();
    let pkgs = packages(&cfg);
    assert!(!pkgs.is_empty(), "expected [[package]] stanzas to group",);
    let groups: HashSet<&str> = pkgs
        .iter()
        .map(|pkg| {
            pkg.get("version_group")
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| {
                    panic!("package {:?} is outside any version group", pkg.get("name"))
                })
        })
        .collect();
    assert_eq!(
        groups.len(),
        1,
        "one shared group, not per-crate lines: {groups:?}",
    );
}
