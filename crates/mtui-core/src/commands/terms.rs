//! The `terms` command.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Spawns terminal screens onto the connected reference hosts.
///
/// Ports upstream `mtui.commands.terms.Terms`. With no `termname`, prints the
/// list of available terminal-launcher scripts; with a name, runs
/// `term.<name>.sh` from [`terms_path`](mtui_config::terms_path), passing the
/// selected (sorted) host names as arguments.
///
/// The available term names are derived **from disk** by globbing `term.*.sh`
/// in the terms directory (mirroring upstream's dynamic `_list_terms`), rather
/// than from a config key. The launcher script spawns its own detached terminal
/// emulators and does not need mtui's controlling terminal, so — unlike `edit`
/// and `shell` — the spawn runs directly in [`call`](Terms::call).
///
/// Deviation from upstream: the not-found path emits a single clean error rather
/// than also logging the (typo'd) "Aviable term scripts" info line.
///
/// REPL-oriented — on the MCP deny-list.
pub struct Terms;

/// Prefix and suffix stripped from `term.<name>.sh` to recover `<name>`.
const TERM_PREFIX: &str = "term.";
const TERM_SUFFIX: &str = ".sh";

/// Discover the available term names by globbing `term.*.sh` in `dir`.
///
/// Strips the `term.` prefix and `.sh` suffix from each match and returns the
/// names sorted. A missing or unreadable directory yields an empty list
/// (graceful degrade — upstream lists "available terminals" and no-ops when the
/// script dir is absent).
fn discover_term_names(dir: Option<&Path>) -> Vec<String> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix(TERM_PREFIX)
                .and_then(|n| n.strip_suffix(TERM_SUFFIX))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// Build the argv for a term-script spawn: `[<dir>/term.<name>.sh, <hosts...>]`.
///
/// Factored out so the argument construction is unit-testable without spawning.
fn term_script_argv(dir: &Path, name: &str, hosts: &[String]) -> (PathBuf, Vec<String>) {
    let script = dir.join(format!("{TERM_PREFIX}{name}{TERM_SUFFIX}"));
    (script, hosts.to_vec())
}

/// Render the no-argument listing: a header line followed by the space-joined
/// term names (matching upstream's two `println` calls). Factored out so the
/// exact text is snapshot-testable with deterministic names.
fn render_listing(names: &[String]) -> String {
    format!("available terminals scripts:\n{}\n", names.join(" "))
}

#[async_trait]
impl Command for Terms {
    fn name(&self) -> &'static str {
        "terms"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Spawn terminal screens onto the connected hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Single
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("termname")
                .num_args(0..=1)
                .value_name("TERMNAME")
                .help("Terminal emulator script to spawn consoles with"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        let dir = mtui_config::terms_path();
        let mut out: Vec<String> = discover_term_names(dir.as_deref())
            .into_iter()
            .filter(|n| n.starts_with(text))
            .collect();
        out.extend(
            session
                .targets()
                .names()
                .into_iter()
                .filter(|n| n.starts_with(text)),
        );
        for flag in ["-t", "--target"] {
            if flag.starts_with(text) {
                out.push(flag.to_owned());
            }
        }
        out
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let dir = mtui_config::terms_path();
        let names = discover_term_names(dir.as_deref());

        let Some(termname) = args.get_one::<String>("termname") else {
            // No name: list the available term scripts.
            session.display.print_eol(&render_listing(&names), "");
            return Ok(());
        };

        if !names.iter().any(|n| n == termname) {
            return Err(CommandError::Other(format!(
                "term script not found: {termname}"
            )));
        }

        let mut hosts = select_names(session.targets(), args, true)
            .map_err(|e| CommandError::Other(e.to_string()))?;
        hosts.sort();

        // `names` being non-empty implies the dir resolved.
        let dir = dir.expect("terms_path resolved because a term script was found");
        let (script, argv) = term_script_argv(&dir, termname, &hosts);

        match tokio::process::Command::new(&script)
            .args(&argv)
            .status()
            .await
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => {
                tracing::error!(script = %script.display(), %status, "term script exited non-zero");
                Ok(())
            }
            Err(err) => {
                tracing::error!(script = %script.display(), error = %err, "running term script failed");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    fn seed_terms_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("term.xterm.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.path().join("term.gnome.sh"), "#!/bin/sh\n").unwrap();
        // Non-matching files are ignored.
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        std::fs::write(dir.path().join("term.broken"), "x").unwrap();
        dir
    }

    #[test]
    fn name_and_single_scope() {
        assert_eq!(Terms.name(), "terms");
        assert_eq!(Terms.scope(), Scope::Single);
    }

    #[test]
    fn discover_strips_affixes_and_sorts() {
        let dir = seed_terms_dir();
        let names = discover_term_names(Some(dir.path()));
        assert_eq!(names, vec!["gnome".to_owned(), "xterm".to_owned()]);
    }

    #[test]
    fn discover_missing_dir_is_empty() {
        assert!(discover_term_names(None).is_empty());
        assert!(discover_term_names(Some(Path::new("/no/such/terms/dir"))).is_empty());
    }

    #[test]
    fn argv_builds_script_path_and_host_args() {
        let (script, argv) = term_script_argv(
            Path::new("/data/terms"),
            "xterm",
            &["h1".to_owned(), "h2".to_owned()],
        );
        assert_eq!(script, PathBuf::from("/data/terms/term.xterm.sh"));
        assert_eq!(argv, vec!["h1".to_owned(), "h2".to_owned()]);
    }

    #[tokio::test]
    async fn no_termname_lists_available_header() {
        // The listing pulls from the real (likely absent) terms dir; the header
        // line is the stable contract regardless of installed scripts.
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Terms, &[]);
        Terms.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("available terminals scripts:"));
    }

    #[test]
    fn listing_matches_upstream_two_line_format() {
        // Deterministic snapshot of the exact no-arg output with seeded names.
        let names = vec!["gnome".to_owned(), "xterm".to_owned()];
        insta::assert_snapshot!(render_listing(&names), @r"
        available terminals scripts:
        gnome xterm
        ");
    }

    #[tokio::test]
    async fn missing_termname_errors_cleanly() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        // A name that cannot exist in any real terms dir.
        let args = matches(&Terms, &["definitely-not-a-real-term"]);
        let err = Terms.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m.contains("term script not found")));
    }

    #[test]
    fn complete_offers_host_names_and_target_flag() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let candidates = Terms.complete(&session, "h", "terms h");
        assert!(candidates.contains(&"h1".to_owned()));
        assert!(candidates.contains(&"h2".to_owned()));

        let flags = Terms.complete(&session, "-", "terms -");
        assert!(flags.contains(&"-t".to_owned()) || flags.contains(&"--target".to_owned()));
    }

    #[test]
    fn complete_on_empty_session_does_not_panic() {
        let (session, _buf) = empty_session();
        let _ = Terms.complete(&session, "", "terms ");
    }
}
