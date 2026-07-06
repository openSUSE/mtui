//! The `install` and `uninstall` commands.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};

use super::perform::{PerformOp, drive};
use super::support::add_hosts_arg;
use crate::command::{Command, Scope};
use crate::error::CommandResult;
use crate::session::Session;

/// Reads the required `package` positional list.
fn packages(args: &ArgMatches) -> Vec<String> {
    args.get_many::<String>("package")
        .map(|it| it.cloned().collect())
        .unwrap_or_default()
}

/// Adds the shared `package …` positional (`nargs="+"`) argument.
fn add_package_arg(cmd: clap::Command, help: &'static str) -> clap::Command {
    cmd.arg(
        Arg::new("package")
            .num_args(1..)
            .required(true)
            .action(ArgAction::Append)
            .value_name("PACKAGE")
            .help(help),
    )
}

/// Installs packages from the current active repositories.
///
/// Ports upstream `mtui.commands.zypper.Install`. Drives
/// [`TestReport::perform_install`](mtui_testreport::TestReport::perform_install)
/// over the selected hosts.
pub struct Install;

#[async_trait]
impl Command for Install {
    fn name(&self) -> &'static str {
        "install"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(add_package_arg(cmd, "package to install"))
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        drive(session, args, PerformOp::Install(packages(args))).await
    }
}

/// Removes packages from the system.
///
/// Ports upstream `mtui.commands.zypper.Uninstall`. Drives
/// [`TestReport::perform_uninstall`](mtui_testreport::TestReport::perform_uninstall).
pub struct Uninstall;

#[async_trait]
impl Command for Uninstall {
    fn name(&self) -> &'static str {
        "uninstall"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(add_package_arg(cmd, "package to remove"))
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        drive(session, args, PerformOp::Uninstall(packages(args))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};
    use crate::error::CommandError;

    #[test]
    fn names_and_scopes() {
        assert_eq!(Install.name(), "install");
        assert_eq!(Uninstall.name(), "uninstall");
        assert_eq!(Install.scope(), Scope::Fanout);
        assert_eq!(Uninstall.scope(), Scope::Fanout);
    }

    #[test]
    fn package_is_required() {
        // No positional package → clap rejects (mirrors nargs="+").
        let base = clap::Command::new("install").no_binary_name(true);
        assert!(
            Install
                .configure(base)
                .try_get_matches_from([""; 0])
                .is_err()
        );
    }

    #[test]
    fn parses_multiple_packages() {
        let args = matches(&Install, &["vim", "less"]);
        assert_eq!(packages(&args), vec!["vim".to_owned(), "less".to_owned()]);
    }

    #[tokio::test]
    async fn install_over_loaded_report_succeeds() {
        // FakeReport's perform_install is the trait no-op; the command plumbing
        // (selection, restore) is what is exercised here.
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Install, &["pkg"]);
        Install.call(&mut session, &args).await.unwrap();
        // Group restored after the op.
        assert_eq!(session.targets().names(), vec!["h1"]);
    }

    #[tokio::test]
    async fn install_with_no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Install, &["pkg"]);
        let err = Install.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn uninstall_unknown_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Uninstall, &["-t", "ghost", "pkg"]);
        let err = Uninstall.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
