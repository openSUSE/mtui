//! Shared helpers for command bodies.
//!
//! The two cross-cutting helpers every host-phase command reuses: the
//! `-t/--target` argument and the host selection it drives.

use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::openqa::base::OpenQABase;
use mtui_datasources::openqa::build_openqa_client_with_transport;
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_datasources::qem_dashboard::incident::QemIncident;
use mtui_datasources::{HttpClient, OpenQAError, QemDashboardClient};
use mtui_hosts::{HostsGroup, LockOwner};
use mtui_types::{RequestReviewID, UpdateSource};

use crate::error::CommandError;
use crate::session::Session;

/// The wall-clock budget for remote work over an already-established link
/// (`remove_host`'s close fan-out, `unlock --force`, `reload_products`), whose
/// peer may have vanished without closing the link and so never answer. The
/// value is [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT).
///
/// Concurrent fan-outs spend one budget for the whole call; serial work
/// (`reload_products`) spends one *per host*, since sharing one across a serial
/// pass would tie success to fleet size and fail healthy-but-slow fleets.
/// `testkit` shrinks it so the abandon path costs milliseconds.
#[cfg(not(test))]
pub(crate) fn host_op_budget() -> std::time::Duration {
    crate::session::HOST_CLOSE_TIMEOUT
}
#[cfg(test)]
pub(crate) fn host_op_budget() -> std::time::Duration {
    crate::commands::testkit::host_op_budget_override()
}

/// The caveat every contention line carries. `unlock --force` fans out over the
/// **whole group** (`HostsGroup::unlock_force` passes `|_t| true`, and `unlock`
/// does not honour `-t`) once per *loaded* template (`Scope::Fanout`), so a line
/// reached from a `-t`-scoped command must not let it read as "force this one
/// host" — complying would rip a colleague's in-flight lock off hosts that were
/// never contended.
const FORCE_IS_WHOLE_GROUP: &str = "(unlock --force clears the whole group)";

/// Names the owner of a contended operation lock and the next safe step.
///
/// `TargetLock::is_mine` matches the client PID as well as the user (that is
/// what serialises two `mtui`s of one tester against a host), so `session_user`
/// showing up as the owner is *most likely* a second live mtui of theirs and
/// only possibly a strand from a dead one — the two are indistinguishable
/// remotely, so the line hedges and sends the reader to `list_locks` instead of
/// at `--force` (#521). Phrasing tracks
/// [`HostsGroup::update_lock`](mtui_hosts::HostsGroup::update_lock)'s
/// `held by {by} since {time}`.
pub(crate) fn contended_lock_reason(owner: &LockOwner, session_user: &str) -> String {
    if owner.by.is_empty() {
        return format!(
            "held by an unknown owner, possibly a live mtui; check list_locks \
             {FORCE_IS_WHOLE_GROUP}"
        );
    }
    if owner.by == session_user {
        format!(
            "held by {} (you) since {}, possibly another mtui of yours; check list_locks \
             and your other sessions {FORCE_IS_WHOLE_GROUP}",
            owner.by, owner.since
        )
    } else {
        format!(
            "held by {} since {}, possibly a live mtui; check list_locks \
             {FORCE_IS_WHOLE_GROUP}",
            owner.by, owner.since
        )
    }
}

/// Builds the report's [`QemIncident`], threaded into both
/// `DashboardAutoOpenQA` and `KernelOpenQA` so they share one incident state.
///
/// Takes an already-built [`HttpClient`] (from
/// [`Session::http_client`](crate::session::Session::http_client)) to reuse the
/// session connection pool, and plain values rather than `&Session` so callers
/// never hold a non-`Sync` borrow across the `.await`. `source` is the
/// incident's fallback when the dashboard has no record.
pub(crate) async fn build_incident(
    rrid: RequestReviewID,
    dashboard_api: String,
    http: HttpClient,
    source: UpdateSource,
) -> QemIncident {
    let client = QemDashboardClient::with_client(http, dashboard_api);
    QemIncident::with_client(rrid, client, source).await
}

/// A fresh, unpopulated [`DashboardAutoOpenQA`] for the auto workflow on openQA
/// instance `host`, its concurrent per-setting job fetches bounded by
/// `max_parallel`. Call [`DashboardAutoOpenQA::run`] to populate it.
#[must_use]
pub(crate) fn build_auto_openqa(
    host: String,
    incident: &QemIncident,
    rrid: RequestReviewID,
    max_parallel: usize,
) -> DashboardAutoOpenQA {
    DashboardAutoOpenQA::new(host, incident, rrid, max_parallel)
}

/// A fresh, unpopulated [`KernelOpenQA`] connector for openQA instance `host`.
///
/// Resolves API credentials from the standard `client.conf`/`$OPENQA_CONFIG`
/// search path, keyed on the instance host; call [`KernelOpenQA::run`] to
/// populate it. Takes an already-built transport (from
/// [`Session::openqa_transport`](crate::session::Session::openqa_transport)) so
/// a per-host loop (primary + baremetal instances) reuses one connection pool.
///
/// # Errors
///
/// Returns [`OpenQAError::ClientBuild`] if the underlying `ruoqa` client fails
/// to build (a malformed `client.conf`, or an invalid `User-Agent`/API key).
pub(crate) fn build_kernel_openqa(
    incident: &QemIncident,
    host: &str,
    transport: reqwest::Client,
) -> Result<KernelOpenQA, OpenQAError> {
    let client = build_openqa_client_with_transport(transport, host)?;
    let base = OpenQABase::new(client, incident);
    Ok(KernelOpenQA::new(base))
}

/// Guards a command body that requires a loaded update, so a data-source
/// command fails cleanly instead of building a client for an empty RRID.
///
/// # Errors
///
/// [`CommandError::Other`] when the active report is the null object.
pub(crate) fn require_update(
    session: &Session,
) -> Result<mtui_types::RequestReviewID, CommandError> {
    let meta = session.metadata();
    if !meta.is_loaded() {
        return Err(CommandError::Other(
            "Metadata not loaded, please use load_template first".to_owned(),
        ));
    }
    meta.rrid().cloned().ok_or_else(|| {
        CommandError::Other("Metadata not loaded, please use load_template first".to_owned())
    })
}

/// Loaded template RRIDs starting with `text`, for the caller to merge with its
/// flag candidates so `-T/--template` completes.
#[must_use]
pub(crate) fn template_completion(session: &Session, text: &str) -> Vec<String> {
    session
        .templates
        .rrids()
        .into_iter()
        .filter(|rrid| rrid.starts_with(text))
        .collect()
}

/// Filters flag/value choices for tab completion.
///
/// `synonyms` groups interchangeable flags — e.g. `[["-t", "--target"]]`;
/// `extra` carries free-form candidates (hosts, packages, RRIDs); `line` is the
/// line typed so far and `text` the partial word under the cursor.
///
/// * A synonym group already on the line is dropped, bundled short flags
///   (`-abc` ⇒ `-a`, `-b`, `-c`) expanded first.
/// * `text` exactly equalling a candidate short-circuits to just that one;
///   otherwise every candidate starting with `text` is returned.
///
/// Input order is preserved (flags as given, then `extra`) rather than derived
/// from a set, so the menu is deterministic and testable.
#[must_use]
pub(crate) fn complete_choices(
    synonyms: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    // De-duplicated, first-seen order preserved.
    let mut choices: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push =
        |c: String, choices: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
            if seen.insert(c.clone()) {
                choices.push(c);
            }
        };
    for group in synonyms {
        for &flag in *group {
            push(flag.to_owned(), &mut choices, &mut seen);
        }
    }
    for e in extra {
        push(e, &mut choices, &mut seen);
    }

    // Drop any synonym group the already-typed tokens (minus the command name)
    // committed to.
    let mut tokens: Vec<String> = line.split(' ').map(str::to_owned).collect();
    if !tokens.is_empty() {
        tokens.remove(0);
    }
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].clone();
        let bytes = tok.as_bytes();
        if bytes.len() > 2 && bytes[0] == b'-' && bytes[1] != b'-' {
            // Bundled short flags: enqueue each as its own `-x` token.
            for ch in tok[1..].chars() {
                tokens.push(format!("-{ch}"));
            }
            i += 1;
            continue;
        }
        for group in synonyms {
            if group.contains(&tok.as_str()) {
                let drop: std::collections::HashSet<&str> = group.iter().copied().collect();
                choices.retain(|c| !drop.contains(c.as_str()));
            }
        }
        i += 1;
    }

    // Exact match short-circuits to just that candidate.
    if let Some(exact) = choices.iter().find(|c| c.as_str() == text) {
        return vec![exact.clone()];
    }
    choices
        .into_iter()
        .filter(|c| c.starts_with(text))
        .collect()
}

/// Completion for a command offering its own flags plus the template synonym
/// groups (`-T/--template`, `--all-templates`) and the loaded RRIDs, but **no**
/// host names (`commit`, `checkout`, `show_diff`, `put`, …). `extra` carries
/// command-specific free-form candidates.
#[must_use]
pub(crate) fn complete_with_templates(
    session: &Session,
    own_flags: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    let mut groups: Vec<&[&str]> = own_flags.to_vec();
    groups.push(&["-T", "--template"]);
    groups.push(&["--all-templates"]);
    let mut candidates = extra;
    candidates.extend(session.templates.rrids());
    complete_choices(&groups, candidates, line, text)
}

/// Completion for a host-phase (fan-out) command: `-t/--target`, the loaded
/// template RRIDs, and the connected host names, with `extra_flags` prepended
/// as additional synonym groups (a command's own `--force`/`--installed`). The
/// shared shape behind `run`, `reboot`, `prepare`, `update`, `downgrade`,
/// `install`, `uninstall`, `set_repo` and `add_host`.
#[must_use]
pub(crate) fn complete_fanout(
    session: &Session,
    extra_flags: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    let mut groups: Vec<&[&str]> = vec![&["-t", "--target"]];
    groups.extend_from_slice(extra_flags);
    groups.push(&["-T", "--template"]);
    groups.push(&["--all-templates"]);
    let mut candidates: Vec<String> = extra;
    candidates.extend(session.templates.rrids());
    candidates.extend(session.targets().names());
    complete_choices(&groups, candidates, line, text)
}

/// File-path variant of [`complete_choices`].
///
/// Directory entries under the directory part of `text` (basename-prefix
/// filtered, directories carrying a trailing `/`) merged with
/// [`complete_choices`]. A `~` prefix expands; an unreadable directory yields
/// no candidates, so a transient typo does not tear down completion.
#[must_use]
pub(crate) fn complete_choices_filelist(
    synonyms: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    let mut merged = extra;
    merged.extend(complete_path(text));
    complete_choices(synonyms, merged, line, text)
}

/// Expands a leading tilde in a completion path: `~`/`~/…` → `$HOME`,
/// `~user`/`~user/…` → that user's home (getpwnam). Best-effort, per the
/// completer's "transient input must not tear down completion" convention: an
/// unknown user, no `$HOME`, or a non-Unix target leaves `text` unexpanded.
fn expand_tilde(text: &str) -> String {
    let Some(rest) = text.strip_prefix('~') else {
        return text.to_owned();
    };

    // Bare `~` or `~/…`: the user segment is empty.
    if rest.is_empty() || rest.starts_with('/') {
        return match std::env::var_os("HOME") {
            Some(home) => format!("{}{rest}", home.to_string_lossy()),
            None => text.to_owned(),
        };
    }

    // `~user` or `~user/…`: resolve the named user's home via getpwnam.
    let (user, tail) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    resolve_user_home(user).map_or_else(|| text.to_owned(), |home| format!("{home}{tail}"))
}

/// The home directory of a named user via getpwnam, or `None` when it can't be
/// resolved — including every non-Unix target, where the caller leaves the
/// tilde unexpanded.
#[cfg(unix)]
fn resolve_user_home(user: &str) -> Option<String> {
    nix::unistd::User::from_name(user)
        .ok()
        .flatten()
        .map(|u| u.dir.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn resolve_user_home(_user: &str) -> Option<String> {
    None
}

/// Directory entries matching the basename prefix in `text`, shared by the
/// `edit`/`put` file completers. Directories carry a trailing `/`; a `~`/`~user`
/// prefix expands; a bare prefix completes against the current directory.
/// Best-effort: an unreadable directory yields no candidates.
#[must_use]
pub(crate) fn complete_path(text: &str) -> Vec<String> {
    use std::path::Path;

    let expanded = expand_tilde(text);

    let (dir, prefix) = match expanded.rfind('/') {
        // Keep the trailing slash so the re-joined candidate stays anchored.
        Some(idx) => (expanded[..=idx].to_owned(), expanded[idx + 1..].to_owned()),
        None => (String::new(), expanded.clone()),
    };
    let read_dir = if dir.is_empty() {
        std::fs::read_dir(Path::new("."))
    } else {
        std::fs::read_dir(Path::new(&dir))
    };
    let Ok(entries) = read_dir else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let mut candidate = format!("{dir}{name}");
        if is_dir {
            candidate.push('/');
        }
        out.push(candidate);
    }
    out.sort();
    out
}

/// Adds the repeatable `-t/--target` host argument: [`ArgAction::Append`], so
/// each occurrence names one host. Omitted, every enabled host is acted on.
pub fn add_hosts_arg(cmd: clap::Command) -> clap::Command {
    cmd.arg(
        Arg::new("hosts")
            .short('t')
            .long("target")
            .value_name("HOST")
            .action(ArgAction::Append)
            .help(
                "Host to act on. Can be used multiple times. \
                 If omitted all hosts are used",
            ),
    )
}

/// The parsed `-t/--target` hostnames, or `None` when the flag was omitted.
/// `None` is distinct from `Some([])` (which clap never produces for an
/// `Append` arg): callers read it as "all enabled hosts".
#[must_use]
pub(crate) fn hosts_arg(args: &ArgMatches) -> Option<Vec<String>> {
    args.try_get_many::<String>("hosts")
        .ok()
        .flatten()
        .map(|it| it.cloned().collect())
}

/// Whether the invocation named explicit `-t` hosts. The fan-out skip rule keys
/// on this: with no explicit `-t` a host-phase command may be skipped on a
/// template with no connected host, but a typo'd `-t` must fail loudly.
#[must_use]
pub(crate) fn named_hosts(args: &ArgMatches) -> bool {
    hosts_arg(args).is_some_and(|v| !v.is_empty())
}

/// Resolves the hostnames a host-phase command acts on, **without** consuming
/// the group.
///
/// * `-t host …` → exactly those hosts (membership-validated; only the enabled
///   among them when `enabled`).
/// * no `-t` → every enabled host.
/// * the deprecated `-t all` → every enabled host, with a warning.
///
/// Names rather than a new group, because `HostsGroup::select` consumes the
/// group and drops the unselected hosts — which a state-preserving command
/// (`run`, `reboot`) must not do. The caller drives the subset in place via a
/// [`Command::PerHost`](mtui_hosts::Command) map keyed on them.
///
/// # Errors
///
/// Returns [`HostError::NotConnected`](mtui_hosts::HostError) when a named
/// host is not in the group, except for the deprecated `all` sentinel which
/// degrades to every enabled host.
pub fn select_names(
    group: &HostsGroup,
    args: &ArgMatches,
    enabled: bool,
) -> Result<Vec<String>, mtui_hosts::HostError> {
    let is_enabled = |name: &str| {
        !enabled
            || group
                .get(name)
                .is_some_and(|t| t.state() != mtui_types::enums::TargetState::Disabled)
    };

    match hosts_arg(args) {
        Some(hosts) if !hosts.is_empty() && !hosts.iter().any(|h| h == "all") => {
            for name in &hosts {
                if !group.contains(name) {
                    return Err(mtui_hosts::HostError::NotConnected { host: name.clone() });
                }
            }
            Ok(hosts.into_iter().filter(|h| is_enabled(h)).collect())
        }
        Some(_) => {
            tracing::info!("Using all hosts. Warning: option 'all' is deprecated");
            Ok(group
                .names()
                .into_iter()
                .filter(|h| is_enabled(h))
                .collect())
        }
        None => Ok(group
            .names()
            .into_iter()
            .filter(|h| is_enabled(h))
            .collect()),
    }
}

/// Builds a [`Command::PerHost`](mtui_hosts::Command) map that runs `command` on
/// exactly `hosts`, leaving every other host in the group untouched.
#[must_use]
pub(crate) fn per_host(command: &str, hosts: &[String]) -> mtui_hosts::Command {
    mtui_hosts::Command::PerHost(
        hosts
            .iter()
            .map(|h| (h.clone(), command.to_owned()))
            .collect(),
    )
}

/// Pages `output` through the session's display: the REPL drives
/// [`page_interactive`](crate::display::page_interactive), reading the Enter/`q`
/// continuation through the session's serialised
/// [`Prompter`](mtui_hosts::Prompter); headless callers take
/// [`page`](crate::display::page), which forwards every line unpaged so output
/// stays byte-identical. The prompter is cloned before the mutable `display`
/// borrow to sidestep the split borrow.
pub(crate) async fn page_output(session: &mut Session, output: &[String]) {
    if session.is_repl {
        let prompter = session.prompter().cloned();
        crate::display::page_interactive(output, &mut session.display, prompter.as_ref()).await;
    } else {
        let mut writer = |line: &str| session.display.println(line);
        crate::display::page(output, false, Some(&mut writer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_types::enums::TargetState;

    fn cmd() -> clap::Command {
        add_hosts_arg(clap::Command::new("t").no_binary_name(true))
    }

    /// The three contention lines are one family: each names the owner, hedges
    /// the inference it draws from the name, and keeps `unlock --force`'s
    /// whole-group scope visible instead of reading as "force this one host"
    /// (#521). The own/foreign pair must also stay distinguishable in *both*
    /// directions, so inverting the predicate cannot pass.
    #[test]
    fn contended_lock_reason_hedges_and_scopes_the_force_remedy() {
        let alice = LockOwner {
            by: "alice".to_owned(),
            since: "Tuesday, 14.11.2023 22:13 UTC".to_owned(),
        };
        let foreign = contended_lock_reason(&alice, "bob");
        let mine = contended_lock_reason(&alice, "alice");
        let unknown = contended_lock_reason(&LockOwner::default(), "bob");

        for line in [&foreign, &mine, &unknown] {
            assert!(line.contains("list_locks"), "{line}");
            assert!(
                line.contains("unlock --force clears the whole group"),
                "{line}"
            );
            assert!(line.contains("possibly"), "hedge missing: {line}");
        }
        assert!(
            foreign.contains("held by alice since Tuesday, 14.11.2023 22:13 UTC"),
            "{foreign}"
        );
        assert!(
            foreign.contains("possibly a live mtui")
                && !foreign.contains("(you)")
                && !foreign.contains("mtui of yours"),
            "{foreign}"
        );
        assert!(
            mine.contains("held by alice (you) since Tuesday, 14.11.2023 22:13 UTC")
                && mine.contains("possibly another mtui of yours")
                && mine.contains("check list_locks and your other sessions"),
            "{mine}"
        );
        assert!(!mine.contains("possibly a live mtui"), "{mine}");
        assert!(
            unknown.contains("held by an unknown owner") && !unknown.contains("override"),
            "{unknown}"
        );
    }

    #[test]
    fn complete_choices_offers_flags_and_extras_by_prefix() {
        let out = complete_choices(
            &[&["-t", "--target"]],
            vec!["host1".to_owned(), "host2".to_owned()],
            "run ",
            "",
        );
        assert_eq!(out, vec!["-t", "--target", "host1", "host2"]);

        // Prefix filter on a flag.
        assert_eq!(
            complete_choices(&[&["-t", "--target"]], vec![], "run ", "--"),
            vec!["--target"]
        );
        // Prefix filter on an extra.
        assert_eq!(
            complete_choices(&[&["-t"]], vec!["host1".to_owned()], "run ", "ho"),
            vec!["host1"]
        );
    }

    #[test]
    fn complete_choices_drops_used_synonym_group() {
        // `-t` already typed → neither `-t` nor `--target` is offered again.
        let out = complete_choices(
            &[&["-t", "--target"]],
            vec!["host1".to_owned()],
            "run -t host1 ",
            "",
        );
        assert_eq!(out, vec!["host1"]);
        // The long form counts too.
        let out = complete_choices(&[&["-t", "--target"]], vec![], "run --target host1 ", "-");
        assert!(out.is_empty());
    }

    #[test]
    fn complete_choices_expands_bundled_short_flags() {
        // `-if` on the line consumes both the `-i/-f` groups.
        let out = complete_choices(
            &[
                &["-i", "--installed"],
                &["-f", "--force"],
                &["-t", "--target"],
            ],
            vec![],
            "prepare -if ",
            "-",
        );
        // Only the -t group survives.
        assert_eq!(out, vec!["-t", "--target"]);
    }

    #[test]
    fn complete_choices_exact_match_short_circuits() {
        let out = complete_choices(
            &[&["-t", "--target"]],
            vec!["host1".to_owned()],
            "run ",
            "host1",
        );
        assert_eq!(out, vec!["host1"]);
    }

    #[test]
    fn complete_choices_dedupes_preserving_order() {
        let out = complete_choices(
            &[&["-t"]],
            vec!["-t".to_owned(), "host1".to_owned(), "host1".to_owned()],
            "run ",
            "",
        );
        assert_eq!(out, vec!["-t", "host1"]);
    }

    #[test]
    fn complete_path_lists_and_marks_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("apex")).unwrap();
        let base = format!("{}/", dir.path().display());

        let a = complete_path(&format!("{base}a"));
        assert!(a.iter().any(|c| c.ends_with("alpha.txt")));
        assert!(a.iter().any(|c| c.ends_with("apex/")));
    }

    #[test]
    fn complete_path_unreadable_is_empty() {
        assert!(complete_path("/no/such/dir/x").is_empty());
    }

    #[test]
    fn expand_tilde_bare_and_slash_use_home() {
        let home = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned());
        let Some(home) = home else {
            return; // no HOME in this environment; nothing to assert.
        };
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/a/b"), format!("{home}/a/b"));
    }

    #[test]
    fn expand_tilde_non_tilde_passthrough() {
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("rel/path"), "rel/path");
    }

    #[cfg(unix)]
    #[test]
    fn expand_tilde_resolves_named_user_home() {
        // The *current* user is always in the password DB, so the test is
        // hermetic and assumes nothing about root/nobody.
        let Some(me) = nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
        else {
            return;
        };
        let home = me.dir.to_string_lossy().into_owned();
        let name = &me.name;
        assert_eq!(expand_tilde(&format!("~{name}")), home);
        assert_eq!(expand_tilde(&format!("~{name}/x")), format!("{home}/x"));
    }

    #[test]
    fn expand_tilde_unknown_user_is_unexpanded() {
        let text = "~nosuchuser123456/x";
        assert_eq!(expand_tilde(text), text);
    }

    #[test]
    fn complete_path_unknown_user_is_empty() {
        assert!(complete_path("~nosuchuser123456/x").is_empty());
    }

    #[test]
    fn complete_choices_filelist_merges_files_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("payload.bin"), "x").unwrap();
        let path = format!("{}/pay", dir.path().display());

        let out = complete_choices_filelist(&[&["-t"]], vec![], "put ", &path);
        assert!(out.iter().any(|c| c.ends_with("payload.bin")));
    }

    fn parse(argv: &[&str]) -> ArgMatches {
        cmd().try_get_matches_from(argv).unwrap()
    }

    fn group(hosts: &[(&str, TargetState)]) -> HostsGroup {
        let targets = hosts
            .iter()
            .map(|(h, state)| {
                Target::with_connection(*h, *state, Box::new(MockConnection::new(*h)))
            })
            .collect();
        HostsGroup::new(targets, false)
    }

    #[test]
    fn hosts_arg_none_when_omitted_some_when_given() {
        assert!(hosts_arg(&parse(&[])).is_none());
        assert_eq!(
            hosts_arg(&parse(&["-t", "a", "-t", "b"])),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
    }

    #[test]
    fn named_hosts_reflects_flag() {
        assert!(!named_hosts(&parse(&[])));
        assert!(named_hosts(&parse(&["-t", "a"])));
    }

    #[test]
    fn select_names_all_enabled_when_omitted() {
        let g = group(&[("h1", TargetState::Enabled), ("h2", TargetState::Enabled)]);
        let mut names = select_names(&g, &parse(&[]), true).unwrap();
        names.sort();
        assert_eq!(names, vec!["h1", "h2"]);
    }

    #[test]
    fn select_names_drops_disabled_when_enabled() {
        let g = group(&[("h1", TargetState::Enabled), ("h2", TargetState::Disabled)]);
        assert_eq!(select_names(&g, &parse(&[]), true).unwrap(), vec!["h1"]);
        // enabled=false keeps disabled hosts.
        let mut all = select_names(&g, &parse(&[]), false).unwrap();
        all.sort();
        assert_eq!(all, vec!["h1", "h2"]);
    }

    #[test]
    fn select_names_named_subset() {
        let g = group(&[("h1", TargetState::Enabled), ("h2", TargetState::Enabled)]);
        assert_eq!(
            select_names(&g, &parse(&["-t", "h2"]), true).unwrap(),
            vec!["h2"]
        );
    }

    #[test]
    fn select_names_unknown_host_errors() {
        let g = group(&[("h1", TargetState::Enabled)]);
        let err = select_names(&g, &parse(&["-t", "ghost"]), true).unwrap_err();
        assert!(matches!(err, mtui_hosts::HostError::NotConnected { host } if host == "ghost"));
    }

    #[test]
    fn select_names_all_sentinel_is_every_host() {
        let g = group(&[("h1", TargetState::Enabled), ("h2", TargetState::Enabled)]);
        let mut names = select_names(&g, &parse(&["-t", "all"]), true).unwrap();
        names.sort();
        assert_eq!(names, vec!["h1", "h2"]);
    }

    #[test]
    fn require_update_errors_when_unloaded() {
        use crate::commands::testkit::{empty_session, session_with_hosts};
        let (session, _buf) = empty_session();
        let err = require_update(&session).unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));

        // A loaded report yields its RRID.
        let (session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let rrid = require_update(&session).unwrap();
        assert_eq!(rrid.to_string(), "SUSE:Maintenance:1:1");
    }

    #[test]
    fn template_completion_offers_loaded_rrids_by_prefix() {
        use crate::commands::testkit::{fake_report, session_with_hosts};
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        let mut all = template_completion(&session, "");
        all.sort();
        assert_eq!(all, vec!["SUSE:Maintenance:1:1", "SUSE:Maintenance:2:2"]);
        // Prefix filter.
        assert_eq!(
            template_completion(&session, "SUSE:Maintenance:2"),
            vec!["SUSE:Maintenance:2:2"]
        );
        assert!(template_completion(&session, "nope").is_empty());
    }

    #[test]
    fn per_host_covers_only_named() {
        let c = per_host("echo hi", &["h1".to_owned()]);
        match c {
            mtui_hosts::Command::PerHost(m) => {
                assert_eq!(m.get("h1").map(String::as_str), Some("echo hi"));
                assert!(!m.contains_key("h2"));
            }
            mtui_hosts::Command::All(_) => panic!("expected PerHost"),
        }
    }
}
