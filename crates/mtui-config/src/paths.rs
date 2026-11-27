//! Filesystem path resolution for `mtui-config`: the ordered config search
//! paths (`config_chain`) and the XDG data directory ([`data_dir`]).
//!
//! The config filename is always `mtui.toml`. Precedence, lowest → highest:
//!
//! ```text
//! /etc/mtui.toml  →  ~/.mtui.toml  →  $XDG_CONFIG_HOME/mtui/mtui.toml
//! ```
//!
//! with `--config <file>` and `$MTUI_CONF` each short-circuiting the chain to a
//! single file. The home dotfile is for operators who prefer it to the XDG
//! directory; when both exist the XDG file wins on shared keys (merged last).
//!
//! Pure and I/O-free: it computes paths, it never reads files (that is
//! [`crate::Config::load`]'s job).

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::notice::ConfigSource;

/// System-wide config file. Lowest precedence.
const ETC_CONFIG: &str = "/etc/mtui.toml";

/// Environment variable holding an explicit config-file override.
const ENV_CONFIG: &str = "MTUI_CONF";

/// Basename of the per-user config file inside the XDG config directory.
const CONFIG_FILE: &str = "mtui.toml";

/// Expand a leading `~` / `~/`; the `~user` form is left untouched.
#[must_use]
pub(crate) fn expanduser(path: &Path) -> PathBuf {
    expand_home(path, home_dir().as_deref())
}

/// [`expanduser`] against a given home directory.
fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let (Some(s), Some(home)) = (path.to_str(), home) else {
        return path.to_path_buf();
    };
    if s == "~" {
        return home.to_path_buf();
    }
    match s.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => path.to_path_buf(),
    }
}

/// Best-effort home directory, via the `directories` crate.
fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// The per-user XDG config file (`$XDG_CONFIG_HOME/mtui/mtui.toml`), if
/// resolvable.
#[must_use]
fn xdg_config_file() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.config_dir().join(CONFIG_FILE))
}

/// The home dotfile `~/.mtui.toml`, if a home dir can be resolved.
#[must_use]
fn home_config_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".mtui.toml"))
}

/// The user data directory for mtui (`$XDG_DATA_HOME/mtui`), if resolvable.
///
/// Durable per-user state such as the REPL history lives here, apart from the
/// disposable cache dir: it must survive a cache wipe.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "mtui").map(|p| p.data_dir().to_path_buf())
}

/// Compute the ordered list of config files to load, lowest precedence first.
///
/// Resolution rules (the explicit forms short-circuit the chain):
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
pub(crate) fn config_chain(explicit: Option<PathBuf>) -> Chain {
    resolve_chain(
        explicit,
        std::env::var_os(ENV_CONFIG).map(PathBuf::from),
        home_dir(),
        home_config_file(),
        xdg_config_file(),
    )
}

/// The files [`crate::Config::load`] reads, and what its messages say about them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Chain {
    /// Config files, lowest precedence first.
    pub(crate) layers: Vec<PathBuf>,
    /// The unread INI `~/.mtuirc`, looked for on the default chain only.
    pub(crate) legacy_rc: Option<PathBuf>,
    pub(crate) source: ConfigSource,
}

/// Pure core of [`config_chain`], with the process environment injected so no
/// test has to change it. `home_file` is `~/.mtui.toml`, `xdg` the XDG file.
fn resolve_chain(
    explicit: Option<PathBuf>,
    env_config: Option<PathBuf>,
    home: Option<PathBuf>,
    home_file: Option<PathBuf>,
    xdg: Option<PathBuf>,
) -> Chain {
    let explicit = explicit.or_else(|| {
        env_config
            .filter(|env| !env.as_os_str().is_empty())
            .map(|env| expand_home(&env, home.as_deref()))
    });
    let legacy_rc = match explicit {
        Some(_) => None,
        None => home.as_ref().map(|h| h.join(".mtuirc")),
    };
    Chain {
        layers: resolve_search_paths(explicit.clone(), home_file, xdg.clone()),
        legacy_rc,
        source: ConfigSource {
            explicit,
            user_file: xdg,
            home,
        },
    }
}

/// The layers of [`resolve_chain`]: the explicit file alone, else the default
/// chain.
fn resolve_search_paths(
    explicit: Option<PathBuf>,
    home: Option<PathBuf>,
    xdg: Option<PathBuf>,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path];
    }

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

    // Resolution is tested through the pure `resolve_chain` core so no test
    // changes the process environment (`unsafe` under edition 2024, and racy
    // across threads).

    const HOME: &str = "/home/u";
    const HOME_FILE: &str = "/home/u/.mtui.toml";
    const XDG: &str = "/home/u/.config/mtui/mtui.toml";

    fn chain_for(explicit: Option<&str>, env_config: Option<&str>) -> Chain {
        resolve_chain(
            explicit.map(PathBuf::from),
            env_config.map(PathBuf::from),
            Some(HOME.into()),
            Some(HOME_FILE.into()),
            Some(XDG.into()),
        )
    }

    fn explicit_chain(file: &str) -> Chain {
        Chain {
            layers: vec![file.into()],
            legacy_rc: None,
            source: ConfigSource {
                explicit: Some(file.into()),
                user_file: Some(XDG.into()),
                home: Some(HOME.into()),
            },
        }
    }

    #[test]
    fn the_flag_wins_over_the_env_var_and_replaces_the_chain() {
        assert_eq!(
            chain_for(Some("/flag.toml"), Some("/env.toml")),
            explicit_chain("/flag.toml")
        );
    }

    #[test]
    fn the_env_var_alone_replaces_the_chain_with_tilde_expanded() {
        assert_eq!(
            chain_for(None, Some("/env.toml")),
            explicit_chain("/env.toml")
        );
        assert_eq!(
            chain_for(None, Some("~/my.toml")),
            explicit_chain("/home/u/my.toml")
        );
    }

    #[test]
    fn an_empty_or_unset_env_var_gives_the_default_chain_and_its_legacy_rc() {
        let default_chain = Chain {
            layers: vec![ETC_CONFIG.into(), HOME_FILE.into(), XDG.into()],
            legacy_rc: Some(PathBuf::from(HOME).join(".mtuirc")),
            source: ConfigSource {
                explicit: None,
                user_file: Some(XDG.into()),
                home: Some(HOME.into()),
            },
        };
        assert_eq!(chain_for(None, Some("")), default_chain);
        assert_eq!(chain_for(None, None), default_chain);
    }

    #[test]
    fn no_home_means_no_legacy_rc_and_no_tilde_expansion() {
        let chain = resolve_chain(None, Some("~/my.toml".into()), None, None, None);
        assert_eq!(chain.layers, vec![PathBuf::from("~/my.toml")]);
        assert_eq!(chain.legacy_rc, None);
        assert_eq!(resolve_chain(None, None, None, None, None).legacy_rc, None);
    }

    #[test]
    fn defaults_are_etc_then_home_then_xdg() {
        let home = PathBuf::from("/home/u/.mtui.toml");
        let xdg = PathBuf::from("/home/u/.config/mtui/mtui.toml");
        let paths = resolve_search_paths(None, Some(home.clone()), Some(xdg.clone()));
        assert_eq!(
            paths,
            vec![PathBuf::from(ETC_CONFIG), home, xdg],
            "order must be etc → home → xdg"
        );
    }

    #[test]
    fn defaults_home_only_when_no_xdg() {
        let home = PathBuf::from("/home/u/.mtui.toml");
        let paths = resolve_search_paths(None, Some(home.clone()), None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG), home]);
    }

    #[test]
    fn defaults_without_home_or_xdg_is_etc_only() {
        let paths = resolve_search_paths(None, None, None);
        assert_eq!(paths, vec![PathBuf::from(ETC_CONFIG)]);
    }

    #[test]
    fn public_wrapper_returns_etc_and_optional_xdg() {
        // The public entry returns an explicit path regardless of ambient env.
        let chain = config_chain(Some(PathBuf::from("/x.toml")));
        assert_eq!(chain.layers, vec![PathBuf::from("/x.toml")]);
        assert_eq!(chain.source.explicit, Some(PathBuf::from("/x.toml")));
        assert_eq!(chain.legacy_rc, None);
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
        // The filename is always `mtui.toml` / `.mtui.toml`, never `config.toml`.
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
        // The data dir must be the `mtui` subdir, so history lands under it.
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
        assert_eq!(
            expanduser(Path::new("/abs/path")),
            PathBuf::from("/abs/path")
        );
    }
}
