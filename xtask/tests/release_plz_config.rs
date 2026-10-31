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

fn package<'a>(config: &'a toml::Value, name: &str) -> &'a toml::Table {
    packages(config)
        .into_iter()
        .find(|pkg| pkg.get("name").and_then(toml::Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("release-plz.toml has no [[package]] {name}"))
}

fn str_list<'a>(table: &'a toml::Table, key: &str) -> Vec<&'a str> {
    table
        .get(key)
        .unwrap_or_else(|| panic!("{key} is set"))
        .as_array()
        .unwrap_or_else(|| panic!("{key} is a list"))
        .iter()
        .map(|v| v.as_str().unwrap_or_else(|| panic!("{key} holds strings")))
        .collect()
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
        Some("mtui"),
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

/// The one draft section covers every shippable crate: `changelog_include`
/// must match the `crates/*` members (the facade itself and the
/// `publish = false` xtask deliberately excepted), or a crate's commits
/// silently miss the draft (and an unknown name fails the Release PR).
#[test]
fn changelog_include_matches_workspace_members() {
    use std::collections::HashSet;
    let cfg = config();
    let mtui = package(&cfg, "mtui");
    let included: HashSet<&str> = str_list(mtui, "changelog_include").into_iter().collect();
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates");
    let members: HashSet<String> = std::fs::read_dir(&crates)
        .expect("read crates/")
        .filter_map(|entry| {
            let entry = entry.expect("dir entry");
            if !entry.file_type().expect("file type").is_dir() {
                return None;
            }
            let manifest = entry.path().join("Cargo.toml");
            let text = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|_| panic!("read {}", manifest.display()));
            let manifest: toml::Value = toml::from_str(&text).expect("parse member manifest");
            Some(
                manifest["package"]["name"]
                    .as_str()
                    .expect("member has a package name")
                    .to_owned(),
            )
        })
        .collect();
    let members: HashSet<&str> = members.iter().map(String::as_str).collect();
    assert_eq!(
        included, members,
        "changelog_include drifted from crates/*: missing crates lose draft entries"
    );
}

/// Single-changelog contract: the workspace default stays off and only the
/// facade writes the root file, or a later edit scatters per-crate drafts.
#[test]
fn single_changelog_stays_with_facade() {
    let cfg = config();
    assert_eq!(
        flag(workspace(&cfg), "changelog_update"),
        Some(false),
        "workspace changelogs off: the root file is the only one"
    );
    let mtui = package(&cfg, "mtui");
    assert_eq!(
        flag(mtui, "changelog_update"),
        Some(true),
        "the facade owns the draft section"
    );
    assert_eq!(
        mtui.get("changelog_path").and_then(toml::Value::as_str),
        Some("./CHANGELOG.md"),
        "the draft lands in the root Keep-a-Changelog file"
    );
    for pkg in packages(&cfg) {
        if pkg.get("name").and_then(toml::Value::as_str) != Some("mtui") {
            assert!(
                flag(pkg, "changelog_update") != Some(true),
                "package {:?} must not enable its own changelog",
                pkg.get("name"),
            );
        }
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
