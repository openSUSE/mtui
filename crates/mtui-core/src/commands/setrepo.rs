//! The `set_repo` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches};
use mtui_hosts::RepoOp;

use super::support::add_hosts_arg;
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Adds or removes an issue repository to or from hosts.
///
/// Ports upstream `mtui.commands.setrepo.SetRepo`. The mutually-exclusive
/// (and required) `-A/--add` vs `-R/--remove` selects the operation, which is
/// fanned out over the selected hosts via
/// [`HostsGroup::fanout_set_repo`](mtui_hosts::HostsGroup) driven by the active
/// report's [`SetRepo`](mtui_hosts::SetRepo) impl.
pub struct SetRepo;

#[async_trait]
impl Command for SetRepo {
    fn name(&self) -> &'static str {
        "set_repo"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Adds or removes an issue repository to or from hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("add")
                    .short('A')
                    .long("add")
                    .action(ArgAction::SetTrue)
                    .help("Add issue repos to refhosts"),
            )
            .arg(
                Arg::new("remove")
                    .short('R')
                    .long("remove")
                    .action(ArgAction::SetTrue)
                    .help("Remove issue repos from refhosts"),
            )
            .group(
                ArgGroup::new("operation")
                    .args(["add", "remove"])
                    .required(true),
            )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let operation = if args.get_flag("add") {
            RepoOp::Add
        } else {
            RepoOp::Remove
        };

        let hosts = super::support::hosts_arg(args);
        let names = match &hosts {
            Some(names) if !names.is_empty() && !names.iter().any(|h| h == "all") => {
                Some(names.as_slice())
            }
            _ => None,
        };
        // Split rather than select: a `-t` subset operation must preserve the
        // unselected hosts in the live report (see `Session::split_targets`).
        let (mut selected, remainder) = match session.split_targets(names) {
            Ok(split) => split,
            Err(e) => return Err(CommandError::Other(e.to_string())),
        };
        if selected.is_empty() {
            session.restore_split_targets(selected, remainder);
            return Err(CommandError::NoRefhostsDefined);
        }

        // The active report must be able to set repos (SL/PI/OBS). The null
        // report cannot, which mirrors upstream's `@requires_update` guard.
        let result = match session.metadata().as_set_repo() {
            Some(set_repo) => {
                selected.fanout_set_repo(operation, set_repo).await;
                Ok(())
            }
            None => Err(CommandError::Other("No update loaded".to_owned())),
        };
        session.restore_split_targets(selected, remainder);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(SetRepo.name(), "set_repo");
        assert_eq!(SetRepo.scope(), Scope::Fanout);
    }

    #[test]
    fn operation_group_is_required() {
        let base = clap::Command::new("set_repo").no_binary_name(true);
        // Neither -A nor -R → required group rejects.
        assert!(
            SetRepo
                .configure(base)
                .try_get_matches_from([""; 0])
                .is_err()
        );
    }

    #[test]
    fn add_and_remove_are_mutually_exclusive() {
        let base = clap::Command::new("set_repo").no_binary_name(true);
        assert!(
            SetRepo
                .configure(base)
                .try_get_matches_from(["-A", "-R"])
                .is_err()
        );
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&SetRepo, &["-A"]);
        let err = SetRepo.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn report_without_set_repo_capability_errors() {
        // FakeReport does not implement `as_set_repo` (returns None), mirroring
        // the null/unloaded report — upstream's `@requires_update` guard.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&SetRepo, &["-A"]);
        let err = SetRepo.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(m) if m == "No update loaded"));
        // The group is restored even on the capability-miss path.
        assert_eq!(session.targets().names(), vec!["h1"]);
    }

    #[tokio::test]
    async fn set_repo_t_subset_keeps_unselected_host() {
        // A `-t` subset must not drop the unselected host, even when the op
        // itself no-ops on the capability miss: split+merge preserves h2.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "ok");
        let args = matches(&SetRepo, &["-A", "-t", "h1"]);
        let _ = SetRepo.call(&mut session, &args).await;
        assert_eq!(
            session.targets().names(),
            vec!["h1".to_owned(), "h2".to_owned()]
        );
    }
}
