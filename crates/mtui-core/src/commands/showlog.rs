//! The `show_log` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::{add_hosts_arg, complete_fanout, page_output, select_names};
use crate::command::{Command, Scope};
use crate::display::CommandPromptDisplay;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Prints the command protocol (issued commands + output) from the hosts.
///
/// Fans each host's log through `display.show_log` into an accumulator and
/// pages the result. Useful for dumping the command history into a
/// template's reproducer section. The per-host command log is snapshotted
/// first so the report borrow does not overlap the display borrow.
pub struct ShowLog;

#[async_trait]
impl Command for ShowLog {
    fn name(&self) -> &'static str {
        "show_log"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Prints the command protocol (issued commands + output) from the hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    /// `show_log` opts out of the driver's host-less skip: it reports each host's
    /// *in-memory* command protocol (`Target::out`), doing no SSH, so it has
    /// meaningful (or harmlessly empty) work even at zero connected hosts. Like
    /// `export`, dumping the protocol across `--all-templates` must not be
    /// silently skipped when a template is host-less. A host-action command keeps
    /// the default `true`; only these local-read commands override it.
    fn skip_hostless_templates(&self) -> bool {
        false
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("offset")
                    .long("offset")
                    .value_name("N")
                    .value_parser(clap::builder::RangedU64ValueParser::<usize>::new().range(1..))
                    .default_value("1")
                    .help("First log entry to show per host (1-based)"),
            )
            .arg(
                Arg::new("limit")
                    .long("limit")
                    .value_name("N")
                    .value_parser(clap::value_parser!(usize))
                    .help("Max log entries per host (0: headers with entry totals only)"),
            )
    }

    fn complete(&self, session: &Session, text: &str, line: &str) -> Vec<String> {
        complete_fanout(
            session,
            &[&["--offset"], &["--limit"]],
            Vec::new(),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let hosts = select_names(session.targets(), args, true)
            .map_err(|e| CommandError::Other(e.to_string()))?;

        // Select before rendering: the MCP byte budget is spent at write time, so
        // a trailing host is only reachable if the window narrows what is written.
        let offset = args.get_one::<usize>("offset").copied().unwrap_or(1);
        let limit = args.get_one::<usize>("limit").copied();
        let windowed = offset != 1 || limit.is_some();

        // Window against the live log, so only the selected entries are cloned;
        // `--limit 0` clones none. `total` is the pre-window count the header reports.
        let per_host: Vec<(String, usize, Vec<(String, String, String, i32)>)> = hosts
            .iter()
            .filter_map(|name| {
                session.targets().get(name).map(|t| {
                    let log = t.out();
                    let total = log.len();
                    let start = offset.saturating_sub(1).min(total);
                    let end = limit.map_or(total, |l| start.saturating_add(l).min(total));
                    let entries = log[start..end]
                        .iter()
                        .map(|c| {
                            (
                                c.command.clone(),
                                c.stdout.clone(),
                                c.stderr.clone(),
                                i32::from(c.exitcode),
                            )
                        })
                        .collect();
                    (name.clone(), total, entries)
                })
            })
            .collect();

        let mut output: Vec<String> = Vec::new();
        for (name, total, entries) in &per_host {
            let mut sink = |line: &str| output.push(line.to_owned());
            CommandPromptDisplay::show_log(
                name,
                entries,
                windowed.then_some((offset, *total)),
                &mut sink,
            );
        }

        page_output(session, &output).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::listversions::ListVersions;
    use crate::commands::testkit::{
        Buffer, empty_session, fake_report, matches, session_scripting, session_scripting_hosts,
        session_scripting_multi, session_with_hosts,
    };

    /// One host running `cmds` in order, so each log entry names its command.
    async fn scripted_session(cmds: &[(&str, &str)]) -> (Session, Buffer) {
        let (mut session, buf) = session_scripting_multi("SUSE:Maintenance:1:1", "h1", cmds);
        for (cmd, _) in cmds {
            session.targets_mut().run(*cmd).await;
        }
        (session, buf)
    }

    /// Three hosts driven to different log lengths: h1 three entries, h2 one,
    /// h3 none. A `PerHost` map skips the hosts it does not name.
    async fn uneven_hosts_session() -> (Session, Buffer) {
        let (mut session, buf) = session_scripting_hosts(
            "SUSE:Maintenance:1:1",
            &["h1", "h2", "h3"],
            &[("c1", "one\n"), ("c2", "two\n"), ("c3", "three\n")],
        );
        for pairs in [
            &[("h1", "c1"), ("h2", "c1")][..],
            &[("h1", "c2")],
            &[("h1", "c3")],
        ] {
            let map: std::collections::BTreeMap<String, String> = pairs
                .iter()
                .map(|(h, c)| ((*h).to_owned(), (*c).to_owned()))
                .collect();
            session.targets_mut().run(map).await;
        }
        (session, buf)
    }

    #[tokio::test]
    async fn windowed_middle_entry_only() {
        let (mut session, buf) = scripted_session(&[
            ("cmd-one", "one-out\n"),
            ("cmd-two", "two-out\n"),
            ("cmd-three", "three-out\n"),
        ])
        .await;
        let args = matches(&ShowLog, &["-t", "h1", "--offset", "2", "--limit", "1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1: (showing entries 2-2 of 3)"),
            "{out}"
        );
        assert!(out.contains("cmd-two"), "{out}");
        assert!(!out.contains("cmd-one"), "{out}");
        assert!(!out.contains("cmd-three"), "{out}");
    }

    #[tokio::test]
    async fn offset_only_windows_to_end() {
        let (mut session, buf) = scripted_session(&[
            ("cmd-one", "one-out\n"),
            ("cmd-two", "two-out\n"),
            ("cmd-three", "three-out\n"),
        ])
        .await;
        let args = matches(&ShowLog, &["-t", "h1", "--offset", "2"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1: (showing entries 2-3 of 3)"),
            "{out}"
        );
        assert!(out.contains("cmd-two"), "{out}");
        assert!(out.contains("cmd-three"), "{out}");
        assert!(!out.contains("cmd-one"), "{out}");
    }

    #[tokio::test]
    async fn limit_zero_is_count_only_probe() {
        let (mut session, buf) =
            scripted_session(&[("cmd-one", "one-out\n"), ("cmd-two", "two-out\n")]).await;
        let args = matches(&ShowLog, &["-t", "h1", "--limit", "0"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1: (showing entries 0 of 2)"),
            "{out}"
        );
        assert!(!out.contains(":~>"), "{out}");
    }

    #[tokio::test]
    async fn offset_past_end_empty_window_no_error() {
        let (mut session, buf) =
            scripted_session(&[("cmd-one", "one-out\n"), ("cmd-two", "two-out\n")]).await;
        let args = matches(&ShowLog, &["-t", "h1", "--offset", "5", "--limit", "2"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1: (showing entries 0 of 2)"),
            "{out}"
        );
        assert!(!out.contains(":~>"), "{out}");
    }

    #[tokio::test]
    async fn limit_zero_reports_each_host_own_total() {
        let (mut session, buf) = uneven_hosts_session().await;
        let args = matches(&ShowLog, &["--limit", "0"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1: (showing entries 0 of 3)"),
            "{out}"
        );
        assert!(
            out.contains("log from h2: (showing entries 0 of 1)"),
            "{out}"
        );
        assert!(
            out.contains("log from h3: (showing entries 0 of 0)"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn window_is_applied_per_host() {
        let (mut session, buf) = uneven_hosts_session().await;
        let args = matches(&ShowLog, &["--offset", "2", "--limit", "1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1: (showing entries 2-2 of 3)"),
            "{out}"
        );
        assert!(out.contains("h1:~> c2 [0]"), "{out}");
        // h2's only entry sits before the window; h3 has none.
        assert!(
            out.contains("log from h2: (showing entries 0 of 1)"),
            "{out}"
        );
        assert!(
            out.contains("log from h3: (showing entries 0 of 0)"),
            "{out}"
        );
        assert!(!out.contains(":~> c1 "), "{out}");
    }

    #[test]
    fn offset_zero_rejected_by_clap_parser() {
        // The synthesised MCP schema erases the range bound, so both surfaces
        // rely on this parse-time rejection.
        let err = ShowLog
            .configure(clap::Command::new("show_log").no_binary_name(true))
            .try_get_matches_from(["--offset", "0"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(err.to_string().contains("--offset"), "{err}");
    }

    #[tokio::test]
    async fn unwindowed_output_byte_identical() {
        let (mut session, buf) =
            scripted_session(&[("cmd-one", "one-out\n"), ("cmd-two", "two-out\n")]).await;
        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        assert_eq!(
            buf.contents(),
            "log from h1:\nh1:~> cmd-one [0]\nstdout:\none-out\n\nstderr:\n\nh1:~> cmd-two [0]\nstdout:\ntwo-out\n\nstderr:\n\n"
        );
    }

    #[test]
    fn complete_offers_own_flags_template_flags_and_hosts() {
        // Completion is part of the command surface: the paging flags must be
        // offered alongside the shared fan-out/template flags and host names.
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "linux");
        let out = ShowLog.complete(&session, "", "show_log ");
        for f in [
            "-t",
            "--target",
            "--offset",
            "--limit",
            "-T",
            "--template",
            "--all-templates",
        ] {
            assert!(out.contains(&f.to_owned()), "missing {f}: {out:?}");
        }
        assert!(out.contains(&"h1".to_owned()), "missing host: {out:?}");
    }

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ShowLog.name(), "show_log");
        assert_eq!(ShowLog.scope(), Scope::Fanout);
    }

    #[test]
    fn opts_out_of_hostless_skip() {
        // show_log reads the in-memory protocol; it must dispatch at zero hosts.
        assert!(!ShowLog.skip_hostless_templates());
    }

    #[test]
    fn ssh_dependent_fanout_command_keeps_default_skip() {
        // Negative control: the audit deliberately left SSH-driven Fanout
        // commands skippable. If this flips, re-run the host-less audit.
        assert!(ListVersions.skip_hostless_templates());
    }

    #[tokio::test]
    async fn runs_across_all_hostless_templates_without_error() {
        // Every loaded template is host-less and no `-t` is named: the driver
        // would skip a default host-action command (→ NoRefhostsDefined), but
        // show_log opts out and must run on each, returning Ok. A headless
        // session with >1 loaded template fans out without an explicit flag.
        let (mut session, _buf) = empty_session();
        session
            .templates
            .add(fake_report("SUSE:Maintenance:1:1", &[], ""));
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &[], ""));
        let args = matches(&ShowLog, &[]);
        ShowLog.run(&mut session, &args).await.unwrap();
    }

    #[tokio::test]
    async fn shows_ran_command_log() {
        // session_scripting echoes the command into the host log.
        let (mut session, buf) =
            session_scripting("SUSE:Maintenance:1:1", "h1", "uname -a", "Linux\n");
        session.targets_mut().run("uname -a").await;
        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("log from h1"), "{out}");
        assert!(out.contains("uname -a"), "{out}");
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    #[allow(unsafe_code)]
    async fn interactive_paging_reads_prompter() {
        use mtui_hosts::Prompter;

        // A tall-enough screen means the whole log fits in one screen and no
        // prompt read is needed; the interactive path must still print it all.
        // `ACCTEST_*` → `#[serial(env)]`.
        unsafe {
            std::env::set_var("ACCTEST_COLS", "80");
            std::env::set_var("ACCTEST_ROWS", "40");
        }
        let (mut session, buf) =
            session_scripting("SUSE:Maintenance:1:1", "h1", "uname -a", "Linux\n");
        session.targets_mut().run("uname -a").await;
        session.is_repl = true;
        session.set_prompter(Prompter::new(std::sync::Arc::new(|_t: String| {
            Box::pin(async move { Ok(String::new()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = std::io::Result<String>> + Send>,
                >
        })));
        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        unsafe {
            std::env::remove_var("ACCTEST_COLS");
            std::env::remove_var("ACCTEST_ROWS");
        }
        let out = buf.contents();
        assert!(
            out.contains("log from h1") && out.contains("uname -a"),
            "{out}"
        );
    }
}
