//! The `check_hosts` command.

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::target::health::{CheckName, HealthOptions, HealthReport, HostHealth, Status};
use serde_json::{Value, json};

use super::support::{add_hosts_arg, one_line, select_hosts};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Characters of a check's detail printed.
const DETAIL_LIMIT: usize = 200;

/// What the check leaves out, said wherever a verdict is printed so `ok` is
/// never read as covering it.
const NOT_CHECKED: &str = "dependencies and pending patches are not checked";

/// Checks the selected hosts read-only and gives each a verdict.
///
/// Every host is reported, a disabled or unreachable one too. The command
/// takes neither the operation lock nor a pool claim, writes no history row,
/// records nothing in the host log and changes no package or repository; it
/// fails when any host's verdict is `fail` or `unknown`, after printing every
/// host. A check that does not apply to a host is listed, not failed.
pub struct CheckHosts;

#[async_trait]
impl Command for CheckHosts {
    fn name(&self) -> &'static str {
        "check_hosts"
    }

    fn about(&self) -> Option<&'static str> {
        Some(
            "Checks the connected hosts read-only (packages, root space, failed units, pending \
             reboot, leftover test repositories) and gives each a verdict: ok, warn, fail or \
             unknown. Fails when any host is fail or unknown. Takes no operation lock or pool \
             claim and changes no package; its zypper probes hold zypper's own lock briefly.",
        )
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help("print one JSON document instead of the text blocks"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        std::iter::once("--json".to_owned())
            .chain(session.targets().names())
            .filter(|s| s.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let as_json = args.get_flag("json");
        let required = session.config.target_required_packages.clone();
        let options = HealthOptions {
            required_packages: &required,
            min_root_free_mib: session.config.target_min_root_free,
        };
        let cancel = session.cancel_token();
        let template = session.templates.active_rrid().map(str::to_owned);
        let targets = session.targets_mut();
        // Disabled hosts too: each is reported as not checked rather than left out.
        let hosts = select_hosts(targets, args, false)?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }
        let report = targets.check_health(&hosts, &options, &cancel).await;

        if as_json {
            let doc = render_json(&report, template.as_deref());
            session.display.println(
                &serde_json::to_string_pretty(&doc)
                    .expect("serialising a serde_json::Value is infallible"),
            );
        } else {
            for line in render_text(&report) {
                session.display.println(&line);
            }
        }

        if report.cancelled {
            return Err(CommandError::Cancelled(
                "check_hosts stopped between checks; the checks it did not run are unknown"
                    .to_owned(),
            ));
        }
        let unhealthy = unhealthy(&report);
        if unhealthy.is_empty() {
            Ok(())
        } else {
            Err(CommandError::Other(format!(
                "check_hosts: {} of {} hosts not healthy: {}",
                unhealthy.len(),
                report.hosts.len(),
                unhealthy.join(", ")
            )))
        }
    }
}

/// `host (verdict)` for every host whose verdict fails the command.
fn unhealthy(report: &HealthReport) -> Vec<String> {
    report
        .hosts
        .iter()
        .filter(|h| matches!(h.verdict(), Status::Fail | Status::Unknown))
        .map(|h| format!("{} ({})", one_line(&h.host, None), h.verdict()))
        .collect()
}

/// The verdict of the whole run: the worst host verdict.
fn overall(report: &HealthReport) -> Status {
    report
        .hosts
        .iter()
        .map(HostHealth::verdict)
        .max()
        .unwrap_or(Status::Unknown)
}

/// How many hosts got each verdict, best first.
fn counts(report: &HealthReport) -> [(Status, usize); 4] {
    [Status::Ok, Status::Warn, Status::Fail, Status::Unknown].map(|status| {
        (
            status,
            report
                .hosts
                .iter()
                .filter(|h| h.verdict() == status)
                .count(),
        )
    })
}

/// Hosts with at least one check that does not apply to them.
fn partly_checked(report: &HealthReport) -> usize {
    report
        .hosts
        .iter()
        .filter(|h| !h.not_checked.is_empty())
        .count()
}

/// One summary line, printed first so a cut-off text output still carries it.
fn summary(report: &HealthReport) -> String {
    let tally: Vec<String> = counts(report)
        .iter()
        .map(|(status, n)| format!("{n} {status}"))
        .collect();
    let partly = match partly_checked(report) {
        0 => String::new(),
        n => format!("; {n} with checks that do not apply to them"),
    };
    let stop = if report.cancelled { "; cancelled" } else { "" };
    format!(
        "check_hosts: {} — {} of {} hosts{partly}{stop}; {NOT_CHECKED}",
        overall(report),
        tally.join(", "),
        report.hosts.len()
    )
}

/// The text form: the summary, then one block per host, verdict first.
fn render_text(report: &HealthReport) -> Vec<String> {
    let mut lines = vec![summary(report)];
    for health in &report.hosts {
        let host = one_line(&health.host, None);
        match &health.skipped {
            Some(reason) => lines.push(format!(
                "{host}: {} — {}",
                health.verdict(),
                one_line(&reason.to_string(), Some(DETAIL_LIMIT))
            )),
            None => {
                lines.push(format!("{host}: {}", health.verdict()));
                for check in &health.checks {
                    lines.push(format!(
                        "  {:<17} {:<7} {}",
                        check.name.as_str(),
                        check.status.as_str(),
                        one_line(&check.detail, Some(DETAIL_LIMIT))
                    ));
                }
                for skip in &health.not_checked {
                    lines.push(format!(
                        "  {:<17} {:<7} not checked: {}",
                        skip.name.as_str(),
                        "-",
                        skip.reason
                    ));
                }
            }
        }
    }
    lines
}

/// The `--json` form: one self-describing document.
fn render_json(report: &HealthReport, template: Option<&str>) -> Value {
    let summary: serde_json::Map<String, Value> = counts(report)
        .iter()
        .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
        .chain([
            ("hosts".to_owned(), json!(report.hosts.len())),
            ("partly_checked".to_owned(), json!(partly_checked(report))),
        ])
        .collect();
    let hosts: Vec<Value> = report
        .hosts
        .iter()
        .map(|h| {
            json!({
                "host": one_line(&h.host, None),
                "verdict": h.verdict().as_str(),
                "reason": h
                    .skipped
                    .as_ref()
                    .map(|r| one_line(&r.to_string(), Some(DETAIL_LIMIT))),
                "checks": h.checks.iter().map(|c| json!({
                    "name": c.name.as_str(),
                    "status": c.status.as_str(),
                    "detail": one_line(&c.detail, Some(DETAIL_LIMIT)),
                })).collect::<Vec<_>>(),
                "not_checked": h.not_checked.iter().map(|n| json!({
                    "name": n.name.as_str(),
                    "reason": n.reason,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "command": "check_hosts",
        "verdict": overall(report).as_str(),
        "summary": summary,
        "cancelled": report.cancelled,
        "template": template,
        "checks": CheckName::ALL.map(CheckName::as_str),
        "unchecked": ["dependencies", "patches"],
        "hosts": hosts,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mtui_hosts::{
        HostsGroup, MockConnection, MockSftpOp, POOL_LOCK_PATH, TARGET_LOCK_PATH, Target,
        Transactional,
    };
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;
    use mtui_types::system::{System, SystemProduct};

    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_targets};

    // The probe commands, pinned as literals: a change to what reaches a
    // host must show up here. Outputs are hand-written, not captures.
    const RPM_PREFIX: &str =
        "LC_ALL=C rpm -q --queryformat \"%{Name} %{Version}-%{Release}\n\" -- ";
    const KERNELS: &str = "kernel-64kb kernel-azure kernel-default kernel-default-base \
                           kernel-kvmsmall kernel-preempt kernel-rt kernel-zfcpdump";
    const DF: &str = "LC_ALL=C df -Pk /";
    const SYSTEMCTL: &str = "LC_ALL=C systemctl --failed --no-legend --plain --no-pager";
    const NEEDS_REBOOTING: &str = "LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n needs-rebooting";
    const LR: &str = "LC_ALL=C ZYPP_LOCK_TIMEOUT=0 zypper -n lr";

    const RRID: &str = "SUSE:Maintenance:1:1";
    const DF_OK: &str = "Filesystem     1024-blocks    Used Available Capacity Mounted on\n\
                         /dev/vda3         41922560 9452292  31929116      23% /\n";
    const LR_OK: &str = "# | Alias | Name | Enabled | GPG Check | Refresh\n\
                         1 | repo-oss | Main | Yes | (r ) Yes | Yes\n";

    fn rpm(core: &str) -> String {
        format!("{RPM_PREFIX}{core} {KERNELS}")
    }

    fn sles_rpm() -> String {
        rpm("rpm zypper glibc systemd bash")
    }

    fn log(stdout: &str, stderr: &str, exit: i16) -> CommandLog {
        CommandLog::new("", stdout, stderr, exit, 0)
    }

    fn system(product: &str, version: &str) -> System {
        System::new(
            SystemProduct::new(product, version, "x86_64"),
            BTreeSet::new(),
            false,
        )
    }

    /// A mock whose every unscripted command fails, so no test passes by
    /// leaving a probe unscripted.
    fn mock(name: &str) -> MockConnection {
        MockConnection::new(name).with_default(log("", "unscripted command", 127))
    }

    /// `rpm -q` answers for `command`: `kernel-default` and every other
    /// queried name but `absent` installed, the other kernels not.
    fn rpm_answer(command: &str, absent: &[&str]) -> String {
        let names = command.strip_prefix(RPM_PREFIX).expect("rpm probe");
        names
            .split_whitespace()
            .map(|n| {
                if absent.contains(&n) || (n.starts_with("kernel-") && n != "kernel-default") {
                    format!("package {n} is not installed\n")
                } else {
                    format!("{n} 1.0-1\n")
                }
            })
            .collect()
    }

    /// A SLES 15 mock answering every probe; `units` is the `systemctl`
    /// answer.
    fn sles_mock(name: &str, rpm_absent: &[&str], units: &str) -> MockConnection {
        let rpm = sles_rpm();
        mock(name)
            .with_response(rpm.clone(), log(&rpm_answer(&rpm, rpm_absent), "", 1))
            .with_response(DF, log(DF_OK, "", 0))
            .with_response(SYSTEMCTL, log(units, "", 0))
            .with_response(NEEDS_REBOOTING, log("", "", 0))
            .with_response(LR, log(LR_OK, "", 0))
    }

    fn target(name: &str, conn: MockConnection, sys: System, root: Transactional) -> Target {
        let mut t = Target::with_connection(name, TargetState::Enabled, Box::new(conn));
        t.set_system(sys, root);
        t
    }

    /// A SLES 15 host whose probes answer as given.
    fn host(name: &str, rpm_absent: &[&str], units: &str) -> (Target, MockConnection) {
        let conn = sles_mock(name, rpm_absent, units);
        let handle = conn.clone();
        (
            target(name, conn, system("SLES", "15-SP5"), Transactional::No),
            handle,
        )
    }

    fn sles_probes() -> Vec<String> {
        vec![
            sles_rpm(),
            DF.to_owned(),
            SYSTEMCTL.to_owned(),
            NEEDS_REBOOTING.to_owned(),
            LR.to_owned(),
        ]
    }

    /// A healthy SLE 11 host: no `systemctl`, no `needs-rebooting`.
    fn sle11(name: &str) -> Target {
        let old_rpm = rpm("rpm zypper glibc bash");
        let conn = mock(name)
            .with_response(old_rpm.clone(), log(&rpm_answer(&old_rpm, &[]), "", 1))
            .with_response(DF, log(DF_OK, "", 0))
            .with_response(LR, log(LR_OK, "", 0));
        target(name, conn, system("SLES", "11-SP3"), Transactional::No)
    }

    #[test]
    fn name_scope_and_flags() {
        assert_eq!(CheckHosts.name(), "check_hosts");
        assert_eq!(CheckHosts.scope(), Scope::Fanout);
        let args = matches(&CheckHosts, &["--json", "-t", "h1"]);
        assert!(args.get_flag("json"));
    }

    #[test]
    fn complete_offers_json_and_hosts() {
        let (h1, _) = host("h1", &[], "");
        let (session, _buf) = session_with_targets(RRID, vec![h1]);
        assert_eq!(
            CheckHosts.complete(&session, "--", "check_hosts --"),
            ["--json"]
        );
        assert_eq!(CheckHosts.complete(&session, "h", "check_hosts h"), ["h1"]);
    }

    #[tokio::test]
    async fn healthy_hosts_with_no_report_loaded_succeed() {
        let (h1, _) = host("h1", &[], "");
        let (mut session, buf) = empty_session();
        session.metadata_mut().base_mut().targets = HostsGroup::new(vec![h1], false);

        CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .expect("a healthy host passes");

        insta::assert_snapshot!(buf.contents());
    }

    /// SL Micro and SLE 11 are not failed for checks that do not apply.
    #[tokio::test]
    async fn healthy_read_only_root_and_sle11_hosts_pass_with_their_skips_named() {
        let micro_conn = mock("micro")
            .with_response(sles_rpm(), log(&rpm_answer(&sles_rpm(), &[]), "", 1))
            .with_response(DF, log(DF_OK, "", 0))
            .with_response(SYSTEMCTL, log("", "", 0))
            .with_response(LR, log(LR_OK, "", 0));
        let (mut session, buf) = session_with_targets(
            RRID,
            vec![
                target(
                    "micro",
                    micro_conn,
                    system("SL-Micro", "6.1"),
                    Transactional::Yes,
                ),
                sle11("old"),
            ],
        );

        CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .expect("both hosts are healthy");

        insta::assert_snapshot!(buf.contents());
    }

    #[tokio::test]
    async fn target_narrows_the_hosts_with_a_report_loaded() {
        let (h1, _) = host("h1", &[], "");
        let (h2, h2_handle) = host("h2", &["glibc"], "");
        let (mut session, buf) = session_with_targets(RRID, vec![h1, h2]);

        CheckHosts
            .call(&mut session, &matches(&CheckHosts, &["-t", "h1"]))
            .await
            .expect("h1 alone is healthy");

        let out = buf.contents();
        assert!(
            out.starts_with("check_hosts: ok — 1 ok, 0 warn, 0 fail, 0 unknown of 1 hosts;"),
            "{out}"
        );
        assert!(out.contains("\nh1: ok\n"), "{out}");
        assert!(!out.contains("h2"), "{out}");
        assert!(h2_handle.commands().is_empty());
    }

    #[tokio::test]
    async fn every_host_is_reported_and_any_fail_or_unknown_fails_the_command() {
        let (good, _) = host("good", &[], "");
        let (broken, _) = host("broken", &["glibc"], "");
        let (warned, _) = host(
            "warned",
            &[],
            "kdump.service loaded failed failed Load kdump\n",
        );
        let rh = target("rh", mock("rh"), system("rhel", "9"), Transactional::No);
        let mut gone = Target::new(
            &mtui_config::Config::default(),
            "gone",
            TargetState::Enabled,
        );
        gone.set_system(system("SLES", "15-SP5"), Transactional::No);
        let (mut off, _) = host("off", &[], "");
        off.set_state(TargetState::Disabled);
        let (mut session, buf) =
            session_with_targets(RRID, vec![good, broken, warned, rh, gone, off]);

        let err = CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "check_hosts: 4 of 6 hosts not healthy: broken (fail), gone (unknown), off (unknown), \
             rh (unknown)"
        );
        insta::assert_snapshot!(buf.contents());
    }

    #[tokio::test]
    async fn warn_alone_does_not_fail_the_command() {
        let (warned, _) = host("w1", &[], "kdump.service loaded failed failed Load kdump\n");
        let (mut session, buf) = session_with_targets(RRID, vec![warned]);
        CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .expect("warn passes");
        assert!(
            buf.contents()
                .starts_with("check_hosts: warn — 0 ok, 1 warn"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn json_is_one_self_describing_document() {
        let (good, _) = host("good", &[], "");
        let (broken, _) = host("broken", &["glibc"], "");
        let (mut off, _) = host("off", &[], "");
        off.set_state(TargetState::Disabled);
        let (mut session, buf) = session_with_targets(RRID, vec![good, broken, off, sle11("old")]);

        let _ = CheckHosts
            .call(&mut session, &matches(&CheckHosts, &["--json"]))
            .await
            .unwrap_err();

        let doc: Value = serde_json::from_str(&buf.contents()).expect("one JSON document");
        insta::assert_snapshot!(serde_json::to_string_pretty(&doc).unwrap());
    }

    #[tokio::test]
    async fn takes_no_lock_and_leaves_another_owners_lock_and_pool_claim_alone() {
        let lock = b"1700000000:alice:4242:their update\n".to_vec();
        let pool = b"1700000000:alice:4242:mtui pool SUSE:Maintenance:9:9\n".to_vec();
        let conn = sles_mock("h1", &[], "")
            .with_file(TARGET_LOCK_PATH, lock.clone())
            .with_file(POOL_LOCK_PATH, pool.clone());
        let handle = conn.clone();
        let t = target("h1", conn, system("SLES", "15-SP5"), Transactional::No);
        let (mut session, _buf) = session_with_targets(RRID, vec![t]);

        CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .expect("healthy");

        assert_eq!(handle.sftp_ops(), Vec::<MockSftpOp>::new());
        assert_eq!(handle.file_contents(TARGET_LOCK_PATH), Some(lock));
        assert_eq!(handle.file_contents(POOL_LOCK_PATH), Some(pool));
        assert_eq!(handle.commands(), sles_probes());
        assert!(!session.targets().get("h1").unwrap().holds_operation_lock());
    }

    /// The probes stay out of the host log, which `show_log` prints and
    /// `export` copies into the install logs.
    #[tokio::test]
    async fn the_probes_leave_nothing_for_show_log_or_export() {
        let (h1, _) = host("h1", &[], "");
        let (mut session, buf) = session_with_targets(RRID, vec![h1]);

        CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .expect("healthy");
        assert!(session.targets().get("h1").unwrap().out().is_empty());

        let before = buf.contents().len();
        crate::commands::ShowLog
            .call(&mut session, &matches(&crate::commands::ShowLog, &[]))
            .await
            .expect("show_log");
        let shown = buf.contents()[before..].to_owned();
        assert!(
            shown.contains("log from h1"),
            "show_log printed nothing: {shown}"
        );
        for probe in sles_probes() {
            let head = probe.lines().next().unwrap().to_owned();
            assert!(!shown.contains(&head), "{head:?} in show_log: {shown}");
        }
    }

    #[tokio::test]
    async fn a_cancel_prints_what_it_has_and_reports_cancelled() {
        let (h1, handle) = host("h1", &[], "");
        let (mut session, buf) = session_with_targets(RRID, vec![h1]);
        session.cancel_token().cancel();

        let err = CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .unwrap_err();

        assert!(matches!(err, CommandError::Cancelled(_)), "{err:?}");
        let out = buf.contents();
        assert!(
            out.starts_with(
                "check_hosts: unknown — 0 ok, 0 warn, 0 fail, 1 unknown of 1 hosts; cancelled"
            ),
            "{out}"
        );
        assert_eq!(out.matches("not run: cancelled").count(), 5, "{out}");
        assert!(handle.commands().is_empty());
    }

    /// The hostile bytes sit on the first stderr line, which the probe quotes,
    /// so only `one_line` can take them out.
    #[tokio::test]
    async fn host_output_is_printed_on_one_sanitised_line() {
        let conn = mock("h1").with_response(
            sles_rpm(),
            log(
                "",
                "error: \u{1b}[31mrpmdb\u{7} broken\u{1b}[0m\tbadly\n",
                1,
            ),
        );
        let t = target("h1", conn, system("SLES", "15-SP5"), Transactional::No);
        let (mut session, buf) = session_with_targets(RRID, vec![t]);

        let _ = CheckHosts
            .call(&mut session, &matches(&CheckHosts, &[]))
            .await
            .unwrap_err();

        let out = buf.contents();
        let row = out
            .lines()
            .find(|l| l.trim_start().starts_with("packages"))
            .expect("packages row");
        assert_eq!(
            row,
            "  packages          unknown rpm exited 1 without an answer for rpm, zypper, glibc, \
             systemd, bash and 8 more: error: rpmdb broken badly"
        );
    }

    #[tokio::test]
    async fn no_hosts_is_an_error_not_a_silent_success() {
        let (mut session, _buf) = empty_session();
        let args = crate::command_parser(&CheckHosts)
            .try_get_matches_from(Vec::<String>::new())
            .expect("argv parses");
        let err = CheckHosts.run(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined), "{err:?}");
    }
}
