//! Filesystem path resolution for `mtui-config`.
//!
//! Two flavours of paths live here, mirroring upstream `mtui/support/paths.py`:
//!
//! * **Config search paths** ([`config_search_paths`]) — the ordered list of
//!   candidate config files, later entries overriding earlier ones when merged.
//! * **User cache path** ([`cache_dir`]) — the XDG cache directory where mtui
//!   persists per-user state (upstream `save_cache_path`).
//!
//! ## Deviation from upstream (intentional)
//!
//! Upstream reads INI from `--config` → `$MTUI_CONF` → `/etc/mtui.cfg` +
//! `~/.mtuirc`. mtui uses **TOML**; the config filename is always
//! `mtui.toml`. Precedence, lowest → highest:
//!
//! ```text
//! /etc/mtui.toml  →  ~/.mtui.toml  →  $XDG_CONFIG_HOME/mtui/mtui.toml
//! ```
//!
//! with `--config <file>` and `$MTUI_CONF` each short-circuiting the chain to a
//! single file. The home dotfile `~/.mtui.toml` echoes upstream's `~/.mtuirc`
//! for operators who prefer a dotfile over the XDG directory; when both exist
//! the XDG file wins on shared keys (it is merged last).
//!
//! This module is pure and I/O-free: it only computes paths, it never reads
//! files (that is [`crate::Config::load`]'s job).

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// System-wide config file. Lowest precedence.
const ETC_CONFIG: &str = "/etc/mtui.toml";

/// Environment variable holding an explicit config-file override.
const ENV_CONFIG: &str = "MTUI_CONF";

/// Environment variable overriding the `term.*.sh` script directory.
const ENV_TERMS: &str = "MTUI_TERMS_DIR";

/// Basename of the per-user config file, used both for the home dotfile
/// (`~/.mtui.toml`, with a leading dot) and inside the XDG config directory
/// (`$XDG_CONFIG_HOME/mtui/mtui.toml`).
const CONFIG_FILE: &str = "mtui.toml";

/// Expand a leading `~` (and `~/`) to the user's home directory.
///
/// Mirrors Python's `Path(...).expanduser()` for the single common case used by
/// mtui config (`~` / `~/...`). A bare `~user` form is left untouched — mtui has
/// never relied on it.
#[must_use]
pub fn expanduser(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
        return path.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Best-effort home directory, via the `directories` crate.
fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// The per-user XDG config file path, if a home/config dir can be resolved.
///
/// Uses `ProjectDirs::from("", "", "mtui")` so the directory is
/// `$XDG_CONFIG_HOME/mtui` (falling back to `~/.config/mtui` per the XDG spec),
/// and the file is `mtui.toml` within it.
#[must_use]
pub fn xdg_config_file() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.config_dir().join(CONFIG_FILE))
}

/// The home-directory dotfile config path `~/.mtui.toml`, if a home dir can be
/// resolved.
///
/// The mtui analogue of upstream's `~/.mtuirc`: a single dotfile in `$HOME`
/// for operators who prefer it to the XDG config directory. Sits between `/etc`
/// and the XDG file in precedence (see [`config_search_paths`]).
#[must_use]
pub fn home_config_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".mtui.toml"))
}

/// The user cache directory for mtui (`$XDG_CACHE_HOME/mtui`), if resolvable.
///
/// Upstream equivalent: `mtui.support.paths.save_cache_path("mtui")`.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.cache_dir().to_path_buf())
}

/// The user data directory for mtui (`$XDG_DATA_HOME/mtui`), if resolvable.
///
/// Where mtui persists durable per-user state such as the REPL history file.
/// Distinct from [`cache_dir`] (disposable) and the config dir (user-authored):
/// history is data the user grows and expects to survive a cache wipe.
///
/// Deliberate deviation from upstream, which keeps history at `~/.mtui_history`;
/// mtui is XDG-first for config/cache/data alike.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.data_dir().to_path_buf())
}

/// The directory holding the `term.*.sh` terminal-launcher scripts, if it can be
/// resolved.
///
/// Resolution order:
///
/// * If `$MTUI_TERMS_DIR` is set (and non-empty), that directory (with `~`
///   expanded) is used verbatim. This is how a system/package install points at
///   its shared datadir (e.g. `/usr/share/mtui/terms`) without copying scripts
///   into the per-user XDG tree.
/// * Otherwise the default is `$XDG_DATA_HOME/mtui/terms` (consistent with
///   [`data_dir`]), where packaging may also install `term.*.sh`.
///
/// Upstream equivalent: `mtui.support.paths.terms_path()`, which resolves the
/// `terms/` directory shipped as package data inside the installed `mtui`
/// package. Rust has no package-data concept, so mtui ships the scripts under
/// `dist/terms/` and lets packaging install them to the datadir (or `MTUI_TERMS_DIR`
/// point elsewhere). The `terms` command derives the available term names by
/// globbing this directory, mirroring upstream's dynamic `_list_terms`.
#[must_use]
pub fn terms_path() -> Option<PathBuf> {
    resolve_terms_path(std::env::var_os(ENV_TERMS).map(PathBuf::from), data_dir())
}

/// Pure core of [`terms_path`], with the environment override and data dir
/// injected so it can be unit-tested without mutating global process state.
fn resolve_terms_path(env_terms: Option<PathBuf>, data: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(env) = env_terms
        && !env.as_os_str().is_empty()
    {
        return Some(expanduser(&env));
    }
    data.map(|d| d.join("terms"))
}

/// Compute the ordered list of config files to load, lowest precedence first.
///
/// Resolution rules (mirrors upstream's short-circuit for the explicit forms):
///
/// * If `explicit` is `Some` (the `--config` flag), that single file is the
///   only candidate.
/// * Else if `$MTUI_CONF` is set (and non-empty), that single file (with `~`
///   expanded) is the only candidate.
/// * Otherwise the default chain is returned, **lowest precedence first**:
///   `/etc/mtui.toml`, then `~/.mtui.toml`, then the XDG per-user file
///   (`$XDG_CONFIG_HOME/mtui/mtui.toml`). [`crate::Config::load`] merges them in
///   order so a later file wins on shared keys.
#[must_use]
pub fn config_search_paths(explicit: Option<PathBuf>) -> Vec<PathBuf> {
    let env = std::env::var_os(ENV_CONFIG).map(PathBuf::from);
    resolve_search_paths(explicit, env, home_config_file(), xdg_config_file())
}

/// Pure core of [`config_search_paths`], with the process environment injected
/// so it can be unit-tested without mutating global state.
fn resolve_search_paths(
    explicit: Option<PathBuf>,
    env_config: Option<PathBuf>,
    home: Option<PathBuf>,
    xdg: Option<PathBuf>,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path];
    }

    if let Some(env) = env_config
        && !env.as_os_str().is_empty()
    {
        return vec![expanduser(&env)];
    }

    // Default chain, lowest precedence first: /etc → home dotfile → XDG.
    let mut paths = vec![PathBuf::from(ETC_CONFIG)];
    if let Some(home) = home {
        paths.push(home);
    }
    if let Some(xdg) = xdg {
        paths.push(xdg);
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: precedence is tested through the pure `resolve_search_paths` core so
    // no test mutates the process environment (which is `unsafe` under edition
    // 2024 and racy across threads). `config_search_paths` is the thin wrapper
    // that only reads `$MTUI_CONF` and calls this core.

    #[test]
    fn explicit_path_short_circuits_everything() {
        // Even with MTUI_CONF set, an explicit --config wins and is the sole entry.
        let paths = resolve_search_paths(
            Some(PathBuf::from("/explicit.toml")),
            Some(PathBuf::from("/from/env.toml")),
            Some(PathBuf::from("/home/u/.mtui.toml")),
            Some(PathBuf::from("/xdg/mtui.toml")),
        );
        assert_eq!(paths, vec![PathBuf::from("/explicit.toml")]);
    }

    #[test]
    fn env_var_is_single_candidate_when_no_explicit() {
        let paths = resolve_search_paths(
            None,
            Some(PathBuf::from("/from/env.toml")),
            Some(PathBuf::from("/home/u/.mtui.toml")),
            Some(PathBuf::from("/xdg/mtui.toml")),
        );
        assert_eq!(paths, vec![PathBuf::from("/from/env.toml")]);
    }

    #[test]
    fn empty_env_var_falls_through_to_defaults() {
        let paths = resolve_search_paths(None, Some(PathBuf::new()), None, None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG)]);
    }

    #[test]
    fn defaults_are_etc_then_home_then_xdg() {
        let home = PathBuf::from("/home/u/.mtui.toml");
        let xdg = PathBuf::from("/home/u/.config/mtui/mtui.toml");
        let paths = resolve_search_paths(None, None, Some(home.clone()), Some(xdg.clone()));
        // Lowest → highest precedence: /etc, then the home dotfile, then XDG.
        assert_eq!(
            paths,
            vec![PathBuf::from(ETC_CONFIG), home, xdg],
            "order must be etc → home → xdg"
        );
    }

    #[test]
    fn defaults_home_only_when_no_xdg() {
        let home = PathBuf::from("/home/u/.mtui.toml");
        let paths = resolve_search_paths(None, None, Some(home.clone()), None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG), home]);
    }

    #[test]
    fn defaults_without_home_or_xdg_is_etc_only() {
        let paths = resolve_search_paths(None, None, None, None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG)]);
    }

    #[test]
    fn env_var_path_expands_tilde() {
        let paths = resolve_search_paths(None, Some(PathBuf::from("~/my.toml")), None, None);
        // The `~` must have been expanded away (or, if no home dir, left as-is).
        if home_dir().is_some() {
            assert!(!paths[0].starts_with("~"));
            assert!(paths[0].ends_with("my.toml"));
        }
    }

    #[test]
    fn public_wrapper_returns_etc_and_optional_xdg() {
        // Smoke-test the public entry: with an explicit path it must return
        // exactly that, regardless of ambient env.
        let paths = config_search_paths(Some(PathBuf::from("/x.toml")));
        assert_eq!(paths, vec![PathBuf::from("/x.toml")]);
    }

    #[test]
    fn home_config_file_is_dotfile_in_home() {
        if let Some(home) = home_dir() {
            let f = home_config_file().expect("home dir resolved, so should the dotfile");
            assert_eq!(f, home.join(".mtui.toml"));
        }
    }

    #[test]
    fn xdg_and_home_files_share_the_mtui_toml_basename() {
        // The config filename is always `mtui.toml` (XDG) / `.mtui.toml` (home);
        // never `config.toml`.
        if let Some(xdg) = xdg_config_file() {
            assert!(xdg.ends_with("mtui.toml"), "XDG file should be mtui.toml");
            assert!(!xdg.ends_with("config.toml"));
        }
        if let Some(home) = home_config_file() {
            assert_eq!(home.file_name().unwrap(), ".mtui.toml");
        }
    }

    #[test]
    fn data_dir_lives_under_an_mtui_directory() {
        // On any environment where a data dir resolves, it must be the mtui
        // subdir (parallel to `cache_dir`), so the history file lands under it.
        if let Some(dir) = data_dir() {
            assert!(
                dir.ends_with("mtui"),
                "data dir should end in `mtui`, got {dir:?}"
            );
        }
    }

    #[test]
    fn terms_path_lives_under_the_mtui_data_dir() {
        // When a data dir resolves, the terms dir is `<data>/terms` and the data
        // component still ends in `mtui` (parallel to `data_dir`). Exercise the
        // pure core with the override forced off so an ambient `MTUI_TERMS_DIR`
        // in the test environment can't perturb the default-path invariant.
        if let Some(dir) = resolve_terms_path(None, data_dir()) {
            assert!(
                dir.ends_with("terms"),
                "terms path should end in `terms`, got {dir:?}"
            );
            assert!(
                dir.parent().is_some_and(|p| p.ends_with("mtui")),
                "terms parent should be the mtui data dir, got {dir:?}"
            );
        }
    }

    #[test]
    fn terms_path_env_override_wins_and_expands_tilde() {
        // A set, non-empty override is used verbatim (data dir irrelevant).
        assert_eq!(
            resolve_terms_path(
                Some(PathBuf::from("/usr/share/mtui/terms")),
                Some(PathBuf::from("/data/mtui")),
            ),
            Some(PathBuf::from("/usr/share/mtui/terms"))
        );
        // A leading `~` in the override is expanded like other mtui paths.
        if let Some(home) = home_dir() {
            assert_eq!(
                resolve_terms_path(Some(PathBuf::from("~/terms")), None),
                Some(home.join("terms"))
            );
        }
    }

    #[test]
    fn terms_path_falls_back_to_data_dir_when_override_absent_or_empty() {
        // Unset override → `<data>/terms`.
        assert_eq!(
            resolve_terms_path(None, Some(PathBuf::from("/data/mtui"))),
            Some(PathBuf::from("/data/mtui/terms"))
        );
        // Empty override is treated as unset (same as `MTUI_CONF`'s handling).
        assert_eq!(
            resolve_terms_path(Some(PathBuf::new()), Some(PathBuf::from("/data/mtui"))),
            Some(PathBuf::from("/data/mtui/terms"))
        );
        // No data dir and no override → nothing resolves.
        assert_eq!(resolve_terms_path(None, None), None);
    }

    #[test]
    fn expanduser_handles_bare_tilde_and_prefix() {
        if let Some(home) = home_dir() {
            assert_eq!(expanduser(Path::new("~")), home);
            assert_eq!(expanduser(Path::new("~/a/b")), home.join("a/b"));
        }
        // A non-tilde path is passed through unchanged.
        assert_eq!(
            expanduser(Path::new("/abs/path")),
            PathBuf::from("/abs/path")
        );
    }
}
