//! The `edit` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::complete_path;
use crate::command::Command;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Edits the active testing template or a local file in `$EDITOR`.
///
/// Ports upstream `mtui.commands.edit.Edit`. Spawning `$EDITOR` (default `vim`)
/// on the controlling terminal needs the local TTY, which only the Phase-6
/// `mtui` binary owns; the command surface (name, optional `filename`, file-path
/// completion) is ported here so the registry and MCP synthesiser see it, but
/// the runtime editor spawn is intercepted in `crates/mtui-cli/src/edit.rs`
/// (like `shell`) — the shared engine, which the headless MCP also drives, has
/// no controlling terminal. Invoked headlessly it errors cleanly rather than
/// hanging. REPL-only — on the MCP deny-list.
pub struct Edit;

#[async_trait]
impl Command for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Edit the active testing template or a local file in $EDITOR.")
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        cmd.arg(
            Arg::new("filename")
                .num_args(0..=1)
                .value_name("FILENAME")
                .help("File to edit (defaults to the active template)"),
        )
    }

    fn complete(&self, _session: &Session, text: &str, _line: &str) -> Vec<String> {
        complete_path(text)
    }

    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        // The editor attaches to the controlling terminal, which only the REPL
        // owns; the CLI intercepts the `edit` line before dispatch and spawns
        // `$EDITOR` there. Headless callers get a clean error instead of a hang.
        Err(CommandError::Other(
            "interactive editor is not available in this mode (REPL only)".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches};

    #[test]
    fn name_is_edit() {
        assert_eq!(Edit.name(), "edit");
    }

    #[test]
    fn has_about() {
        assert!(Edit.about().is_some());
    }

    #[test]
    fn configure_accepts_bare_and_filename() {
        // `matches` builds the parser the same way the engine does, so this
        // exercises the real arg grammar for both the bare and one-arg forms.
        let bare = matches(&Edit, &[]);
        assert_eq!(bare.get_one::<String>("filename"), None);
        let with = matches(&Edit, &["foo.txt"]);
        assert_eq!(
            with.get_one::<String>("filename").map(String::as_str),
            Some("foo.txt")
        );
    }

    #[tokio::test]
    async fn headless_call_errors_cleanly() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Edit, &[]);
        let err = Edit.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("not available")));
    }

    #[test]
    fn complete_lists_matching_entries_in_a_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.txt"), "x").unwrap();
        std::fs::write(dir.path().join("beta.txt"), "y").unwrap();
        std::fs::create_dir(dir.path().join("apex")).unwrap();

        let base = format!("{}/", dir.path().display());
        let (session, _buf) = empty_session();

        let all = Edit.complete(&session, &base, "");
        assert!(all.iter().any(|c| c.ends_with("alpha.txt")));
        assert!(all.iter().any(|c| c.ends_with("beta.txt")));
        // Directories carry a trailing slash.
        assert!(all.iter().any(|c| c.ends_with("apex/")));

        // Prefix filters and preserves the directory anchor.
        let a = Edit.complete(&session, &format!("{base}a"), "");
        assert!(a.iter().all(|c| c.contains("/a")));
        assert!(a.iter().any(|c| c.ends_with("alpha.txt")));
        assert!(a.iter().any(|c| c.ends_with("apex/")));
        assert!(!a.iter().any(|c| c.ends_with("beta.txt")));
    }

    #[test]
    fn complete_unreadable_dir_is_empty() {
        let (session, _buf) = empty_session();
        assert!(Edit.complete(&session, "/no/such/dir/x", "").is_empty());
    }
}
