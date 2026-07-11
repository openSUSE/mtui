//! The `show_log` command.

use async_trait::async_trait;
use clap::ArgMatches;

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::display::{CommandPromptDisplay, page};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Prints the command protocol (issued commands + output) from the hosts.
///
/// Ports upstream `mtui.commands.simplelists.ListLog` (`show_log`), which fans
/// each host's log through `display.show_log` into an accumulator and pages the
/// result. Useful for dumping the command history into a template's reproducer
/// section. The per-host command log is snapshotted first so the report borrow
/// does not overlap the display borrow.
pub struct ShowLog;

#[async_trait]
impl Command for ShowLog {
    fn name(&self) -> &'static str {
        "show_log"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Prints the command protocol (issued commands + output) from the hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    /// `show_log` opts out of the driver's host-less skip: it reports each host's
    /// *in-memory* command protocol (`Target::out`), doing no SSH, so it has
    /// meaningful (or harmlessly empty) work even at zero connected hosts. Like
    /// `export`, dumping the protocol across `--all-templates` must not be
    /// silently skipped when a template is host-less. A host-action command keeps
    /// the default `true`; only these local-read commands override it.
    fn skip_hostless_templates(&self) -> bool {
        false
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        session
            .targets()
            .names()
            .into_iter()
            .filter(|n| n.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let hosts = select_names(session.targets(), args, true)
            .map_err(|e| CommandError::Other(e.to_string()))?;

        // Snapshot each host's command log into the display's tuple shape.
        let per_host: Vec<(String, Vec<(String, String, String, i32)>)> = hosts
            .iter()
            .filter_map(|name| {
                session.targets().get(name).map(|t| {
                    let entries = t
                        .out()
                        .iter()
                        .map(|c| {
                            (
                                c.command.clone(),
                                c.stdout.clone(),
                                c.stderr.clone(),
                                i32::from(c.exitcode),
                            )
                        })
                        .collect();
                    (name.clone(), entries)
                })
            })
            .collect();

        let mut output: Vec<String> = Vec::new();
        for (name, entries) in &per_host {
            let mut sink = |line: &str| output.push(line.to_owned());
            CommandPromptDisplay::show_log(name, entries, &mut sink);
        }

        let is_repl = session.is_repl;
        let mut writer = |line: &str| session.display.println(line);
        page(&output, is_repl, Some(&mut writer));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::listversions::ListVersions;
    use crate::commands::testkit::{empty_session, fake_report, matches, session_scripting};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ShowLog.name(), "show_log");
        assert_eq!(ShowLog.scope(), Scope::Fanout);
    }

    #[test]
    fn opts_out_of_hostless_skip() {
        // show_log reads the in-memory protocol; it must dispatch at zero hosts.
        assert!(!ShowLog.skip_hostless_templates());
    }

    #[test]
    fn ssh_dependent_fanout_command_keeps_default_skip() {
        // Negative control: the audit deliberately left SSH-driven Fanout
        // commands skippable. If this flips, re-run the host-less audit.
        assert!(ListVersions.skip_hostless_templates());
    }

    #[tokio::test]
    async fn runs_across_all_hostless_templates_without_error() {
        // Every loaded template is host-less and no `-t` is named: the driver
        // would skip a default host-action command (→ NoRefhostsDefined), but
        // show_log opts out and must run on each, returning Ok. A headless
        // session with >1 loaded template fans out without an explicit flag.
        let (mut session, _buf) = empty_session();
        session
            .templates
            .add(fake_report("SUSE:Maintenance:1:1", &[], ""));
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &[], ""));
        let args = matches(&ShowLog, &[]);
        ShowLog.run(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn shows_ran_command_log() {
        // session_scripting echoes the command into the host log.
        let (mut session, buf) =
            session_scripting("SUSE:Maintenance:1:1", "h1", "uname -a", "Linux\n");
        session.targets_mut().run("uname -a").await;
        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("log from h1"), "{out}");
        assert!(out.contains("uname -a"), "{out}");
    }
}
