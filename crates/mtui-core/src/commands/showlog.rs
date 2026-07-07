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

    fn scope(&self) -> Scope {
        Scope::Fanout
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

        let interactive = session.interactive;
        let mut writer = |line: &str| session.display.println(line);
        page(&output, interactive, Some(&mut writer));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{matches, session_scripting};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ShowLog.name(), "show_log");
        assert_eq!(ShowLog.scope(), Scope::Fanout);
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
