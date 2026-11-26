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

use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::HostsGroup;
use mtui_testreport::Diagnostic;

use mtui_testreport::UpdateError;

use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Renders update-check [`Diagnostic`] sections: "Additional rpm output" with
/// the word `warning` recolored yellow, "not supported by its vendor" plain.
fn render_diagnostics(session: &mut Session, diagnostics: &[Diagnostic]) {
    // Degradations never fold; only clean sections share a folded run.
    let mut clean: Vec<String> = Vec::new();
    let flush = |session: &mut Session, clean: &mut Vec<String>| {
        for line in crate::fold::fold_output(clean) {
            session.display.println(&line);
        }
        clean.clear();
    };
    for diag in diagnostics {
        if diag.degradation {
            flush(session, &mut clean);
            for line in diag.text.split('\n') {
                session.display.println(line);
            }
            continue;
        }
        let line = if diag.highlight_warning {
            // `yellow` is a no-op under `ColorMode::Never`.
            let yellow_warning = session.display.yellow("warning");
            diag.text.replace("warning", &yellow_warning)
        } else {
            diag.text.clone()
        };
        clean.extend(line.split('\n').map(str::to_owned));
    }
    flush(session, &mut clean);
}

/// The id of `--allow-removal`, which every guarded op's command carries.
const ALLOW_REMOVAL: &str = "allow_removal";

/// Adds `--allow-removal` to a command whose package transaction the removal
/// guard checks.
pub(super) fn add_allow_removal_arg(cmd: clap::Command) -> clap::Command {
    cmd.arg(
        Arg::new(ALLOW_REMOVAL)
            .long("allow-removal")
            .action(ArgAction::SetTrue)
            .help(
                "commit the transaction even when its dry run would remove a protected package \
                 (kernel, systemd, glibc, rpm, zypper, openssh, grub2, ...) or cannot be read",
            ),
    )
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

/// The confirmation shared by every non-`update` [`drive`] op for the hosts
/// [`confirmed_hosts`] names, so no successful dispatch is an empty MCP result
/// and a partial one does not read as a total failure. Qualified with the
/// degradation count like [`confirm_update`]: the verdict prints above the
/// diagnostics (the head is what survives `mcp_max_output_bytes`), so an
/// unqualified head line would read as a clean success to a truncated or
/// skim-read result. Only degradations count; routine check sections never
/// qualify the verdict.
fn confirm(session: &mut Session, verb: &str, hosts_label: &str, diagnostics: &[Diagnostic]) {
    let n = degradation_count(diagnostics);
    if n == 0 {
        session
            .display
            .println(&format!("{verb} completed on {hosts_label}"));
        return;
    }
    let plural = if n == 1 { "" } else { "s" };
    session.display.println(&format!(
        "{verb} completed on {hosts_label}, with {n} degradation{plural} reported below"
    ));
}

/// The hosts a confirmation names: the whole selection on success, the hosts
/// the flow completed on beside a failure, or `None` when it completed on none.
fn confirmed_hosts(outcome: &Result<(), UpdateError>, hosts_label: &str) -> Option<String> {
    match outcome {
        Ok(()) => Some(hosts_label.to_owned()),
        Err(e) if !e.completed().is_empty() => Some(e.completed().join(", ")),
        Err(_) => None,
    }
}

fn degradation_count(diagnostics: &[Diagnostic]) -> usize {
    diagnostics.iter().filter(|d| d.degradation).count()
}

/// `update`'s confirmation, qualified when the flow recorded a
/// [`Diagnostic::degradation`].
///
/// `perform_update` returns `Ok` for a run whose patch applied but whose
/// `--newpackage` prepare or test-repo cleanup degraded. The verdict is printed
/// *above* the diagnostics (the head is what survives `mcp_max_output_bytes`),
/// so an unqualified "update completed on …" would be the only line a truncated
/// result carries.
///
/// Only degradations are counted: the check's own recognised sections ride in
/// the same vec and are **routine** (mtui patches from a test repo whose vendor
/// differs from the official one, so zypper's "not supported by its vendor"
/// notice fires on healthy updates).
fn confirm_update(session: &mut Session, hosts_label: &str, diagnostics: &[Diagnostic]) {
    let n = degradation_count(diagnostics);
    if n == 0 {
        confirm(session, "update", hosts_label, diagnostics);
        return;
    }
    let plural = if n == 1 { "" } else { "s" };
    session.display.println(&format!(
        "update completed on {hosts_label}: the patch passed its checks, with {n} \
         degradation{plural} reported below"
    ));
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
    // Like `-t`, shared by the ops' commands; `update` has no such flag.
    let allow_removal = matches!(args.try_get_one::<bool>(ALLOW_REMOVAL), Ok(Some(true)));
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
    // Non-fatal flow diagnostics (holds lost in a transactional reboot gap)
    // are collected here and rendered on both paths, so an unattended MCP
    // caller sees them.
    let mut diagnostics = Vec::new();
    // The flow's `Err` message already names the failed host(s)/reason. Success
    // is confirmed to the display so an MCP call is never a silent "success".
    let (verb, outcome): (&str, Result<(), UpdateError>) = match &op {
        PerformOp::Install(pkgs) => (
            "install",
            report
                .perform_install(&mut selected, pkgs, allow_removal, &mut diagnostics)
                .await,
        ),
        PerformOp::Uninstall(pkgs) => (
            "uninstall",
            report
                .perform_uninstall(&mut selected, pkgs, allow_removal, &mut diagnostics)
                .await,
        ),
        PerformOp::Prepare {
            packages,
            force,
            testing,
            installed_only,
        } => (
            "prepare",
            report
                .perform_prepare(
                    &mut selected,
                    packages,
                    *force,
                    *testing,
                    *installed_only,
                    allow_removal,
                    &mut diagnostics,
                )
                .await,
        ),
        PerformOp::Downgrade(pkgs) => (
            "downgrade",
            report
                .perform_downgrade(&mut selected, pkgs, allow_removal, &mut diagnostics)
                .await,
        ),
        PerformOp::Update {
            noprepare,
            newpackage,
        } => {
            // `update` alone surfaces a failure verdict, so restore the split
            // *before* returning or a failed update strands the unselected
            // hosts.
            let update_result = report
                .perform_update(&mut selected, *noprepare, *newpackage, &mut diagnostics)
                .await;
            session.restore_split_targets(selected, remainder);
            // Verdict before the unbounded diagnostics: `SharedBuf` keeps the
            // head, so a trailing line is what `max_output_bytes` eats.
            if let Some(hosts) = confirmed_hosts(&update_result, &hosts_label) {
                confirm_update(session, &hosts, &diagnostics);
            }
            render_diagnostics(session, &diagnostics);
            // A cancellation checkpoint surfaces as an ordinary `UpdateError`;
            // map it so the caller sees `Cancelled`, not a generic failure.
            update_result.map_err(|e| map_flow_error(&e))?;
            return Ok(());
        }
    };

    session.restore_split_targets(selected, remainder);
    if let Some(hosts) = confirmed_hosts(&outcome, &hosts_label) {
        confirm(session, verb, &hosts, &diagnostics);
    }
    render_diagnostics(session, &diagnostics);
    outcome.map_err(|e| map_flow_error(&e))?;
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

    #[test]
    fn repetitive_diagnostics_fold_but_warnings_survive() {
        let (mut session, buf) = session_with_color(ColorMode::Never);
        let spam = vec![Diagnostic::plain("ok"); 5];
        render_diagnostics(&mut session, &spam);
        let out = buf.contents();
        assert!(out.contains("…[4 identical lines folded]"), "{out:?}");
        assert_eq!(out.lines().count(), 2, "{out:?}");

        let (mut session, buf) = session_with_color(ColorMode::Never);
        let warns = vec![Diagnostic::highlighted("warning: x"); 5];
        render_diagnostics(&mut session, &warns);
        let out = buf.contents();
        assert!(!out.contains("identical"), "{out:?}");
        assert_eq!(out.lines().count(), 5, "{out:?}");
    }

    #[test]
    fn highlighted_warnings_survive_folding_under_color() {
        // Under `Never` `yellow` is a no-op, so the test above cannot catch
        // color-before-scan folding; here the escapes are real and the SGR
        // `m` must not defeat the fold-safety scan.
        let (mut session, buf) = session_with_color(ColorMode::Always);
        let warns = vec![Diagnostic::highlighted("warning: x"); 5];
        render_diagnostics(&mut session, &warns);
        let out = buf.contents();
        assert!(out.contains("\u{1b}["), "color must be on: {out:?}");
        assert!(!out.contains("identical"), "{out:?}");
        assert_eq!(out.lines().count(), 5, "{out:?}");
    }

    #[test]
    fn degradation_repeats_never_fold() {
        let (mut session, buf) = session_with_color(ColorMode::Never);
        let degs = vec![Diagnostic::degradation("boom"); 5];
        render_diagnostics(&mut session, &degs);
        let out = buf.contents();
        assert_eq!(
            out.matches("boom").count(),
            5,
            "all repeats survive: {out:?}"
        );
        assert!(!out.contains("identical"), "{out:?}");
        assert_eq!(out.lines().count(), 5, "{out:?}");
    }

    #[test]
    fn clean_spam_folds_around_degradation() {
        let (mut session, buf) = session_with_color(ColorMode::Never);
        let diags = vec![
            Diagnostic::plain("ok"),
            Diagnostic::plain("ok"),
            Diagnostic::plain("ok"),
            Diagnostic::degradation("boom"),
            Diagnostic::plain("ok"),
            Diagnostic::plain("ok"),
            Diagnostic::plain("ok"),
        ];
        render_diagnostics(&mut session, &diags);
        let out = buf.contents();
        assert_eq!(
            out.matches("…[2 identical lines folded]").count(),
            2,
            "{out:?}"
        );
        assert!(out.contains("boom"), "{out:?}");
    }

    // --- success confirmation ----------------------------------------------

    use crate::commands::Update;
    use crate::commands::testkit::{
        matches, session_with_cancelled_perform, session_with_cancelled_update,
        session_with_degraded_install, session_with_degraded_update, session_with_hosts,
        session_with_partial_perform, session_with_partial_update, session_with_update_diagnostics,
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

    /// #525 review: `perform_update` returns `Ok` for a run whose patch applied
    /// but whose `--newpackage` prepare and repo cleanup degraded. Those reach
    /// the caller as diagnostics, but they are printed *below* the verdict, so
    /// the verdict line itself must not read as an unqualified full success —
    /// it is the only line a truncated result is guaranteed to carry.
    #[tokio::test]
    async fn a_degraded_update_qualifies_its_confirmation_and_counts_the_degradations() {
        let (mut session, buf) = session_with_degraded_update(RRID, &["h1"]);
        drive(&mut session, &no_args(), update_op()).await.unwrap();
        let out = buf.contents();
        let head = out.lines().next().unwrap_or_default();
        assert!(head.starts_with("update completed on h1:"), "got: {out:?}");
        assert!(
            head.contains("2 degradations reported below"),
            "got: {out:?}"
        );
        assert!(
            head.contains("the patch passed its checks"),
            "the head must claim only the checked patch: {out:?}"
        );
    }

    /// #534 review: a *healthy* update still emits check sections — mtui patches
    /// from a test update repo whose vendor differs from the official one, so
    /// the vendor notice is routine. Counting those would qualify almost every
    /// run and would be false besides: they are the checks' own output, and the
    /// checks passed. Only degradations qualify the verdict.
    #[tokio::test]
    async fn check_sections_alone_leave_the_confirmation_bare() {
        let (mut session, buf) = session_with_update_diagnostics(RRID, &["h1"], false);
        drive(&mut session, &no_args(), update_op()).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("not supported by its vendor"), "got: {out:?}");
        assert_eq!(
            out.lines().next().unwrap_or_default(),
            "update completed on h1",
            "check output must not read as a degradation: {out:?}"
        );
    }

    /// The clean run keeps the bare shape every other op prints — the
    /// qualification is a statement about degradations, not about `update`.
    #[tokio::test]
    async fn a_clean_update_confirms_without_qualification() {
        let (mut session, buf) = session_with_hosts(RRID, &["h1"], "ok");
        drive(&mut session, &no_args(), update_op()).await.unwrap();
        assert_eq!(buf.contents().trim(), "update completed on h1");
    }

    #[test]
    fn one_degradation_is_counted_in_the_singular() {
        let (mut session, buf) = session_with_color(ColorMode::Never);
        confirm_update(&mut session, "h1", &[Diagnostic::degradation("something")]);
        assert!(
            buf.contents().contains("with 1 degradation reported below"),
            "{:?}",
            buf.contents()
        );
    }

    /// The non-`update` ops route flow diagnostics to the display too: a
    /// degradation `perform_install` records must reach an MCP caller, not
    /// only the server log.
    #[tokio::test]
    async fn install_degradation_reaches_the_display() {
        let (mut session, buf) = session_with_degraded_install(RRID, &["h1"]);
        drive(
            &mut session,
            &no_args(),
            PerformOp::Install(vec!["pkg".to_owned()]),
        )
        .await
        .unwrap();
        let out = buf.contents();
        assert!(
            out.contains("install lost a hold in the reboot gap (h1)"),
            "the degradation must be rendered: {out:?}"
        );
    }

    /// Like `update`, the other ops qualify the head line when the run
    /// degraded: it is the only line a truncated MCP result carries.
    #[tokio::test]
    async fn a_degraded_install_qualifies_its_confirmation() {
        let (mut session, buf) = session_with_degraded_install(RRID, &["h1"]);
        drive(
            &mut session,
            &no_args(),
            PerformOp::Install(vec!["pkg".to_owned()]),
        )
        .await
        .unwrap();
        let out = buf.contents();
        let head = out.lines().next().unwrap_or_default();
        assert!(head.starts_with("install completed on h1,"), "got: {out:?}");
        assert!(
            head.contains("1 degradation reported below"),
            "got: {out:?}"
        );
    }

    #[test]
    fn non_update_confirm_counts_degradations_in_plural() {
        let (mut session, buf) = session_with_color(ColorMode::Never);
        confirm(
            &mut session,
            "downgrade",
            "h1",
            &[
                Diagnostic::degradation("first"),
                Diagnostic::degradation("second"),
            ],
        );
        assert_eq!(
            buf.contents().trim(),
            "downgrade completed on h1, with 2 degradations reported below"
        );
    }

    #[test]
    fn non_update_confirm_ignores_routine_sections() {
        // Only degradations qualify the verdict; routine check output rides
        // along without changing the head line.
        let (mut session, buf) = session_with_color(ColorMode::Never);
        confirm(
            &mut session,
            "prepare",
            "h1",
            &[Diagnostic::plain("not supported by its vendor")],
        );
        assert_eq!(buf.contents().trim(), "prepare completed on h1");
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

    /// A partial update still fails for the host it excluded, but its head line
    /// names the hosts it completed on — not the whole selection.
    #[tokio::test]
    async fn a_partial_update_confirms_only_the_hosts_it_completed_on() {
        let (mut session, buf) = session_with_partial_update(RRID, &["h1", "h2"]);
        let err = drive(&mut session, &no_args(), update_op())
            .await
            .expect_err("the excluded host still fails the update");
        assert!(
            matches!(&err, CommandError::Other(m) if m.starts_with("h2: ")),
            "got: {err:?}"
        );
        let out = buf.contents();
        let mut lines = out.lines();
        assert_eq!(
            lines.next().unwrap_or_default(),
            "update completed on h1: the patch passed its checks, with 1 degradation reported below",
            "got: {out:?}"
        );
        assert!(
            lines
                .next()
                .is_some_and(|l| l.starts_with("h2: ") && l.ends_with("excluded from the update")),
            "the exclusion follows the verdict: {out:?}"
        );
        assert_eq!(out.matches("completed").count(), 1, "{out:?}");
    }

    /// A cancel of the other ops surfaces as `Cancelled` and claims no host.
    #[tokio::test]
    async fn every_cancelled_op_claims_nothing() {
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
        ] {
            let (mut session, buf) = session_with_cancelled_perform(RRID, &["h1", "h2"]);
            let err = drive(&mut session, &no_args(), op)
                .await
                .expect_err("a cancelled op must not report success");
            assert!(matches!(err, CommandError::Cancelled(_)), "{verb}: {err:?}");
            assert!(
                !buf.contents().contains("completed"),
                "{verb}: {:?}",
                buf.contents()
            );
        }
    }

    /// The other ops' partial runs keep the failure for the host that failed,
    /// but their head line names only the hosts they completed on.
    #[tokio::test]
    async fn every_partial_op_confirms_only_the_hosts_it_completed_on() {
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
        ] {
            let (mut session, buf) = session_with_partial_perform(RRID);
            let err = drive(&mut session, &no_args(), op)
                .await
                .expect_err("the host that failed still fails the run");
            assert!(
                matches!(&err, CommandError::Other(m) if m.starts_with("h2: ")),
                "{verb}: {err:?}"
            );
            let out = buf.contents();
            assert_eq!(
                out.lines().next().unwrap_or_default(),
                format!("{verb} completed on h1"),
                "{out:?}"
            );
            assert!(!out.contains("h2"), "{verb} never claims h2: {out:?}");
            assert_eq!(out.matches("completed").count(), 1, "{out:?}");
        }
    }

    /// `--allow-removal` reaches the report for each guarded op, and only
    /// when it is passed.
    #[tokio::test]
    async fn allow_removal_reaches_the_report_only_when_passed() {
        use crate::commands::downgrade::Downgrade;
        use crate::commands::prepare::Prepare;
        use crate::commands::zypper::{Install, Uninstall};

        let pkg = || vec!["pkg".to_owned()];
        let prepare = || PerformOp::Prepare {
            packages: pkg(),
            force: false,
            testing: false,
            installed_only: false,
        };
        for flag in [true, false] {
            let with = |argv: &[&'static str]| {
                let mut v = argv.to_vec();
                if flag {
                    v.insert(0, "--allow-removal");
                }
                v
            };
            for (verb, args, op) in [
                (
                    "install",
                    matches(&Install, &with(&["pkg"])),
                    PerformOp::Install(pkg()),
                ),
                (
                    "uninstall",
                    matches(&Uninstall, &with(&["pkg"])),
                    PerformOp::Uninstall(pkg()),
                ),
                ("prepare", matches(&Prepare, &with(&[])), prepare()),
                (
                    "downgrade",
                    matches(&Downgrade, &with(&[])),
                    PerformOp::Downgrade(pkg()),
                ),
            ] {
                let (mut session, buf) = session_with_hosts(RRID, &["h1"], "ok");
                drive(&mut session, &args, op).await.unwrap();
                assert_eq!(
                    buf.contents().contains("allow_removal was set"),
                    flag,
                    "{verb} --allow-removal={flag}: {:?}",
                    buf.contents()
                );
            }
        }
    }
}
