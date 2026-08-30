//! Tests the fan-out contract: `Command::run` resolving templates and driving
//! `call` across them.

mod support;

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use clap::ArgMatches;
use mtui_core::commands::support::add_hosts_arg;
use mtui_core::{Command, CommandError, CommandResult, Scope, Session};
use support::FakeReport;

/// A command that records the RRID it ran against (via the active pointer) and
/// optionally fails on a configured set of RRIDs.
struct MockCommand {
    scope: Scope,
    fail_on: HashSet<String>,
    skip_hostless: bool,
    ran: Mutex<Vec<String>>,
}

impl MockCommand {
    fn new(scope: Scope) -> Self {
        Self {
            scope,
            fail_on: HashSet::new(),
            skip_hostless: true,
            ran: Mutex::new(Vec::new()),
        }
    }
    fn failing(scope: Scope, fail_on: &[&str]) -> Self {
        Self {
            scope,
            fail_on: fail_on.iter().map(|s| (*s).to_owned()).collect(),
            skip_hostless: true,
            ran: Mutex::new(Vec::new()),
        }
    }
    /// A command that opts out of the driver's host-less skip (like `export`).
    fn no_hostless_skip(scope: Scope) -> Self {
        Self {
            scope,
            fail_on: HashSet::new(),
            skip_hostless: false,
            ran: Mutex::new(Vec::new()),
        }
    }
    fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }
}

#[async_trait]
impl Command for MockCommand {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn scope(&self) -> Scope {
        self.scope
    }
    fn skip_hostless_templates(&self) -> bool {
        self.skip_hostless
    }
    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
    }
    async fn call(&self, session: &mut Session, _args: &ArgMatches) -> CommandResult {
        let rrid = session.metadata().id();
        self.ran.lock().unwrap().push(rrid.clone());
        if self.fail_on.contains(&rrid) {
            return Err(CommandError::Other(format!("boom on {rrid}")));
        }
        Ok(())
    }
}

/// Builds `ArgMatches` for [`MockCommand`] from a set of raw argv tokens,
/// through the real [`mtui_core::engine::command_parser`] — the shared base
/// parser (`-T/--template`, `--all-templates`) plus `MockCommand`'s own `-t`,
/// so the flags' shape can never drift from what `run` actually parses.
fn matches(argv: &[&str]) -> ArgMatches {
    let cmd = MockCommand::new(Scope::Active);
    mtui_core::engine::command_parser(&cmd)
        .try_get_matches_from(argv)
        .expect("parse mock argv")
}

fn session(is_repl: bool, reports: &[(&str, &[&str])]) -> Session {
    let mut s = Session::new(mtui_config::Config::default(), is_repl);
    for (rrid, hosts) in reports {
        s.templates.add(FakeReport::with_hosts(rrid, hosts).boxed());
    }
    s
}

// --- _resolve_templates branches (observed through which RRIDs `call` ran on) ---

#[tokio::test]
async fn active_scope_runs_active_only() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Active);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    // Active is the first-added ("a").
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn explicit_template_hit_runs_that_one() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Active);
    cmd.run(&mut s, &matches(&["-T", "b"])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["b".to_owned()]);
}

#[tokio::test]
async fn explicit_template_miss_errors() {
    let mut s = session(true, &[("a", &["h1"])]);
    let cmd = MockCommand::new(Scope::Active);
    let err = cmd.run(&mut s, &matches(&["-T", "zzz"])).await.unwrap_err();
    assert!(matches!(err, CommandError::TemplateNotLoaded(r) if r == "zzz"));
    assert!(cmd.ran().is_empty());
}

#[tokio::test]
async fn single_scope_never_fans_out() {
    // Even headless with several loaded, Single runs exactly once (active).
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Single);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn fanout_scope_runs_every_template() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

#[tokio::test]
async fn all_templates_flag_forces_fanout() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Active);
    cmd.run(&mut s, &matches(&["--all-templates"]))
        .await
        .unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

#[tokio::test]
async fn headless_multi_load_defaults_to_fanout() {
    // MCP (interactive=false) with >1 loaded and no scope → fan-out.
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Active);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

// --- tri-state `--all-templates` ---

#[tokio::test]
async fn suppressed_all_templates_headless_multi_load_is_ambiguous() {
    // `--all-templates=false` must not read as unset (today's inertness): a
    // `Fanout` command headlessly with several loaded and no active pointer
    // to fall back to must refuse, not silently fan out anyway.
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    let err = cmd
        .run(&mut s, &matches(&["--all-templates=false"]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CommandError::AmbiguousTemplate { .. }),
        "got: {err:?}"
    );
    assert!(cmd.ran().is_empty(), "the body must not have run at all");
}

#[tokio::test]
async fn suppressed_all_templates_repl_multi_load_runs_active_once() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&["--all-templates=false"]))
        .await
        .unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn absent_all_templates_flag_still_fans_out() {
    // Anti-vacuity for the two tests above: over-reading the tri-state as
    // `Some(false)` when the flag is simply absent must not also refuse.
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

// --- `Scope::Explicit` ---

#[tokio::test]
async fn explicit_scope_headless_multi_load_with_no_flags_is_ambiguous() {
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Explicit);
    let err = cmd.run(&mut s, &matches(&[])).await.unwrap_err();
    assert!(
        matches!(err, CommandError::AmbiguousTemplate { .. }),
        "got: {err:?}"
    );
    assert!(cmd.ran().is_empty(), "the body must have run zero times");
}

#[tokio::test]
async fn explicit_scope_single_loaded_runs_without_naming_it() {
    // Anti-vacuity for the test above: a lone loaded template must still run,
    // never refused just for having `Scope::Explicit`.
    let mut s = session(false, &[("a", &["h1"])]);
    let cmd = MockCommand::new(Scope::Explicit);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn explicit_scope_repl_multi_load_runs_the_active_one_once() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Explicit);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn explicit_scope_all_templates_flag_still_fans_out() {
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Explicit);
    cmd.run(&mut s, &matches(&["--all-templates"]))
        .await
        .unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

#[tokio::test]
async fn bare_all_templates_flag_still_forces_fanout() {
    // `default_missing_value` must still make the bare flag mean `Some(true)`.
    let mut s = session(false, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&["--all-templates"]))
        .await
        .unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

// --- run() aggregation contract ---

#[tokio::test]
async fn single_template_error_propagates_directly() {
    let mut s = session(true, &[("a", &["h1"])]);
    let cmd = MockCommand::failing(Scope::Active, &["a"]);
    let err = cmd.run(&mut s, &matches(&[])).await.unwrap_err();
    // Not wrapped in FanOut — single-template path preserves the direct error.
    assert!(matches!(err, CommandError::Other(_)));
}

#[tokio::test]
async fn partial_failure_raises_fanout_aggregate() {
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::failing(Scope::Fanout, &["b"]);
    let err = cmd.run(&mut s, &matches(&[])).await.unwrap_err();
    match err {
        CommandError::FanOut { failures, stop } => {
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].0, "b");
            // Nothing stopped this fan-out: every template ran, so the
            // aggregate carries no stop note to mislead the caller with.
            assert_eq!(stop, None, "an uninterrupted fan-out has no stop note");
        }
        other => panic!("expected FanOut, got {other:?}"),
    }
    // Every template still got its turn.
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

#[tokio::test]
async fn hostless_templates_are_skipped_when_no_hosts_named() {
    // Fan-out over two templates, one with no connected host: it is skipped and
    // does not fail the run.
    let mut s = session(true, &[("a", &["h1"]), ("b", &[])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn all_skipped_raises_no_refhosts() {
    // Every fanned-out template lacks a connected host and none named via -t:
    // the command ran nowhere, which must be an error, not a silent success.
    let mut s = session(true, &[("a", &[]), ("b", &[])]);
    let cmd = MockCommand::new(Scope::Fanout);
    let err = cmd.run(&mut s, &matches(&[])).await.unwrap_err();
    assert!(matches!(err, CommandError::NoRefhostsDefined));
    assert!(cmd.ran().is_empty());
}

/// A command exercising every default trait method (no overrides but the two
/// required ones).
struct DefaultCommand;

#[async_trait]
impl Command for DefaultCommand {
    fn name(&self) -> &'static str {
        "default"
    }
    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        Ok(())
    }
}

#[tokio::test]
async fn default_trait_methods_are_sensible() {
    let cmd = DefaultCommand;
    assert_eq!(cmd.name(), "default");
    assert_eq!(cmd.aliases(), &[] as &[&str]);
    assert_eq!(cmd.scope(), Scope::Active);
    // Host-less templates are skippable by default (host-action commands).
    assert!(cmd.skip_hostless_templates());
    // configure is the identity (command unchanged).
    let base = clap::Command::new("default");
    assert_eq!(cmd.configure(base).get_name(), "default");

    let mut s = session(true, &[("a", &["h1"])]);
    assert!(cmd.complete(&s, "", "").is_empty());
    // run() drives the default body against the active template.
    cmd.run(&mut s, &matches(&[])).await.unwrap();
}

#[tokio::test]
async fn named_host_skips_a_template_that_does_not_own_it() {
    // `-t h1` names a host only template "a" owns: "b" (host-less here, but
    // the same rule applies to a template owning other, non-named hosts) is
    // skipped rather than dispatched into an `HostNotConnected` failure
    // (#575) — the named-host analogue of the host-less skip above.
    let mut s = session(true, &[("a", &["h1"]), ("b", &[])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&["-t", "h1"])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn named_host_in_one_of_two_templates_only_that_one_runs() {
    // Anti-vacuity for the skip above, on hosts that are genuinely connected
    // rather than merely absent: "b" owns "h2", not the named "h1", and must
    // still be skipped rather than run or fail.
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    cmd.run(&mut s, &matches(&["--all-templates", "-t", "h1"]))
        .await
        .unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned()]);
}

#[tokio::test]
async fn named_unknown_host_across_all_templates_is_no_refhosts_not_fanout() {
    // No loaded template owns "nosuchhost": every one is skipped, which must
    // still surface as `NoRefhostsDefined` (ran nowhere), not a silent Ok and
    // not a `FanOut` aggregate of per-template `HostNotConnected` failures.
    let mut s = session(true, &[("a", &["h1"]), ("b", &["h2"])]);
    let cmd = MockCommand::new(Scope::Fanout);
    let err = cmd
        .run(&mut s, &matches(&["-t", "nosuchhost"]))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CommandError::NoRefhostsDefined),
        "got: {err:?}"
    );
    assert!(cmd.ran().is_empty());
}

#[tokio::test]
async fn opt_out_of_hostless_skip_dispatches_every_template() {
    // A command overriding `skip_hostless_templates()` to `false` (like `export`
    // for Auto/Kernel workflows) is dispatched into host-less templates instead
    // of being skipped up front, even with no `-t` named.
    let mut s = session(true, &[("a", &["h1"]), ("b", &[])]);
    let cmd = MockCommand::no_hostless_skip(Scope::Fanout);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}

#[tokio::test]
async fn opt_out_of_hostless_skip_runs_all_hostless() {
    // With every template host-less and the skip disabled, the command still
    // runs on each (rather than the all-skipped -> NoRefhostsDefined path).
    let mut s = session(true, &[("a", &[]), ("b", &[])]);
    let cmd = MockCommand::no_hostless_skip(Scope::Fanout);
    cmd.run(&mut s, &matches(&[])).await.unwrap();
    assert_eq!(cmd.ran(), vec!["a".to_owned(), "b".to_owned()]);
}
