//! The `show_log` command.

use async_trait::async_trait;
use clap::{Arg, ArgMatches};

use super::support::{add_hosts_arg, complete_fanout, hosts_arg, page_output, select_names};
use crate::command::{Command, Scope};
use crate::display::CommandPromptDisplay;
use crate::error::{CommandError, CommandResult};
use crate::session::{LogEntry, Session};
use mtui_types::enums::TargetState;

/// Prints the command protocol (issued commands + output) from the hosts.
///
/// Fans each host's log through `display.show_log` into an accumulator and pages
/// the result — useful for dumping the command history into a template's
/// reproducer section. The per-host log is snapshotted first so the report borrow
/// does not overlap the display borrow.
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

    /// Opts out of the driver's host-less skip: it reads the *in-memory*
    /// protocol (`Target::out`) with no SSH, so `--all-templates` must not skip a
    /// host-less template. Only such local-read commands override the default.
    fn skip_hostless_templates(&self) -> bool {
        false
    }

    /// Opts into the driver's retained-log ownership: a template holding a
    /// named `-t` host only as a removed host's retained log still dispatches,
    /// so a detached-only fan-out answers instead of skipping everywhere.
    fn includes_detached_hosts(&self) -> bool {
        true
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
        let rrid = session.templates.active_rrid().unwrap_or_default();
        complete_fanout(
            session,
            &[&["--offset"], &["--limit"]],
            session.detached_hosts(rrid),
            line,
            text,
        )
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        // Live hosts plus removed ones with a retained log: `remove_host`
        // keeps the verbatim protocol in the session until `unload`.
        let rrid = session
            .templates
            .active_rrid()
            .unwrap_or_default()
            .to_owned();
        let detached: std::collections::BTreeSet<String> =
            session.detached_hosts(&rrid).into_iter().collect();
        // Names only: whether a selected host reads live or detached is
        // decided at render time from `session.targets()`, so a re-added host
        // reads live and a racing `unload` still errors explicitly-named hosts.
        let requested = hosts_arg(args);
        let explicit = requested
            .as_ref()
            .is_some_and(|names| !names.is_empty() && !names.iter().any(|h| h == "all"));
        let hosts: Vec<String> = if explicit {
            let Some(names) = requested else {
                unreachable!("explicit -t without hostnames");
            };
            let mut selected = Vec::new();
            for name in names {
                let live = session
                    .targets()
                    .get(&name)
                    .is_some_and(|t| t.state() != TargetState::Disabled);
                let gone_with_log = !session.targets().contains(&name) && detached.contains(&name);
                if live || gone_with_log {
                    selected.push(name);
                } else if !session.targets().contains(&name) {
                    return Err(CommandError::Other(
                        mtui_hosts::HostError::NotConnected { host: name }.to_string(),
                    ));
                }
                // Else live but disabled: keeps the old silent drop.
            }
            selected
        } else {
            let live = select_names(session.targets(), args, true)
                .map_err(|e| CommandError::Other(e.to_string()))?;
            // Detached hosts a re-add made live again read from the live
            // group; only the still-gone ones extend the view.
            live.into_iter()
                .chain(
                    detached
                        .into_iter()
                        .filter(|h| !session.targets().contains(h)),
                )
                .collect()
        };

        // Selected before rendering: the MCP byte budget is spent at write time,
        // so a trailing host is only reachable if the window narrows the writes.
        let offset = args.get_one::<usize>("offset").copied().unwrap_or(1);
        let limit = args.get_one::<usize>("limit").copied();
        let windowed = offset != 1 || limit.is_some();
        let start = offset.saturating_sub(1);
        let end = limit.map_or(usize::MAX, |l| start.saturating_add(l));

        // Windowed against the live log, so only selected entries are cloned and
        // `--limit 0` clones none. `total` is the pre-window count for the header.
        // Attached hosts always read live: a disabled one keeps the
        // select_names silent drop and must never resurface its stale retained
        // log as detached. Only a truly gone host reads detached, and a
        // detached entry lost to a racing `unload` still errors when
        // explicitly named.
        let mut per_host: Vec<(String, usize, Vec<LogEntry>)> = Vec::new();
        for name in &hosts {
            if session.targets().contains(name) {
                if let Some(t) = session
                    .targets()
                    .get(name)
                    .filter(|t| t.state() != TargetState::Disabled)
                {
                    let log = t.out();
                    let (total, entries) = windowed_log(log, offset, limit);
                    per_host.push((name.clone(), total, entries));
                }
                continue;
            }
            match session.detached_window(&rrid, name, start, end) {
                Some((total, entries)) => {
                    per_host.push((format!("{name} (detached)"), total, entries))
                }
                None if explicit => {
                    return Err(CommandError::Other(
                        mtui_hosts::HostError::NotConnected { host: name.clone() }.to_string(),
                    ));
                }
                None => {}
            }
        }

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

/// Slice a host log to the `--offset`/`--limit` window, returning the
/// pre-window total for the header plus the cloned entries.
fn windowed_log(
    log: &mtui_types::hostlog::HostLog,
    offset: usize,
    limit: Option<usize>,
) -> (usize, Vec<LogEntry>) {
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
    (total, entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::listversions::ListVersions;
    use crate::commands::removehost::RemoveHost;
    use crate::commands::testkit::{
        Buffer, empty_session, fake_report, matches, session_scripting, session_scripting_hosts,
        session_scripting_multi, session_with_hosts,
    };
    use crate::commands::unload::Unload;

    /// One host running `cmds` in order, so each log entry names its command.
    async fn scripted_session(cmds: &[(&str, &str)]) -> (Session, Buffer) {
        let (mut session, buf) = session_scripting_multi("SUSE:Maintenance:1:1", "h1", cmds);
        for (cmd, _) in cmds {
            session.targets_mut().run(*cmd).await;
        }
        (session, buf)
    }

    /// A removed host's verbatim protocol survives for `show_log` until
    /// `unload`, marked detached.
    #[tokio::test]
    async fn removed_host_log_survives_for_show_log() {
        let (mut session, buf) =
            scripted_session(&[("cmd-one", "one-out\n"), ("cmd-two", "two-out\n")]).await;
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert!(!session.targets().contains("h1"));

        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("log from h1 (detached):"), "{out}");
        assert!(out.contains("cmd-one"), "{out}");
        assert!(out.contains("cmd-two"), "{out}");
    }

    /// A re-added host reads live: the all-view must not duplicate it with
    /// its own retained log.
    #[tokio::test]
    async fn readded_host_shows_live_once_in_all_view() {
        use mtui_hosts::{MockConnection, Target};
        use mtui_types::enums::TargetState;

        let (mut session, buf) = scripted_session(&[("cmd-one", "one-out\n")]).await;
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.detached_hosts("SUSE:Maintenance:1:1"), ["h1"]);

        session.targets_mut().add(Target::with_connection(
            "h1",
            TargetState::Enabled,
            Box::new(MockConnection::new("h1")),
        ));
        let args = matches(&ShowLog, &[]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("log from h1")).count(),
            1,
            "live host must render exactly once, never duplicated: {out}"
        );
        assert!(!out.contains("(detached)"), "{out}");
        assert!(
            !out.contains("cmd-one"),
            "stale detached entries must not render for a live host: {out}"
        );
    }

    /// `unload` drops the retained logs with the template.
    #[tokio::test]
    async fn unload_drops_retained_logs() {
        let (mut session, _buf) = scripted_session(&[("cmd-one", "one-out\n")]).await;
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.detached_hosts("SUSE:Maintenance:1:1"), ["h1"]);

        let args = matches(&Unload, &["SUSE:Maintenance:1:1"]);
        Unload.call(&mut session, &args).await.unwrap();
        assert!(session.detached_hosts("SUSE:Maintenance:1:1").is_empty());

        let args = matches(&ShowLog, &["-t", "h1"]);
        let err = ShowLog.call(&mut session, &args).await.unwrap_err();
        assert!(
            matches!(err, CommandError::Other(_)) && err.to_string().contains("not connected"),
            "{err:?}"
        );
    }

    /// Three hosts at different log lengths — h1 three entries, h2 one, h3 none
    /// — since a `PerHost` map skips the hosts it does not name.
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
        // The paging flags belong on the surface alongside the shared fan-out /
        // template flags and host names.
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
        assert!(!ShowLog.skip_hostless_templates());
    }

    #[test]
    fn ssh_dependent_fanout_command_keeps_default_skip() {
        // Negative control: SSH-driven Fanout commands stay skippable.
        assert!(ListVersions.skip_hostless_templates());
    }

    #[tokio::test]
    async fn runs_across_all_hostless_templates_without_error() {
        // The driver would skip a default host-action command here
        // (NoRefhostsDefined), but show_log opts out and must run on each.
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

        // A tall screen fits the whole log, so no prompt read is needed and the
        // interactive path must still print it all. `ACCTEST_*` is
        // process-global, hence `#[serial(env)]`.
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

    /// A template holding `-t h1` only as a retained log still answers a
    /// `show_log` fan-out: the detached-host rule belongs to this command, so
    /// the driver must not skip the template for it.
    ///
    /// Through `run()`, not `call()`: the skip lives in the fan-out driver.
    #[tokio::test]
    async fn detached_only_template_answers_named_fanout() {
        const OTHER: &str = "SUSE:Maintenance:2:2";
        let (mut session, buf) = scripted_session(&[("cmd-one", "one-out\n")]).await;
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert_eq!(session.detached_hosts("SUSE:Maintenance:1:1"), ["h1"]);
        session.templates.add(fake_report(OTHER, &["h1"], "ok"));

        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.run(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(
            out.contains("log from h1 (detached):"),
            "the detached-only template must answer, not be skipped: {out}"
        );
        assert!(
            out.contains("cmd-one"),
            "the retained protocol must render: {out}"
        );
        assert_eq!(
            out.lines().filter(|l| l.starts_with("log from h1")).count(),
            2,
            "both the detached and the live template answer: {out}"
        );
    }

    /// An explicit `-t` naming one live and one removed host renders each
    /// from its own source: live without the marker, detached with it.
    /// Distinguishes the two render branches — collapsing either into the
    /// other drops the marker or the entries.
    #[tokio::test]
    async fn explicit_mixed_live_and_detached_renders_each_source() {
        let (mut session, buf) = session_scripting_hosts(
            "SUSE:Maintenance:1:1",
            &["h1", "h2"],
            &[("cmd-live", "live-out\n"), ("cmd-gone", "gone-out\n")],
        );
        for pairs in [&[("h1", "cmd-live")][..], &[("h2", "cmd-gone")][..]] {
            let map: std::collections::BTreeMap<String, String> = pairs
                .iter()
                .map(|(h, c)| ((*h).to_owned(), (*c).to_owned()))
                .collect();
            session.targets_mut().run(map).await;
        }
        let args = matches(&RemoveHost, &["-t", "h2"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        assert!(!session.targets().contains("h2"));

        let args = matches(&ShowLog, &["-t", "h1", "-t", "h2"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("log from h1:"), "{out}");
        assert!(!out.contains("log from h1 (detached)"), "{out}");
        assert!(out.contains("cmd-live"), "{out}");
        assert!(out.contains("log from h2 (detached):"), "{out}");
        assert!(out.contains("cmd-gone"), "{out}");
    }

    /// A live but disabled host with a retained log reads as no host at all —
    /// like any disabled host — never its stale log labeled `(detached)`.
    #[tokio::test]
    async fn disabled_readded_host_never_shows_stale_detached() {
        use mtui_hosts::{MockConnection, Target};
        use mtui_types::enums::TargetState;

        let (mut session, buf) = scripted_session(&[("cmd-one", "one-out\n")]).await;
        let args = matches(&RemoveHost, &["-t", "h1"]);
        RemoveHost.call(&mut session, &args).await.unwrap();
        session.targets_mut().add(Target::with_connection(
            "h1",
            TargetState::Disabled,
            Box::new(MockConnection::new("h1")),
        ));

        let args = matches(&ShowLog, &["-t", "h1"]);
        ShowLog.call(&mut session, &args).await.unwrap();
        assert!(
            !buf.contents().contains("(detached)"),
            "an attached host must never show its stale log as detached: {}",
            buf.contents()
        );
        let args = matches(&ShowLog, &[]);
        ShowLog.call(&mut session, &args).await.unwrap();
        assert!(
            !buf.contents().contains("(detached)"),
            "the all-view must not resurface it either: {}",
            buf.contents()
        );
    }
}
