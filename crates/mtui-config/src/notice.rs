//! What loading the config files ignored, kept as data so each surface renders
//! the paths in its own form.

use std::path::{Path, PathBuf};

/// Unknown keys named per file before the rest are only counted, so a wrong
/// file cannot flood the output.
pub const MAX_NOTICES_PER_FILE: usize = 20;

/// Characters of a key name kept in a notice.
const MAX_KEY_CHARS: usize = 100;

/// One thing loading ignored. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigNotice {
    /// A key no option reads; `table` is `None` at the top level.
    UnknownKey {
        file: PathBuf,
        table: Option<String>,
        key: String,
    },
    /// A table (or array of tables) no option reads, named once.
    UnknownTable { file: PathBuf, table: String },
    /// The unknown keys of `file` past [`MAX_NOTICES_PER_FILE`].
    MoreUnknown { file: PathBuf, count: usize },
    /// The INI `~/.mtuirc`, which mtui does not read; `user_file` is where its
    /// settings belong.
    LegacyRc {
        path: PathBuf,
        user_file: Option<PathBuf>,
    },
}

impl ConfigNotice {
    /// The notice as one line. With `home`, a path under it is shown as `~/…`.
    #[must_use]
    pub fn render(&self, home: Option<&Path>) -> String {
        let p = |path: &Path| shown_path(path, home);
        match self {
            Self::UnknownKey {
                file,
                table: Some(table),
                key,
            } => format!("unknown key [{table}] {key} in {}; ignored", p(file)),
            Self::UnknownKey {
                file,
                table: None,
                key,
            } => format!("unknown key {key} in {}; ignored", p(file)),
            Self::UnknownTable { file, table } => {
                format!("unknown table [{table}] in {}; ignored", p(file))
            }
            Self::MoreUnknown { file, count } => {
                format!(
                    "{count} more unknown keys in {} not named; ignored",
                    p(file)
                )
            }
            Self::LegacyRc { path, user_file } => format!(
                "{} is not read: mtui reads only TOML config files; put its settings {}; \
                 see {}configuration.html",
                p(path),
                in_file(user_file.as_deref(), home),
                env!("CARGO_PKG_HOMEPAGE")
            ),
        }
    }
}

/// Which config file is in use, for messages that tell the operator where to
/// set an option.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigSource {
    /// The single file named by `--config` or `$MTUI_CONF`.
    pub explicit: Option<PathBuf>,
    /// The per-user file of the default chain (`$XDG_CONFIG_HOME/mtui/mtui.toml`).
    pub user_file: Option<PathBuf>,
    /// The home directory paths are shown relative to on headless output.
    pub home: Option<PathBuf>,
}

impl ConfigSource {
    /// `in <file>` naming the file to set an option in, under `~` when it lies
    /// in the home directory; `in your mtui.toml` when none resolves.
    #[must_use]
    pub fn edit_location(&self) -> String {
        in_file(
            self.explicit.as_deref().or(self.user_file.as_deref()),
            self.home.as_deref(),
        )
    }
}

fn in_file(file: Option<&Path>, home: Option<&Path>) -> String {
    file.map_or_else(
        || "in your mtui.toml".to_owned(),
        |f| format!("in {}", shown_path(f, home)),
    )
}

/// `path`, as `~/…` when it lies under `home`.
fn shown_path(path: &Path, home: Option<&Path>) -> String {
    match home.map(|h| path.strip_prefix(h)) {
        Some(Ok(rest)) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(Ok(rest)) => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// A key name as a notice may print it: no control or bidi characters, and
/// at most [`MAX_KEY_CHARS`] characters.
pub(crate) fn clean_key(key: &str) -> String {
    let mut kept = key.chars().filter(|&c| {
        !(c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
    });
    let mut out: String = kept.by_ref().take(MAX_KEY_CHARS).collect();
    if kept.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/tester";

    fn home() -> Option<&'static Path> {
        Some(Path::new(HOME))
    }

    #[test]
    fn headless_form_puts_home_paths_under_tilde_and_keeps_the_rest() {
        let notices = [
            ConfigNotice::UnknownKey {
                file: "/home/tester/.config/mtui/mtui.toml".into(),
                table: Some("mtui".into()),
                key: "tempdir".into(),
            },
            ConfigNotice::UnknownKey {
                file: "/etc/mtui.toml".into(),
                table: None,
                key: "stray".into(),
            },
            ConfigNotice::UnknownTable {
                file: "/srv/conf/mtui.toml".into(),
                table: "svn".into(),
            },
            ConfigNotice::MoreUnknown {
                file: "/home/tester/.mtui.toml".into(),
                count: 3,
            },
            ConfigNotice::LegacyRc {
                path: "/home/tester/.mtuirc".into(),
                user_file: Some("/home/tester/.config/mtui/mtui.toml".into()),
            },
        ];
        let headless: Vec<String> = notices.iter().map(|n| n.render(home())).collect();
        assert_eq!(
            headless,
            [
                "unknown key [mtui] tempdir in ~/.config/mtui/mtui.toml; ignored",
                "unknown key stray in /etc/mtui.toml; ignored",
                "unknown table [svn] in /srv/conf/mtui.toml; ignored",
                "3 more unknown keys in ~/.mtui.toml not named; ignored",
                "~/.mtuirc is not read: mtui reads only TOML config files; put its \
                 settings in ~/.config/mtui/mtui.toml; see \
                 https://opensuse.github.io/mtui/configuration.html",
            ]
        );
        assert_eq!(
            notices[0].render(None),
            "unknown key [mtui] tempdir in /home/tester/.config/mtui/mtui.toml; ignored"
        );
        assert_eq!(
            notices[4].render(None),
            "/home/tester/.mtuirc is not read: mtui reads only TOML config files; put \
             its settings in /home/tester/.config/mtui/mtui.toml; see \
             https://opensuse.github.io/mtui/configuration.html"
        );
    }

    #[test]
    fn a_sibling_of_home_is_not_shown_under_tilde() {
        assert_eq!(
            shown_path(Path::new("/home/tester2/mtui.toml"), home()),
            "/home/tester2/mtui.toml"
        );
    }

    #[test]
    fn edit_location_names_the_explicit_file_then_the_user_file_then_none() {
        let mut source = ConfigSource {
            explicit: Some("/home/tester/conf/qa.toml".into()),
            user_file: Some("/home/tester/.config/mtui/mtui.toml".into()),
            home: home().map(Path::to_path_buf),
        };
        assert_eq!(source.edit_location(), "in ~/conf/qa.toml");
        source.explicit = Some("/srv/qa.toml".into());
        assert_eq!(source.edit_location(), "in /srv/qa.toml");
        source.explicit = None;
        assert_eq!(source.edit_location(), "in ~/.config/mtui/mtui.toml");
        source.user_file = None;
        assert_eq!(source.edit_location(), "in your mtui.toml");
    }

    #[test]
    fn clean_key_drops_terminal_controls_and_caps_the_length() {
        assert_eq!(clean_key("evil\u{1b}[31m\u{202e}key\r"), "evil[31mkey");
        let long = "k".repeat(MAX_KEY_CHARS + 5);
        assert_eq!(clean_key(&long), format!("{}…", "k".repeat(MAX_KEY_CHARS)));
        assert_eq!(
            clean_key(&"k".repeat(MAX_KEY_CHARS)),
            "k".repeat(MAX_KEY_CHARS)
        );
    }
}
