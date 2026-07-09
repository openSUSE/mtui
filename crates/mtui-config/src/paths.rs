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
//! `~/.mtuirc`. mtui-rs uses **TOML** and an XDG-first order:
//!
//! ```text
//! --config  →  $MTUI_CONF  →  $XDG_CONFIG_HOME/mtui/config.toml  →  /etc/mtui.toml
//! ```
//!
//! This module is pure and I/O-free: it only computes paths, it never reads
//! files (that is [`crate::Config::load`]'s job).

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// System-wide config file. Lowest precedence.
const ETC_CONFIG: &str = "/etc/mtui.toml";

/// Environment variable holding an explicit config-file override.
const ENV_CONFIG: &str = "MTUI_CONF";

/// Basename of the per-user config file inside the XDG config directory.
const XDG_CONFIG_FILE: &str = "config.toml";

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
/// `$XDG_CONFIG_HOME/mtui` (falling back to `~/.config/mtui` per the XDG spec).
#[must_use]
pub fn xdg_config_file() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.config_dir().join(XDG_CONFIG_FILE))
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
/// Where mtui-rs persists durable per-user state such as the REPL history file.
/// Distinct from [`cache_dir`] (disposable) and the config dir (user-authored):
/// history is data the user grows and expects to survive a cache wipe.
///
/// Deliberate deviation from upstream, which keeps history at `~/.mtui_history`;
/// mtui-rs is XDG-first for config/cache/data alike.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.data_dir().to_path_buf())
}

/// Compute the ordered list of config files to load, lowest precedence first.
///
/// Resolution rules (mirrors upstream's short-circuit for the explicit forms):
///
/// * If `explicit` is `Some` (the `--config` flag), that single file is the
///   only candidate.
/// * Else if `$MTUI_CONF` is set (and non-empty), that single file (with `~`
///   expanded) is the only candidate.
/// * Otherwise the default pair is returned, **lowest precedence first**:
///   `/etc/mtui.toml`, then the XDG per-user file. [`crate::Config::load`]
///   merges them in order so the per-user file wins on shared keys.
#[must_use]
pub fn config_search_paths(explicit: Option<PathBuf>) -> Vec<PathBuf> {
    let env = std::env::var_os(ENV_CONFIG).map(PathBuf::from);
    resolve_search_paths(explicit, env, xdg_config_file())
}

/// Pure core of [`config_search_paths`], with the process environment injected
/// so it can be unit-tested without mutating global state.
fn resolve_search_paths(
    explicit: Option<PathBuf>,
    env_config: Option<PathBuf>,
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

    let mut paths = vec![PathBuf::from(ETC_CONFIG)];
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
            Some(PathBuf::from("/xdg/config.toml")),
        );
        assert_eq!(paths, vec![PathBuf::from("/explicit.toml")]);
    }

    #[test]
    fn env_var_is_single_candidate_when_no_explicit() {
        let paths = resolve_search_paths(
            None,
            Some(PathBuf::from("/from/env.toml")),
            Some(PathBuf::from("/xdg/config.toml")),
        );
        assert_eq!(paths, vec![PathBuf::from("/from/env.toml")]);
    }

    #[test]
    fn empty_env_var_falls_through_to_defaults() {
        let paths = resolve_search_paths(None, Some(PathBuf::new()), None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG)]);
    }

    #[test]
    fn defaults_are_etc_first_then_xdg() {
        let xdg = PathBuf::from("/home/u/.config/mtui/config.toml");
        let paths = resolve_search_paths(None, None, Some(xdg.clone()));
        // /etc is always present and lowest precedence (first).
        assert_eq!(paths.first(), Some(&PathBuf::from(ETC_CONFIG)));
        // The XDG dir is appended after /etc (higher precedence).
        assert_eq!(paths.last(), Some(&xdg));
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn defaults_without_xdg_is_etc_only() {
        let paths = resolve_search_paths(None, None, None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG)]);
    }

    #[test]
    fn env_var_path_expands_tilde() {
        let paths = resolve_search_paths(None, Some(PathBuf::from("~/my.toml")), None);
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
