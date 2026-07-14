//! Shared helpers for command bodies.
//!
//! Ports the two cross-cutting `_command.py` helpers every host-phase command
//! reuses: the `-t/--target` argument (`_add_hosts_arg`) and the host selection
//! it drives (`parse_hosts`).

use clap::{Arg, ArgAction, ArgMatches};
use mtui_datasources::openqa::base::OpenQABase;
use mtui_datasources::openqa::client::{ApiCredentials, ClientConf, OpenQAClient};
use mtui_datasources::openqa::kernel::KernelOpenQA;
use mtui_datasources::qem_dashboard::dashboard_openqa::DashboardAutoOpenQA;
use mtui_datasources::qem_dashboard::incident::QemIncident;
use mtui_datasources::{HttpClient, VerifyPolicy, resolve_verify};
use mtui_hosts::HostsGroup;
use mtui_types::RequestReviewID;

use crate::error::CommandError;
use crate::session::Session;

/// Resolves the TLS-verify policy from the session config (the openQA / QEM
/// connectors' shared verify seam).
#[must_use]
pub fn config_verify_policy(session: &Session) -> VerifyPolicy {
    resolve_verify(
        VerifyPolicy::Default(true),
        Some(VerifyPolicy::from_config(&session.config.ssl_verify)),
    )
}

/// Builds the report's [`QemIncident`] (the shared handle both openQA connectors
/// build on).
///
/// Mirrors upstream, which constructs a single `QEMIncident(config, rrid)` and
/// threads it into `DashboardAutoOpenQA` / `KernelOpenQA`. Takes plain values
/// rather than `&Session` so callers never hold a (non-`Sync`) `Session` borrow
/// across the `.await`.
///
/// # Errors
///
/// [`CommandError::Other`] when the shared HTTP client cannot be built.
pub async fn build_incident(
    rrid: RequestReviewID,
    dashboard_api: String,
    policy: VerifyPolicy,
) -> Result<QemIncident, CommandError> {
    QemIncident::new(rrid, dashboard_api, policy)
        .await
        .map_err(|e| CommandError::Other(format!("could not query QEM Dashboard: {e}")))
}

/// Builds a fresh, unpopulated [`DashboardAutoOpenQA`] for the auto workflow on
/// the given openQA instance `host` (upstream `DashboardAutoOpenQA(config,
/// config.openqa_instance, incident, rrid)`). Call [`DashboardAutoOpenQA::run`]
/// to populate it.
#[must_use]
pub fn build_auto_openqa(
    host: String,
    incident: &QemIncident,
    rrid: RequestReviewID,
) -> DashboardAutoOpenQA {
    DashboardAutoOpenQA::new(host, incident, rrid)
}

/// Builds a fresh, unpopulated [`KernelOpenQA`] connector for a given openQA
/// instance `host` (upstream `KernelOpenQA(config, host, incident, rrid)`).
///
/// Resolves openQA API credentials from the standard `client.conf` search
/// paths, keyed on the instance host. Call [`KernelOpenQA::run`] to populate it.
///
/// # Errors
///
/// [`CommandError::Other`] when the shared HTTP client cannot be built.
pub fn build_kernel_openqa(
    incident: &QemIncident,
    host: &str,
    policy: VerifyPolicy,
) -> Result<KernelOpenQA, CommandError> {
    let http = HttpClient::new(policy)
        .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))?;
    let server = host
        .rsplit("://")
        .next()
        .unwrap_or(host)
        .trim_end_matches('/');
    let creds: ApiCredentials = ApiCredentials::resolve(&ClientConf::load(), server, host);
    let client = OpenQAClient::new(http, host.to_owned(), creds);
    let base = OpenQABase::new(client, &incident.rrid, incident);
    Ok(KernelOpenQA::new(base))
}

/// Guards a command body that requires a loaded update (upstream
/// `@requires_update`).
///
/// Returns [`CommandError::Other`] with the upstream message when no report is
/// loaded, so a data-source command errors cleanly instead of building a client
/// for an empty RRID. On success returns the active report's
/// [`RequestReviewID`](mtui_types::RequestReviewID).
///
/// # Errors
///
/// [`CommandError::Other`] when the active report is the null object.
pub fn require_update(session: &Session) -> Result<mtui_types::RequestReviewID, CommandError> {
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

/// Tab-completion candidates that offer every loaded template RRID (upstream
/// `template_completion`).
///
/// Returned RRIDs that start with `text` are offered; the caller merges these
/// with any flag candidates. Mirrors upstream, which lets `-T/--template` be
/// completed with the loaded RRIDs.
#[must_use]
pub fn template_completion(session: &Session, text: &str) -> Vec<String> {
    session
        .templates
        .rrids()
        .into_iter()
        .filter(|rrid| rrid.starts_with(text))
        .collect()
}

/// Filters flag/value choices for tab completion (upstream `complete_choices`).
///
/// `synonyms` groups interchangeable flags — e.g. `[["-t", "--target"]]`; `extra`
/// carries the free-form candidates (host names, package names, template RRIDs).
/// `line` is the command line typed so far and `text` the partial word under the
/// cursor.
///
/// Behaviour mirrors upstream exactly:
/// * A synonym group already present on the line is dropped from the offered set
///   (once you type `-t`, neither `-t` nor `--target` is suggested again).
/// * A bundled short-flag token (`-abc`, i.e. a single `-` followed by two or
///   more non-`-` chars) is expanded to `-a`, `-b`, `-c` for that matching.
/// * If `text` exactly equals a candidate, only that candidate is returned (the
///   completion is already satisfied).
/// * Otherwise every candidate starting with `text` is returned.
///
/// Unlike upstream — which derives the result from a `set` and so returns an
/// unstable order — this preserves the input order (flags first in the order
/// given, then `extra`), which keeps the menu deterministic and testable.
#[must_use]
pub fn complete_choices(
    synonyms: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    // Ordered candidate list (flags in given order, then extras), de-duplicated
    // while preserving first-seen order.
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

    // Walk the already-typed tokens (skip the command name) and remove any
    // synonym group the user has already committed to. Expand bundled short
    // flags (`-abc` → `-a -b -c`) first, exactly like upstream.
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

    // Exact match short-circuits to just that candidate (upstream parity).
    if let Some(exact) = choices.iter().find(|c| c.as_str() == text) {
        return vec![exact.clone()];
    }
    choices
        .into_iter()
        .filter(|c| c.starts_with(text))
        .collect()
}

/// Completion for a command that offers its own flags plus the template synonym
/// groups (`-T/--template`, `--all-templates`) and the loaded RRIDs, but **no**
/// host names (upstream commands like `add_host`, `commit`, `checkout`,
/// `show_diff`, `put` that pass only `template_completion` to `complete_choices`).
///
/// `extra` carries any command-specific free-form candidates.
#[must_use]
pub fn complete_with_templates(
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
/// template RRIDs (upstream `template_completion`), and the connected host names.
///
/// `extra_flags` are prepended as additional synonym groups (e.g. a command's own
/// `--force`/`--installed` options). This is the shared shape behind `run`,
/// `reboot`, `prepare`, `update`, `downgrade`, `install`, `uninstall`, `set_repo`,
/// and `add_host`.
#[must_use]
pub fn complete_fanout(
    session: &Session,
    extra_flags: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    // `-t/--target`, the command's own flags, then the template synonym groups
    // (`-T/--template`, `--all-templates`) — upstream `template_completion`.
    let mut groups: Vec<&[&str]> = vec![&["-t", "--target"]];
    groups.extend_from_slice(extra_flags);
    groups.push(&["-T", "--template"]);
    groups.push(&["--all-templates"]);
    // Loaded RRIDs, then any command-specific extras (packages), then host names.
    let mut candidates: Vec<String> = extra;
    candidates.extend(session.templates.rrids());
    candidates.extend(session.targets().names());
    complete_choices(&groups, candidates, line, text)
}

/// File-path variant of [`complete_choices`] (upstream `complete_choices_filelist`).
///
/// Offers directory entries under the directory part of `text` (basename-prefix
/// filtered, directories carrying a trailing `/`) merged with the flag/value
/// choices from [`complete_choices`]. A `~` prefix expands to the home directory;
/// an unreadable directory yields no file candidates (a transient typo must not
/// tear down completion).
#[must_use]
pub fn complete_choices_filelist(
    synonyms: &[&[&str]],
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    let mut merged = extra;
    merged.extend(complete_path(text));
    complete_choices(synonyms, merged, line, text)
}

/// Expands a leading tilde in a completion path, mirroring Python's
/// `os.path.expanduser` for the forms mtui's file completer sees.
///
/// - `~` / `~/…` → `$HOME` (+ the remainder), as before.
/// - `~user` / `~user/…` → that user's home directory (getpwnam), so completing
///   another user's tree resolves to real absolute candidates.
///
/// Best-effort, matching the completer's "transient input must not tear down
/// completion" convention: an unknown user, a degraded environment (no `$HOME`),
/// or a non-Unix target leaves `text` unexpanded.
fn expand_tilde(text: &str) -> String {
    let Some(rest) = text.strip_prefix('~') else {
        return text.to_owned();
    };

    // Bare `~` or `~/…`: split off the user segment (empty here) from `/rest`.
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

/// The home directory of a named user, or `None` when it can't be resolved.
///
/// Uses getpwnam on Unix; non-Unix targets can't resolve arbitrary users, so the
/// caller falls back to leaving the tilde unexpanded.
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

/// Lists directory entries matching the basename prefix in `text` (shared by the
/// `edit`/`put` file completers).
///
/// Splits `text` into a directory part and a basename prefix, lists that
/// directory, and offers entries whose name starts with the prefix (directories
/// carry a trailing `/`). A `~`/`~user` prefix expands to the corresponding home
/// directory. A bare prefix completes against the current directory.
/// Best-effort: an unreadable directory yields no candidates.
#[must_use]
pub fn complete_path(text: &str) -> Vec<String> {
    use std::path::Path;

    // `~` / `~/…` / `~user` / `~user/…` → expand (upstream `expanduser`).
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

/// Adds the repeatable `-t/--target` host argument (upstream `_add_hosts_arg`).
///
/// `action="append"` upstream → [`ArgAction::Append`] here: the flag may be
/// given more than once, each occurrence naming one host. When omitted, the
/// command acts on every enabled host.
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
///
/// `None` (no `-t`) is distinct from `Some([])` (which clap never produces for
/// an `Append` arg) — callers use the `None` case to mean "all enabled hosts",
/// matching upstream's `if self.args.hosts:` branch.
#[must_use]
pub fn hosts_arg(args: &ArgMatches) -> Option<Vec<String>> {
    args.try_get_many::<String>("hosts")
        .ok()
        .flatten()
        .map(|it| it.cloned().collect())
}

/// Whether the invocation named explicit `-t` hosts.
///
/// The fan-out skip rule (`_command.py`) keys on this: a host-phase command with
/// no explicit `-t` may be skipped on a template with no connected host, but a
/// typo'd `-t` must fail loudly.
#[must_use]
pub fn named_hosts(args: &ArgMatches) -> bool {
    hosts_arg(args).is_some_and(|v| !v.is_empty())
}

/// Resolves the hostnames a host-phase command acts on (upstream `parse_hosts`),
/// **without** consuming the group.
///
/// * `-t host …` → exactly those hosts (validated against membership; only the
///   enabled among them when `enabled`).
/// * no `-t` → every enabled host.
/// * the deprecated `-t all` → every enabled host, with a warning (upstream
///   keeps the `all` escape hatch for backwards compatibility).
///
/// Returns hostnames (sorted, as [`HostsGroup::names`] yields) rather than a new
/// group: `HostsGroup::select` consumes the group and drops the unselected
/// hosts, which a state-preserving command (`run`, `reboot`) must not do. The
/// caller drives the subset in place via a
/// [`Command::PerHost`](mtui_hosts::Command) map keyed on the returned names.
///
/// # Errors
///
/// Returns [`HostError::NotConnected`](mtui_hosts::HostError) when a named host
/// is not in the group (upstream `HostIsNotConnectedError`), except for the
/// deprecated `all` sentinel which degrades to every enabled host.
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
pub fn per_host(command: &str, hosts: &[String]) -> mtui_hosts::Command {
    mtui_hosts::Command::PerHost(
        hosts
            .iter()
            .map(|h| (h.clone(), command.to_owned()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_types::enums::{ExecutionMode, TargetState};

    fn cmd() -> clap::Command {
        add_hosts_arg(clap::Command::new("t").no_binary_name(true))
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
        // Resolve the *current* user — always present in the password DB — so the
        // test is hermetic and makes no assumption about root/nobody.
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
                Target::with_connection(
                    *h,
                    *state,
                    ExecutionMode::Serial,
                    Box::new(MockConnection::new(*h)),
                )
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
