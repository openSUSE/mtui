//! The `run` command.

use std::collections::BTreeSet;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};
use mtui_hosts::LockOutcome;

use super::support::{
    add_hosts_arg, complete_fanout, contended_lock_reason, page_output, per_host, select_names,
};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Runs a command on a specified host or on all enabled targets.
///
/// Dispatched in parallel across every selected target; each host's input line,
/// exit code, stdout and any stderr are then paged to the display.
///
/// The positional tokens are re-quoted with `shlex::join`, so a token carrying
/// shell metacharacters (`sh -c "a; b"`, `$(...)`) reaches the remote shell
/// intact instead of being re-split by it.
pub struct Run;

#[async_trait]
impl Command for Run {
    fn name(&self) -> &'static str {
        "run"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Runs a command on a specified host or on all enabled targets.")
    }

    fn scope(&self) -> Scope {
        Scope::Explicit
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("command")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .value_name("COMMAND")
                .help(
                    "Command as argv tokens (no shell); pipelines need three tokens: sh, -c, <line>",
                ),
        )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(session, &[], Vec::new(), line, text)
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let tokens: Vec<String> = args
            .get_many::<String>("command")
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        let command = shlex::try_join(tokens.iter().map(String::as_str))
            .map_err(|e| CommandError::Other(format!("invalid command: {e}")))?;

        let session_user = session.config.session_user.clone();
        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }

        // Every selected host must be `Acquired`. Unlike `hostslock`,
        // `Contended` and `Failed` block: running while another owner holds the
        // operation lock breaks the serialization it exists for. Anything else
        // rolls back and aborts without running.
        let selected: BTreeSet<String> = hosts.iter().cloned().collect();
        let outcomes = targets.lock_selected("", &selected).await;

        // Classify under the `targets` borrow; display writes would be a second
        // `session` borrow, so they wait until it is released.
        let mut acquired: BTreeSet<String> = BTreeSet::new();
        let mut blocked: Vec<String> = Vec::new();
        let mut report: Vec<String> = Vec::new();
        for (host, outcome) in &outcomes {
            match outcome {
                LockOutcome::Acquired => {
                    acquired.insert(host.clone());
                }
                LockOutcome::Contended(owner) => {
                    report.push(format!(
                        "{host}: skipped, {}",
                        contended_lock_reason(owner, &session_user)
                    ));
                    blocked.push(host.clone());
                }
                LockOutcome::Failed(reason) => {
                    report.push(format!("{host}: lock FAILED ({reason})"));
                    blocked.push(host.clone());
                }
                LockOutcome::Released => {}
            }
        }

        if !blocked.is_empty() {
            if !acquired.is_empty() {
                targets.unlock_selected(&acquired).await;
            }
            for line in &report {
                session.display.println(line);
            }
            blocked.sort();
            return Err(CommandError::Other(format!(
                "could not lock: {}",
                blocked.join(", ")
            )));
        }

        targets.run(per_host(&command, &hosts)).await;
        targets.unlock_selected(&selected).await;

        let mut output: Vec<String> = Vec::new();
        // A non-zero remote exit is often expected, so this stays `Ok` and the
        // summary line below is the signal. Collected under the `targets`
        // borrow; hosts that ran nothing are skipped.
        let mut failed: Vec<(String, i16)> = Vec::new();
        for name in &hosts {
            let Some(t) = targets.get(name) else {
                continue;
            };
            output.push(format!(
                "{name}:-> {} [{}]",
                t.lastin(),
                fmt_exit(t.lastexit())
            ));
            output.extend(t.lastout().split('\n').map(str::to_owned));
            if !t.lasterr().is_empty() {
                output.push("stderr:".to_owned());
                output.extend(t.lasterr().split('\n').map(str::to_owned));
            }
            if let Some(code) = t.lastexit()
                && code != 0
            {
                failed.push((name.clone(), code));
            }
        }

        page_output(session, &output).await;

        if !failed.is_empty() {
            failed.sort();
            let summary = failed
                .iter()
                .map(|(name, code)| format!("{name} (exit {code})"))
                .collect::<Vec<_>>()
                .join(", ");
            session.display.println(&format!("FAILED on {summary}"));
        }
        Ok(())
    }
}

/// Renders an optional exit code, printing `None` when absent.
fn fmt_exit(code: Option<i16>) -> String {
    match code {
        Some(c) => c.to_string(),
        None => "None".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{
        empty_session, matches, session_scripting_multi, session_with_hosts,
    };

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Run.name(), "run");
        assert_eq!(Run.scope(), Scope::Explicit);
    }

    #[test]
    fn fmt_exit_renders_none_and_code() {
        assert_eq!(fmt_exit(None), "None");
        assert_eq!(fmt_exit(Some(0)), "0");
        assert_eq!(fmt_exit(Some(7)), "7");
    }

    #[test]
    fn complete_offers_target_flag_templates_and_hosts() {
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "linux");
        // Empty tail → the -t flag, the loaded RRID, and every host name.
        let all = Run.complete(&session, "", "run ");
        assert!(all.contains(&"-t".to_owned()), "{all:?}");
        assert!(all.contains(&"--target".to_owned()), "{all:?}");
        assert!(all.contains(&"SUSE:Maintenance:1:1".to_owned()), "{all:?}");
        assert!(
            all.contains(&"h1".to_owned()) && all.contains(&"h2".to_owned()),
            "{all:?}"
        );

        assert_eq!(Run.complete(&session, "h1", "run h1"), vec!["h1"]);

        // Once -t is on the line, neither synonym is offered again.
        let after = Run.complete(&session, "", "run -t h1 ");
        assert!(!after.contains(&"-t".to_owned()) && !after.contains(&"--target".to_owned()));
    }

    #[test]
    fn complete_on_empty_session_does_not_panic() {
        let (session, _buf) = empty_session();
        let out = Run.complete(&session, "-", "run -");
        assert!(out.contains(&"-t".to_owned()));
    }

    #[tokio::test]
    async fn runs_across_all_hosts_and_aggregates_output() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "linux");
        let args = matches(&Run, &["uname", "-a"]);
        Run.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        // `lastin` here is the mock's canned empty-command log; the issued
        // command's shaping is asserted by the echoing mock below.
        assert!(out.contains("h1:->"), "missing h1 banner: {out}");
        assert!(out.contains("h2:->"), "missing h2 banner: {out}");
        assert_eq!(out.matches("[0]").count(), 2, "both hosts exit 0: {out}");
        assert_eq!(out.matches("linux").count(), 2, "both stdout: {out}");
    }

    #[tokio::test]
    async fn quotes_metacharacters_as_a_single_token() {
        // The mock echoes the exact command it received into `lastin`.
        let (mut session, buf) = session_scripting_multi(
            "SUSE:Maintenance:1:1",
            "h1",
            &[("sh -c 'a; b'", "done"), ("sh -c 'a | b'", "done")],
        );
        let args = matches(&Run, &["sh", "-c", "a; b"]);
        Run.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.targets().get("h1").unwrap().lastin(),
            "sh -c 'a; b'"
        );
        assert!(buf.contents().contains("h1:-> sh -c 'a; b' [0]"));

        // The pipeline form the COMMAND help sends callers to must survive
        // try_join as three tokens, not collapse into one quoted word.
        let args = matches(&Run, &["sh", "-c", "a | b"]);
        Run.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.targets().get("h1").unwrap().lastin(),
            "sh -c 'a | b'"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_appends_failed_summary_but_returns_ok() {
        use crate::commands::testkit::session_with_targets;
        use mtui_hosts::{MockConnection, Target};
        use mtui_types::enums::TargetState;
        use mtui_types::hostlog::CommandLog;

        // The summary lists only the failures, sorted, and the command still
        // succeeds.
        let targets: Vec<Target> = [("h1", 0i16), ("h3", 127), ("h2", 1)]
            .into_iter()
            .map(|(name, code)| {
                let conn =
                    MockConnection::new(name).with_default(CommandLog::new("", "out", "", code, 0));
                Target::with_connection(name, TargetState::Enabled, Box::new(conn))
            })
            .collect();
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", targets);
        let args = matches(&Run, &["false"]);
        Run.call(&mut session, &args).await.unwrap();

        let out = buf.contents();
        assert!(
            out.contains("FAILED on h2 (exit 1), h3 (exit 127)"),
            "missing/wrong summary: {out}"
        );
    }

    #[tokio::test]
    async fn all_zero_exit_appends_no_failed_summary() {
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1", "h2"], "linux");
        let args = matches(&Run, &["true"]);
        Run.call(&mut session, &args).await.unwrap();
        assert!(
            !buf.contents().contains("FAILED on"),
            "unexpected summary: {}",
            buf.contents()
        );
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    async fn interactive_pages_output_and_keeps_failed_summary() {
        use crate::commands::testkit::session_with_targets;
        use mtui_hosts::{MockConnection, Prompter, Target};
        use mtui_types::enums::TargetState;
        use mtui_types::hostlog::CommandLog;

        // A tiny screen forces paging and the prompter quits after the first
        // screen; the FAILED summary prints after the body, so it must survive
        // that. `ACCTEST_*` is process-global, hence `#[serial(env)]`.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "3");
        }
        let targets: Vec<Target> = [("h1", 0i16), ("h2", 1)]
            .into_iter()
            .map(|(name, code)| {
                let conn =
                    MockConnection::new(name).with_default(CommandLog::new("", "out", "", code, 0));
                Target::with_connection(name, TargetState::Enabled, Box::new(conn))
            })
            .collect();
        let (mut session, buf) = session_with_targets("SUSE:Maintenance:1:1", targets);
        session.is_repl = true;
        session.set_prompter(Prompter::new(std::sync::Arc::new(|_t: String| {
            Box::pin(async move { Ok("q".to_owned()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        })));
        let args = matches(&Run, &["false"]);
        Run.call(&mut session, &args).await.unwrap();
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }

        let out = buf.contents();
        assert!(
            out.contains("FAILED on h2 (exit 1)"),
            "summary must survive an early quit: {out}"
        );
    }

    #[tokio::test]
    async fn no_hosts_is_no_refhosts_defined() {
        let (mut session, _buf) = empty_session();
        let args = matches(&Run, &["true"]);
        let err = Run.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::NoRefhostsDefined));
    }

    #[tokio::test]
    async fn unknown_named_host_errors() {
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let args = matches(&Run, &["-t", "ghost", "true"]);
        let err = Run.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    /// Where a foreign lock is planted to script a `Contended` outcome; mirrors
    /// `TARGET_LOCK_PATH` in `mtui-hosts`.
    const LOCK_PATH: &str = "/var/lock/mtui.lock";

    use crate::commands::testkit::session_with_targets;
    use mtui_hosts::{MockConnection, Target};
    use mtui_types::enums::TargetState;
    use mtui_types::hostlog::CommandLog;

    /// A free, enabled host that locks cleanly (Acquired) and echoes its run.
    fn free_host(name: &str) -> Target {
        let conn = MockConnection::new(name).with_default(CommandLog::new("", "ok", "", 0, 0));
        Target::with_connection(name, TargetState::Enabled, Box::new(conn))
    }

    /// An enabled host carrying a foreign operation lock → `Contended`.
    fn foreign_locked_host(name: &str) -> Target {
        let conn = MockConnection::new(name)
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(LOCK_PATH, b"1700000000:alice:4242:busy".to_vec());
        Target::with_connection(name, TargetState::Enabled, Box::new(conn))
    }

    /// An enabled host whose lock-file write hard-fails → `Failed`.
    fn lock_failing_host(name: &str) -> Target {
        let conn = MockConnection::new(name)
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_exclusive_write_error(LOCK_PATH);
        Target::with_connection(name, TargetState::Enabled, Box::new(conn))
    }

    /// An enabled host carrying *this* user's lock stamped by a different PID.
    ///
    /// `TargetLock::is_mine` matches on the PID too, so this is contention —
    /// but it is the caller's own leftover, and the report must say so (#521).
    fn own_stranded_lock_line() -> Vec<u8> {
        let me = mtui_config::Config::default().session_user;
        format!("1700000000:{me}:{}", std::process::id() + 1).into_bytes()
    }

    #[tokio::test]
    async fn contended_host_aborts_without_running_and_rolls_back() {
        // One `Contended` host must abort the whole run: neither host executes,
        // and h1's acquired lock is rolled back.
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![free_host("h1"), foreign_locked_host("h2")],
        );
        // Pin the caller's identity so the "not you" branch is the one taken
        // whatever `$USER` the suite runs as.
        session.config.session_user = "bob".to_owned();
        let args = matches(&Run, &["true"]);
        let err = Run.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not lock") && m.contains("h2")),
            "expected lock-abort error naming h2, got {err:?}"
        );
        let out = buf.contents();
        assert!(
            out.contains(
                "h2: skipped, held by alice since Tuesday, 14.11.2023 22:13 UTC, possibly a \
                 live mtui; check list_locks (unlock --force clears the whole group)"
            ),
            "{out}"
        );
        assert!(
            !out.contains("mtui of yours") && !out.contains("(you)"),
            "a colleague's lock must not be reported as the caller's own: {out}"
        );

        let targets = session.targets_mut();
        assert!(targets.get("h1").unwrap().lastexit().is_none(), "h1 ran");
        assert!(targets.get("h2").unwrap().lastexit().is_none(), "h2 ran");
        assert!(
            !targets.get_mut("h1").unwrap().is_locked().await.unwrap(),
            "h1 lock not rolled back"
        );
    }

    #[tokio::test]
    async fn contention_on_the_callers_own_stranded_lock_names_them() {
        // Same user, foreign PID: still not `is_mine` (the PID check serialises
        // one tester's concurrent zypper transactions), so the report must name
        // the caller — hedged, because that signature is a *live* sibling mtui
        // as readily as a strand — and send them to `list_locks` rather than at
        // the whole-group `--force` (#521). The lock stays put.
        let line = own_stranded_lock_line();
        let conn = MockConnection::new("h1")
            .with_default(CommandLog::new("", "ok", "", 0, 0))
            .with_file(LOCK_PATH, line.clone());
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![Target::with_connection(
                "h1",
                TargetState::Enabled,
                Box::new(conn.clone()),
            )],
        );
        let me = session.config.session_user.clone();
        let args = matches(&Run, &["true"]);
        let err = Run.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not lock") && m.contains("h1")),
            "{err:?}"
        );
        let out = buf.contents();
        assert!(out.contains(&format!("held by {me} (you)")), "{out}");
        assert!(out.contains("possibly another mtui of yours"), "{out}");
        assert!(
            out.contains("check list_locks and your other sessions"),
            "{out}"
        );
        assert!(
            out.contains("unlock --force clears the whole group"),
            "{out}"
        );
        assert!(!out.contains("possibly a live mtui"), "{out}");
        assert_eq!(conn.file_contents(LOCK_PATH), Some(line));
        assert!(
            session
                .targets_mut()
                .get("h1")
                .unwrap()
                .lastexit()
                .is_none(),
            "h1 ran under a lock it does not hold"
        );
    }

    #[tokio::test]
    async fn lock_failure_aborts_without_running_and_rolls_back() {
        // A `Failed` host aborts on the same terms as a `Contended` one.
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![free_host("h1"), lock_failing_host("h2")],
        );
        let args = matches(&Run, &["true"]);
        let err = Run.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not lock") && m.contains("h2")),
            "expected lock-abort error naming h2, got {err:?}"
        );
        assert!(buf.contents().contains("h2: lock FAILED"));

        let targets = session.targets_mut();
        assert!(targets.get("h1").unwrap().lastexit().is_none(), "h1 ran");
        assert!(targets.get("h2").unwrap().lastexit().is_none(), "h2 ran");
        assert!(
            !targets.get_mut("h1").unwrap().is_locked().await.unwrap(),
            "h1 lock not rolled back"
        );
    }

    #[tokio::test]
    async fn unselected_bad_host_does_not_block_scoped_run() {
        // h2 is foreign-locked but unselected, so its lock must stay untouched.
        let (mut session, _buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![free_host("h1"), foreign_locked_host("h2")],
        );
        let args = matches(&Run, &["-t", "h1", "true"]);
        Run.call(&mut session, &args).await.unwrap();

        let targets = session.targets_mut();
        assert_eq!(
            targets.get("h1").unwrap().lastexit(),
            Some(0),
            "h1 should have run"
        );
        assert!(
            targets.get("h2").unwrap().lastexit().is_none(),
            "unselected h2 must not run"
        );
        assert!(
            targets.get_mut("h2").unwrap().is_locked().await.unwrap(),
            "unselected h2 lock must be untouched"
        );
    }

    #[tokio::test]
    async fn all_acquired_runs_and_unlocks_selected() {
        let (mut session, _buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![free_host("h1"), free_host("h2")],
        );
        let args = matches(&Run, &["true"]);
        Run.call(&mut session, &args).await.unwrap();

        let targets = session.targets_mut();
        assert_eq!(targets.get("h1").unwrap().lastexit(), Some(0));
        assert_eq!(targets.get("h2").unwrap().lastexit(), Some(0));
        assert!(!targets.get_mut("h1").unwrap().is_locked().await.unwrap());
        assert!(!targets.get_mut("h2").unwrap().is_locked().await.unwrap());
    }
}
