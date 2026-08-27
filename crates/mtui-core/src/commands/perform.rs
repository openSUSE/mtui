//! Shared driver for the `perform_*` workflow commands.
//!
//! `install`, `uninstall`, `prepare`, `downgrade` and `update` share one shape:
//! resolve the `-t` host selection, then drive one of the active report's
//! `perform_*` flows over the selected [`HostsGroup`].
//!
//! Those flows take `&self` (the report) **and** `&mut HostsGroup` at once while
//! the group lives inside the report, so the body splits the group out
//! ([`Session::split_targets`](crate::Session::split_targets)), drives the op
//! over the subset, and recombines it with the untouched remainder
//! ([`Session::restore_split_targets`](crate::Session::restore_split_targets))
//! so a `-t` subset never drops the unselected hosts. The op is an enum rather
//! than a borrowing async closure to keep the future `Send`.

use clap::ArgMatches;
use mtui_hosts::HostsGroup;
use mtui_testreport::Diagnostic;

use mtui_testreport::UpdateError;

use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Renders update-check [`Diagnostic`] sections: "Additional rpm output" with
/// the word `warning` recolored yellow, "not supported by its vendor" plain.
fn render_diagnostics(session: &mut Session, diagnostics: &[Diagnostic]) {
    for diag in diagnostics {
        let line = if diag.highlight_warning {
            // `yellow` is a no-op under `ColorMode::Never`.
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

/// The success confirmation shared by every [`drive`] op, so no successful
/// dispatch is an empty MCP result.
fn confirm(session: &mut Session, verb: &str, hosts_label: &str) {
    session
        .display
        .println(&format!("{verb} completed on {hosts_label}"));
}

/// Maps a flow error onto a [`CommandError`]. The flow's own `cancelled` marker
/// — **not** the session token — is the authority: sniffing the token would hide
/// a genuine host failure that merely coincided with a cancel. The message names
/// which packages were applied, so it is preserved rather than flattened.
fn map_flow_error(e: &UpdateError) -> CommandError {
    if e.is_cancelled() {
        return CommandError::Cancelled(e.to_string());
    }
    CommandError::Other(e.to_string())
}

/// Resolves the `-t` selection and drives `op` over it, restoring the group.
///
/// A `-t` subset runs over only the selected hosts, the remainder split out by
/// [`Session::split_targets`](crate::Session::split_targets) and merged back by
/// [`Session::restore_split_targets`](crate::Session::restore_split_targets).
/// With no `-t` the remainder is empty and selection is lossless.
///
/// # Errors
///
/// * [`CommandError::Other`] when no report is loaded, checked before host
///   selection so a no-op `NullReport` flow never silently "succeeds".
/// * [`CommandError::NoRefhostsDefined`] when the selection is empty.
/// * [`CommandError::Other`] when a named `-t` host is not connected.
pub(super) async fn drive(
    session: &mut Session,
    args: &ArgMatches,
    op: PerformOp,
) -> CommandResult {
    // Guard before touching hosts, so these commands refuse when no report is
    // loaded instead of driving the null report's no-op flow.
    super::support::require_update(session)?;

    let hosts = super::support::hosts_arg(args);
    let names = match &hosts {
        Some(names) if !names.is_empty() && !names.iter().any(|h| h == "all") => {
            Some(names.as_slice())
        }
        _ => None,
    };
    let (mut selected, remainder): (HostsGroup, HostsGroup) =
        match session.split_targets(names, true) {
            Ok(split) => split,
            Err(e) => return Err(CommandError::Other(e.to_string())),
        };
    if selected.is_empty() {
        session.restore_split_targets(selected, remainder);
        return Err(CommandError::NoRefhostsDefined);
    }

    // The host names the op ran over, for the success confirmation line.
    let hosts_label = selected.names().join(", ");
    let report = session.metadata();
    // The flow's `Err` message already names the failed host(s)/reason. Success
    // is confirmed to the display so an MCP call is never a silent "success".
    let (verb, outcome): (&str, Result<(), CommandError>) = match &op {
        PerformOp::Install(pkgs) => (
            "install",
            report
                .perform_install(&mut selected, pkgs)
                .await
                .map_err(|e| map_flow_error(&e)),
        ),
        PerformOp::Uninstall(pkgs) => (
            "uninstall",
            report
                .perform_uninstall(&mut selected, pkgs)
                .await
                .map_err(|e| map_flow_error(&e)),
        ),
        PerformOp::Prepare {
            packages,
            force,
            testing,
            installed_only,
        } => (
            "prepare",
            report
                .perform_prepare(&mut selected, packages, *force, *testing, *installed_only)
                .await
                .map_err(|e| map_flow_error(&e)),
        ),
        PerformOp::Downgrade(pkgs) => (
            "downgrade",
            report
                .perform_downgrade(&mut selected, pkgs)
                .await
                .map_err(|e| map_flow_error(&e)),
        ),
        PerformOp::Update {
            noprepare,
            newpackage,
        } => {
            // `update` alone surfaces a failure verdict, so restore the split
            // *before* returning or a failed update strands the unselected
            // hosts. Its non-fatal diagnostics are collected here — the one
            // place the display is in scope — and rendered on both paths.
            let mut diagnostics = Vec::new();
            let update_result = report
                .perform_update(&mut selected, *noprepare, *newpackage, &mut diagnostics)
                .await;
            session.restore_split_targets(selected, remainder);
            if update_result.is_ok() {
                // Verdict before the unbounded diagnostics: `SharedBuf` keeps
                // the head, so a trailing line is what `max_output_bytes` eats.
                confirm(session, "update", &hosts_label);
            }
            render_diagnostics(session, &diagnostics);
            // A cancellation checkpoint surfaces as an ordinary `UpdateError`;
            // map it so the caller sees `Cancelled`, not a generic failure.
            update_result.map_err(|e| map_flow_error(&e))?;
            return Ok(());
        }
    };

    session.restore_split_targets(selected, remainder);
    outcome?;
    confirm(session, verb, &hosts_label);
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
        // The word `warning` carries an ANSI escape (yellow).
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
        // The vendor section stays plain even though it contains "warning".
        assert!(out.contains("not supported by its vendor"), "got: {out:?}");
        assert!(!out.contains("\u{1b}["), "expected no ANSI, got: {out:?}");
    }

    #[test]
    fn empty_diagnostics_render_nothing() {
        let (mut session, buf) = session_with_color(ColorMode::Always);
        render_diagnostics(&mut session, &[]);
        assert!(buf.contents().is_empty());
    }

    // --- success confirmation ----------------------------------------------

    use crate::commands::Update;
    use crate::commands::testkit::{
        matches, session_with_cancelled_update, session_with_hosts, session_with_update_diagnostics,
    };

    const RRID: &str = "SUSE:Maintenance:1:1";

    fn update_op() -> PerformOp {
        PerformOp::Update {
            noprepare: true,
            newpackage: false,
        }
    }

    /// `drive` only reads `-t` off the matches, so one command's parser serves
    /// every op.
    fn no_args() -> ArgMatches {
        matches(&Update, &[])
    }

    /// #525: every op — `update` included, whose only other output is the
    /// diagnostics a clean transaction never emits — confirms success in the
    /// same shape and names every selected host.
    #[tokio::test]
    async fn every_op_confirms_success_naming_every_selected_host() {
        let pkg = || vec!["pkg".to_owned()];
        for (verb, op) in [
            ("install", PerformOp::Install(pkg())),
            ("uninstall", PerformOp::Uninstall(pkg())),
            (
                "prepare",
                PerformOp::Prepare {
                    packages: pkg(),
                    force: false,
                    testing: false,
                    installed_only: false,
                },
            ),
            ("downgrade", PerformOp::Downgrade(pkg())),
            ("update", update_op()),
        ] {
            let (mut session, buf) = session_with_hosts(RRID, &["h1", "h2"], "ok");
            drive(&mut session, &no_args(), op).await.unwrap();
            assert_eq!(buf.contents().trim(), format!("{verb} completed on h1, h2"));
        }
    }

    /// The label is the `-t` selection, not the whole group: naming the
    /// restored remainder would claim work on a host the op never touched.
    #[tokio::test]
    async fn confirmation_names_only_the_selected_hosts() {
        let (mut session, buf) = session_with_hosts(RRID, &["h1", "h2"], "ok");
        drive(&mut session, &matches(&Update, &["-t", "h1"]), update_op())
            .await
            .unwrap();
        assert_eq!(buf.contents().trim(), "update completed on h1");
    }

    #[tokio::test]
    async fn update_diagnostics_do_not_replace_the_confirmation() {
        let (mut session, buf) = session_with_update_diagnostics(RRID, &["h1"], false);
        drive(&mut session, &no_args(), update_op()).await.unwrap();
        let out = buf.contents();
        let verdict = out.find("update completed on h1").expect("confirmed");
        let diag = out.find("extra rpm output").expect("diagnostics rendered");
        assert!(out.contains("not supported by its vendor"), "got: {out:?}");
        // The diagnostics are unbounded and `SharedBuf` truncates the tail, so
        // the verdict has to come first to survive `max_output_bytes`.
        assert!(verdict < diag, "verdict must precede diagnostics: {out:?}");
    }

    #[tokio::test]
    async fn failed_update_renders_diagnostics_but_claims_nothing() {
        let (mut session, buf) = session_with_update_diagnostics(RRID, &["h1"], true);
        drive(&mut session, &no_args(), update_op())
            .await
            .expect_err("a failing perform_update must not report success");
        let out = buf.contents();
        assert!(out.contains("extra rpm output"), "got: {out:?}");
        assert!(!out.contains("completed"), "got: {out:?}");
    }

    /// A cancelled update stopped short of patching every host, so it must
    /// surface as `Cancelled` and claim nothing.
    #[tokio::test]
    async fn cancelled_update_claims_nothing() {
        let (mut session, buf) = session_with_cancelled_update(RRID, &["h1"]);
        let err = drive(&mut session, &no_args(), update_op())
            .await
            .expect_err("a cancelled perform_update must not report success");
        assert!(matches!(err, CommandError::Cancelled(_)), "got: {err:?}");
        assert!(
            !buf.contents().contains("completed"),
            "{:?}",
            buf.contents()
        );
    }
}
