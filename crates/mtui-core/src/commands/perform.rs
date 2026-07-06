//! Shared driver for the `perform_*` workflow commands.
//!
//! `install`, `uninstall`, `prepare`, `downgrade`, and `update` all follow the
//! same shape: resolve the `-t` host selection, then drive one of the active
//! report's `perform_*` flows over the selected [`HostsGroup`].
//!
//! Those flows take `&self` (the report) **and** `&mut HostsGroup` at once, and
//! the group lives inside the report — so the body moves the group out
//! ([`Session::take_targets`](crate::Session::take_targets)), re-borrows the
//! report immutably, drives the op, and restores the group. Modelling the op as
//! an enum (rather than a borrowing async closure) keeps the future `Send`
//! without wrestling higher-ranked lifetimes.

use clap::ArgMatches;
use mtui_hosts::HostsGroup;

use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// One of the report's `perform_*` workflow flows plus its parsed parameters.
pub(super) enum PerformOp {
    /// `perform_install(packages)`.
    Install(Vec<String>),
    /// `perform_uninstall(packages)`.
    Uninstall(Vec<String>),
    /// `perform_prepare(packages, force, testing, installed_only)`.
    Prepare {
        packages: Vec<String>,
        force: bool,
        testing: bool,
        installed_only: bool,
    },
    /// `perform_downgrade(packages)`.
    Downgrade(Vec<String>),
    /// `perform_update(noprepare, newpackage)`.
    Update { noprepare: bool, newpackage: bool },
}

/// Resolves the `-t` selection and drives `op` over it, restoring the group.
///
/// When `-t` names a strict subset, the unselected hosts are dropped from the
/// live report for the operation: Rust `HostsGroup::select` moves the chosen
/// targets out and there is no group-merge API yet (follow-up bead). With no
/// `-t` — the common path, and what the tests and the e2e gate exercise —
/// selection is lossless.
///
/// # Errors
///
/// * [`CommandError::NoRefhostsDefined`] when the selection is empty.
/// * [`CommandError::Other`] when a named `-t` host is not connected.
pub(super) async fn drive(
    session: &mut Session,
    args: &ArgMatches,
    op: PerformOp,
) -> CommandResult {
    let hosts = super::support::hosts_arg(args);
    let whole = session.take_targets();
    let selected = match &hosts {
        Some(names) if !names.is_empty() && !names.iter().any(|h| h == "all") => {
            whole.select(Some(names), true)
        }
        _ => whole.select(None, true),
    };
    let mut selected: HostsGroup = match selected {
        Ok(g) => g,
        Err(e) => return Err(CommandError::Other(e.to_string())),
    };
    if selected.is_empty() {
        session.restore_targets(selected);
        return Err(CommandError::NoRefhostsDefined);
    }

    let report = session.metadata();
    match &op {
        PerformOp::Install(pkgs) => report.perform_install(&mut selected, pkgs).await,
        PerformOp::Uninstall(pkgs) => report.perform_uninstall(&mut selected, pkgs).await,
        PerformOp::Prepare {
            packages,
            force,
            testing,
            installed_only,
        } => {
            report
                .perform_prepare(&mut selected, packages, *force, *testing, *installed_only)
                .await;
        }
        PerformOp::Downgrade(pkgs) => report.perform_downgrade(&mut selected, pkgs).await,
        PerformOp::Update {
            noprepare,
            newpackage,
        } => {
            report
                .perform_update(&mut selected, *noprepare, *newpackage)
                .await
        }
    }

    session.restore_targets(selected);
    Ok(())
}
