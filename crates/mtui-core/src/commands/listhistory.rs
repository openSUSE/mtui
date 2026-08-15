//! The `list_history` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_types::system::System;

use super::support::{add_hosts_arg, per_host, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// The event types `-e/--event` accepts.
const EVENTS: [&str; 6] = [
    "connect",
    "disconnect",
    "install",
    "update",
    "downgrade",
    "prepare",
];

/// Lists the history of mtui events recorded on the reference hosts.
///
/// Fetches the tail of `/var/log/mtui.log` (optionally `grep`-filtered by
/// `-e/--event`) from each selected host, then renders each host's
/// `when:who:event` lines through the display's `list_history` sink.
/// Selection runs with disabled hosts included.
pub struct ListHistory;

/// Builds the log-fetch command for `count` entries and optional `events`.
fn history_command(count: usize, events: &[String]) -> String {
    if events.is_empty() {
        format!("tail -n {count} /var/log/mtui.log")
    } else {
        let grep_args = events
            .iter()
            .map(|e| format!("-e \":{e}\""))
            .collect::<Vec<_>>()
            .join(" ");
        format!("tac /var/log/mtui.log | grep -m {count} {grep_args} | tac")
    }
}

#[async_trait]
impl Command for ListHistory {
    fn name(&self) -> &'static str {
        "list_history"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists the history of mtui events recorded on the reference hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("event")
                .short('e')
                .long("event")
                .action(ArgAction::Append)
                .value_parser(clap::builder::PossibleValuesParser::new(EVENTS))
                .help("event type to list (repeatable)"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        EVENTS
            .into_iter()
            .map(str::to_owned)
            .chain(session.targets().names())
            .filter(|c| c.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let events: Vec<String> = args
            .try_get_many::<String>("event")
            .ok()
            .flatten()
            .map(|it| it.cloned().collect())
            .unwrap_or_default();

        let targets = session.targets_mut();
        // enabled=false: history is read even from disabled hosts.
        let hosts =
            select_names(targets, args, false).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }

        // Caps at 50 entries, dropping to 10 once 3+ hosts are queried.
        let count = if hosts.len() >= 3 { 10 } else { 50 };
        targets
            .run(per_host(&history_command(count, &events), &hosts))
            .await;

        let rows: Vec<(String, System, Vec<String>)> = hosts
            .iter()
            .filter_map(|name| {
                targets.get(name).map(|t| {
                    let lines = t.lastout().split('\n').map(str::to_owned).collect();
                    (name.clone(), t.system().clone(), lines)
                })
            })
            .collect();
        for (name, system, lines) in rows {
            session.display.list_history(&name, &system, &lines);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListHistory.name(), "list_history");
        assert_eq!(ListHistory.scope(), Scope::Fanout);
    }

    #[test]
    fn history_command_tail_vs_grep() {
        assert_eq!(history_command(50, &[]), "tail -n 50 /var/log/mtui.log");
        assert_eq!(
            history_command(10, &["install".to_owned()]),
            "tac /var/log/mtui.log | grep -m 10 -e \":install\" | tac"
        );
    }

    #[test]
    fn prepare_is_a_selectable_event() {
        // `prepare` writes its own `/var/log/mtui.log` rows (#407), so it must
        // be selectable like every other op: the value parser rejects any
        // event not in `EVENTS`, and `matches` panics on a rejected argv.
        let args = matches(&ListHistory, &["-e", "prepare"]);
        let events: Vec<String> = args
            .get_many::<String>("event")
            .expect("the event arg parsed")
            .cloned()
            .collect();
        assert_eq!(events, vec!["prepare".to_owned()]);
        assert_eq!(
            history_command(10, &["prepare".to_owned()]),
            "tac /var/log/mtui.log | grep -m 10 -e \":prepare\" | tac"
        );
        // And it is offered by tab-completion, where the prefix is ambiguous
        // with nothing else in the list.
        let (session, _buf) = empty_session();
        assert_eq!(
            ListHistory.complete(&session, "prep", "list_history -e prep"),
            vec!["prepare".to_owned()]
        );
    }

    #[tokio::test]
    async fn renders_history_lines() {
        let (mut session, buf) = session_with_hosts(
            "SUSE:Maintenance:1:1",
            &["h1"],
            "1678886400:user:test command\n",
        );
        let args = matches(&ListHistory, &["-t", "h1"]);
        ListHistory.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("history from h1"), "{out}");
        assert!(out.contains("test command"), "{out}");
    }

    #[tokio::test]
    async fn no_hosts_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ListHistory, &[]);
        let err = ListHistory.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }
}
