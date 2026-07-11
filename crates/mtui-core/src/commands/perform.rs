//! Shared driver for the `perform_*` workflow commands.
//!
//! `install`, `uninstall`, `prepare`, `downgrade`, and `update` all follow the
//! same shape: resolve the `-t` host selection, then drive one of the active
//! report's `perform_*` flows over the selected [`HostsGroup`].
//!
//! Those flows take `&self` (the report) **and** `&mut HostsGroup` at once, and
//! the group lives inside the report — so the body splits the group out
//! ([`Session::split_targets`](crate::Session::split_targets)), re-borrows the
//! report immutably, drives the op over the selected subset, and recombines the
//! selected subset with the untouched remainder
//! ([`Session::restore_split_targets`](crate::Session::restore_split_targets)) so
//! a `-t` subset never drops the unselected hosts. Modelling the op as an enum
//! (rather than a borrowing async closure) keeps the future `Send` without
//! wrestling higher-ranked lifetimes.

use clap::ArgMatches;
use mtui_hosts::HostsGroup;
use mtui_testreport::Diagnostic;

use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Renders update-check [`Diagnostic`] sections through the session display,
/// mirroring upstream `checks/update.py`'s two `print(...)` blocks: the
/// "Additional rpm output" section is printed with the word `warning` recolored
/// yellow (`replace("warning", yellow("warning"))`), while the "not supported by
/// its vendor" section is printed plain.
fn render_diagnostics(session: &mut Session, diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let line = if diag.highlight_warning {
            // Recolor every "warning" occurrence yellow, matching upstream's
            // `str.replace`. `yellow` is a no-op under `ColorMode::Never`.
            let yellow_warning = session.display.yellow("warning");
            diag.text.replace("warning", &yellow_warning)
        } else {
            diag.text.clone()
        };
        session.display.println(&line);
    }
}

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
/// A `-t` subset operation runs over only the selected hosts, but the unselected
/// hosts are preserved: the group is split via
/// [`Session::split_targets`](crate::Session::split_targets) and the untouched
/// remainder is merged back by
/// [`Session::restore_split_targets`](crate::Session::restore_split_targets)
/// afterwards. With no `-t` — the common path, and what the tests and the e2e
/// gate exercise — the remainder is empty and selection is lossless.
///
/// # Errors
///
/// * [`CommandError::Other`] when no report is loaded (upstream
///   `@requires_update` → `TestReportNotLoadedError`), checked before host
///   selection so a no-op `NullReport` flow never silently "succeeds".
/// * [`CommandError::NoRefhostsDefined`] when the selection is empty.
/// * [`CommandError::Other`] when a named `-t` host is not connected.
pub(super) async fn drive(
    session: &mut Session,
    args: &ArgMatches,
    op: PerformOp,
) -> CommandResult {
    // Upstream decorates each of these commands with `@requires_update`, which
    // raises `TestReportNotLoadedError` before touching hosts. Enforce the same
    // guard first so `install`/`uninstall`/`prepare`/`downgrade`/`update` refuse
    // when no report is loaded instead of driving the null report's no-op flow.
    super::support::require_update(session)?;

    let hosts = super::support::hosts_arg(args);
    let names = match &hosts {
        Some(names) if !names.is_empty() && !names.iter().any(|h| h == "all") => {
            Some(names.as_slice())
        }
        _ => None,
    };
    let (mut selected, remainder): (HostsGroup, HostsGroup) = match session.split_targets(names) {
        Ok(split) => split,
        Err(e) => return Err(CommandError::Other(e.to_string())),
    };
    if selected.is_empty() {
        session.restore_split_targets(selected, remainder);
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
            // Unlike the other flows, `update` surfaces a failure verdict: a
            // per-host `updater` check failure (post-rollback) or a hard
            // missing-updater failure. Restore the split group *before*
            // returning so a failed update still merges the unselected hosts
            // back, then map the update error onto CommandError.
            //
            // The update check also surfaces recognised-but-non-fatal
            // diagnostic sections (upstream `checks/update.py`'s two
            // `print(...)` blocks). Collect them into a sink here — the one
            // place the session's display is in scope — and render them after
            // the fan-out, on both the success and failure paths.
            let mut diagnostics = Vec::new();
            let update_result = report
                .perform_update(&mut selected, *noprepare, *newpackage, &mut diagnostics)
                .await;
            session.restore_split_targets(selected, remainder);
            render_diagnostics(session, &diagnostics);
            return update_result.map_err(|e| CommandError::Other(e.to_string()));
        }
    }

    session.restore_split_targets(selected, remainder);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::Buffer;
    use crate::display::{ColorMode, CommandPromptDisplay};
    use crate::session::Session;
    use mtui_config::Config;

    fn session_with_color(color: ColorMode) -> (Session, Buffer) {
        let buf = Buffer::new();
        let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), color);
        (
            Session::with_display(Config::default(), false, display),
            buf,
        )
    }

    #[test]
    fn highlighted_diagnostic_recolors_warning_under_color() {
        let (mut session, buf) = session_with_color(ColorMode::Always);
        render_diagnostics(
            &mut session,
            &[Diagnostic::highlighted("\nwarning: extra rpm output\n")],
        );
        let out = buf.contents();
        // The section text is present and the word `warning` carries an ANSI
        // escape (yellow), matching upstream's `replace("warning", yellow(...))`.
        assert!(out.contains("extra rpm output"), "got: {out:?}");
        assert!(
            out.contains("\u{1b}["),
            "expected ANSI escape, got: {out:?}"
        );
        assert!(
            !out.contains("\u{1b}[") || out.contains("warning"),
            "warning token should survive: {out:?}"
        );
    }

    #[test]
    fn highlighted_diagnostic_is_plain_without_color() {
        let (mut session, buf) = session_with_color(ColorMode::Never);
        render_diagnostics(
            &mut session,
            &[Diagnostic::highlighted("\nwarning: extra rpm output\n")],
        );
        let out = buf.contents();
        assert!(out.contains("warning: extra rpm output"), "got: {out:?}");
        assert!(!out.contains("\u{1b}["), "expected no ANSI, got: {out:?}");
    }

    #[test]
    fn plain_diagnostic_never_recolors_even_under_color() {
        let (mut session, buf) = session_with_color(ColorMode::Always);
        render_diagnostics(
            &mut session,
            &[Diagnostic::plain(
                "The following package is not supported by its vendor:\nwarning foo",
            )],
        );
        let out = buf.contents();
        // Upstream prints the vendor section plain (no recoloring), even though
        // it may contain the word "warning".
        assert!(out.contains("not supported by its vendor"), "got: {out:?}");
        assert!(!out.contains("\u{1b}["), "expected no ANSI, got: {out:?}");
    }

    #[test]
    fn empty_diagnostics_render_nothing() {
        let (mut session, buf) = session_with_color(ColorMode::Always);
        render_diagnostics(&mut session, &[]);
        assert!(buf.contents().is_empty());
    }
}
