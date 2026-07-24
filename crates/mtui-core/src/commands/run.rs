//! The `run` command.

use std::collections::BTreeSet;

use async_trait::async_trait;
use clap::{Arg, ArgMatches};
use mtui_hosts::LockOutcome;

use super::support::{add_hosts_arg, complete_fanout, page_output, per_host, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Runs a command on a specified host or on all enabled targets.
///
/// Ports upstream `mtui.commands.run.Run`. The command is dispatched in
/// parallel across every selected target; after it returns, each host's input
/// line, exit code, stdout, and any stderr are collected and paged to the
/// display.
///
/// The positional command tokens are quoted back together with `shlex::join`
/// before being sent, so a single token containing shell metacharacters (e.g.
/// `sh -c "a; b"` or `$(...)`) survives the trip to the remote shell intact
/// instead of being re-split by it.
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
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("command")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .value_name("COMMAND")
                .help("Command to run on refhost"),
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

        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }

        // The operation lock guards the serialized remote transaction, mirroring
        // upstream's `with LockedTargets(...)` around the run. Lock exactly the
        // selected hosts (not the whole group) and require every one to be
        // `Acquired` before running: unlike `hostslock`, a `Contended` host is a
        // blocker here, because the operation lock exists to serialize this
        // remote transaction — running while another owner holds it would break
        // that guarantee. A `Failed` (transport error) host is likewise a
        // blocker. On any non-`Acquired` host we roll back the locks we did
        // acquire and abort without running.
        let selected: BTreeSet<String> = hosts.iter().cloned().collect();
        let outcomes = targets.lock_selected("", &selected).await;

        // Classify while `targets` is borrowed; defer display writes (which would
        // be a second `session` borrow) until after the borrow is released.
        let mut acquired: BTreeSet<String> = BTreeSet::new();
        let mut blocked: Vec<String> = Vec::new();
        let mut report: Vec<String> = Vec::new();
        for (host, outcome) in &outcomes {
            match outcome {
                LockOutcome::Acquired => {
                    acquired.insert(host.clone());
                }
                LockOutcome::Contended => {
                    report.push(format!("{host}: locked by another owner (skipped)"));
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
            // Roll back the locks we acquired this call, then abort without
            // running the command on any host.
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
        // A non-zero remote exit is often expected (this stays `Ok`, matching
        // upstream), but is collected here — while `targets` is still borrowed —
        // to append one explicit summary line naming each failed host so the
        // LLM/user gets an unambiguous signal. Hosts with no command run
        // (`lastexit() == None`) are skipped.
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

        // Page the aggregated per-host output, matching upstream `run.py`'s
        // `page(output, ...)`. Interactive → screen-at-a-time; headless → every
        // line forwarded unpaged (byte-identical to before).
        page_output(session, &output).await;

        if !failed.is_empty() {
            // Sorted for determinism.
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

/// Renders an optional exit code the way upstream `lastexit()` stringifies it.
fn fmt_exit(code: Option<i16>) -> String {
    match code {
        Some(c) => c.to_string(),
        None => "None".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_scripting, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(Run.name(), "run");
        assert_eq!(Run.scope(), Scope::Fanout);
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

        // Prefix filter on a host name.
        assert_eq!(Run.complete(&session, "h1", "run h1"), vec!["h1"]);

        // Once -t is on the line it is no longer offered (synonym removal).
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
        // The exit code and stdout are aggregated per host. `lastin` reflects the
        // mock's canned (empty-command) log; the issued-command shaping is
        // asserted separately via a command-echoing mock.
        assert!(out.contains("h1:->"), "missing h1 banner: {out}");
        assert!(out.contains("h2:->"), "missing h2 banner: {out}");
        assert_eq!(out.matches("[0]").count(), 2, "both hosts exit 0: {out}");
        assert_eq!(out.matches("linux").count(), 2, "both stdout: {out}");
    }

    #[tokio::test]
    async fn quotes_metacharacters_as_a_single_token() {
        // `sh -c "a; b"` must reach the host as one quoted script, not re-split.
        // The mock echoes the exact command it received into `lastin`.
        let (mut session, buf) =
            session_scripting("SUSE:Maintenance:1:1", "h1", "sh -c 'a; b'", "done");
        let args = matches(&Run, &["sh", "-c", "a; b"]);
        Run.call(&mut session, &args).await.unwrap();
        assert_eq!(
            session.targets().get("h1").unwrap().lastin(),
            "sh -c 'a; b'"
        );
        assert!(buf.contents().contains("h1:-> sh -c 'a; b' [0]"));
    }

    #[tokio::test]
    async fn nonzero_exit_appends_failed_summary_but_returns_ok() {
        use crate::commands::testkit::session_with_targets;
        use mtui_hosts::{MockConnection, Target};
        use mtui_types::enums::TargetState;
        use mtui_types::hostlog::CommandLog;

        // h1 exits 0, h2 exits 1, h3 exits 127 — the summary lists only the
        // failures, sorted by hostname, and the command still succeeds.
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

        // Tiny screen so the aggregated output actually needs paging, and a
        // prompter that answers `q` to quit after the first screen. The FAILED
        // summary is printed *after* the paged body, so it must survive an early
        // quit. `ACCTEST_*` is process-global → `#[serial(env)]`.
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
        // The summary is unpaged and appended after the (possibly truncated) body.
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

    /// The operation-lock file path a foreign lock is planted at to script a
    /// `Contended` outcome (mirrors `TARGET_LOCK_PATH` in `mtui-hosts`).
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

    #[tokio::test]
    async fn contended_host_aborts_without_running_and_rolls_back() {
        // h1 free (Acquired), h2 foreign-locked (Contended). The whole run must
        // abort: the command runs on neither host, and h1's acquired lock is
        // rolled back.
        let (mut session, buf) = session_with_targets(
            "SUSE:Maintenance:1:1",
            vec![free_host("h1"), foreign_locked_host("h2")],
        );
        let args = matches(&Run, &["true"]);
        let err = Run.call(&mut session, &args).await.unwrap_err();

        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("could not lock") && m.contains("h2")),
            "expected lock-abort error naming h2, got {err:?}"
        );
        assert!(buf.contents().contains("h2: locked by another owner"));

        let targets = session.targets_mut();
        // Neither host ran the command.
        assert!(targets.get("h1").unwrap().lastexit().is_none(), "h1 ran");
        assert!(targets.get("h2").unwrap().lastexit().is_none(), "h2 ran");
        // h1's acquired lock was rolled back.
        assert!(
            !targets.get_mut("h1").unwrap().is_locked().await.unwrap(),
            "h1 lock not rolled back"
        );
    }

    #[tokio::test]
    async fn lock_failure_aborts_without_running_and_rolls_back() {
        // h1 free (Acquired), h2 transport-failure on lock (Failed). Abort,
        // don't run, roll back h1.
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
        // h2 is foreign-locked but NOT selected: `-t h1` must run on h1 only and
        // never touch h2's lock.
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
        // h2's foreign lock is untouched (still present).
        assert!(
            targets.get_mut("h2").unwrap().is_locked().await.unwrap(),
            "unselected h2 lock must be untouched"
        );
    }

    #[tokio::test]
    async fn all_acquired_runs_and_unlocks_selected() {
        // Happy path: both hosts lock cleanly, run, then unlock.
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
