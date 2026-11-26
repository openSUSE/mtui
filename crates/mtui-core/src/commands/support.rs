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
use mtui_datasources::{HttpClient, IncidentKey, OpenQAError, QemDashboardClient};
use mtui_hosts::HostsGroup;
use mtui_types::{RequestReviewID, UpdateSource};

use crate::command::Command;
use crate::error::CommandError;
use crate::session::Session;

/// The wall-clock budget for remote work over an established link whose peer may
/// have vanished (`remove_host`'s close fan-out, `unlock --force`,
/// `reload_products`): [`HOST_CLOSE_TIMEOUT`](crate::session::HOST_CLOSE_TIMEOUT).
///
/// Concurrent fan-outs spend one budget for the whole call; serial work spends
/// one *per host*, or success would depend on fleet size.
#[cfg(not(test))]
pub(crate) fn host_op_budget() -> std::time::Duration {
    crate::session::HOST_CLOSE_TIMEOUT
}
#[cfg(test)]
pub(crate) fn host_op_budget() -> std::time::Duration {
    crate::commands::testkit::host_op_budget_override()
}

// The REPL steer/scope text for contended lines lives in `mtui_hosts::ContendedSurface::Repl`.

/// Resolves the dashboard identity of the loaded update for the commands that
/// query the dashboard directly.
///
/// `url_dashboard_qam` is the dashboard's base URL, without `/api`.
///
/// # Errors
///
/// [`CommandError::Other`] when the dashboard cannot say which incident this is.
pub(crate) async fn resolve_dashboard_key(
    http: &HttpClient,
    url_dashboard_qam: &str,
    number: &str,
    source: UpdateSource,
    pr_url: Option<&str>,
) -> Result<IncidentKey, CommandError> {
    let client = QemDashboardClient::with_client(
        http.clone(),
        format!("{}/api", url_dashboard_qam.trim_end_matches('/')),
    );
    client
        .resolve_key(number, source, pr_url)
        .await
        .ok_or_else(|| {
            CommandError::Other(format!(
                "QEM Dashboard query failed: could not identify incident {number} on the QEM Dashboard"
            ))
        })
}

/// A fresh, unpopulated [`KernelOpenQA`] connector for openQA instance `host`.
///
/// Resolves API credentials from the standard `client.conf`/`$OPENQA_CONFIG`
/// search path, keyed on the instance host; call [`KernelOpenQA::run`] to
/// populate it. Takes an already-built transport (from
/// [`Session::openqa_transport`](crate::session::Session::openqa_transport)) so
/// a per-host loop reuses one connection pool.
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

/// Builds the session-scoped HTTP client, so one pool serves every command.
///
/// # Errors
///
/// [`CommandError::Other`] when the client cannot be built.
pub(crate) fn session_http(session: &Session) -> Result<HttpClient, CommandError> {
    session
        .http_client()
        .map_err(|e| CommandError::Other(format!("could not build HTTP client: {e}")))
}

/// Builds the session-scoped openQA transport.
///
/// # Errors
///
/// [`CommandError::Other`] when the transport cannot be built.
pub(crate) fn session_openqa_transport(session: &Session) -> Result<reqwest::Client, CommandError> {
    session
        .openqa_transport()
        .map_err(|e| CommandError::Other(format!("could not build openQA transport: {e}")))
}

/// Maps an openQA client build failure to a user-facing command error.
pub(crate) fn openqa_client_err(e: OpenQAError) -> CommandError {
    CommandError::Other(format!("could not build openQA client: {e}"))
}

/// Maps a QEM Dashboard fetch failure to a user-facing command error.
pub(crate) fn dashboard_fetch_err(e: mtui_datasources::QemDashboardError) -> CommandError {
    CommandError::Other(format!(
        "could not fetch openQA data from QEM Dashboard: {e}"
    ))
}

/// Maps an openQA jobs fetch failure to a user-facing command error.
pub(crate) fn openqa_fetch_err(e: OpenQAError) -> CommandError {
    CommandError::Other(format!("could not fetch openQA data: {e}"))
}

/// The session values an openQA/dashboard refresh needs, snapshotted so no
/// `&Session` borrow (it is not `Sync`) crosses an `.await`.
pub(crate) struct OpenqaCtx {
    pub rrid: RequestReviewID,
    pub http: HttpClient,
    pub dashboard_api: String,
    pub openqa_instance: String,
    pub openqa_baremetal: String,
    pub source: UpdateSource,
    pub pr_url: Option<String>,
    pub max_parallel: usize,
}

impl OpenqaCtx {
    /// # Errors
    ///
    /// [`CommandError::Other`] when the HTTP client cannot be built.
    pub(crate) fn snapshot(session: &Session, rrid: RequestReviewID) -> Result<Self, CommandError> {
        Ok(Self {
            rrid,
            http: session_http(session)?,
            dashboard_api: session.config.qem_dashboard_api.clone(),
            openqa_instance: session.config.openqa_instance.clone(),
            openqa_baremetal: session.config.openqa_instance_baremetal.clone(),
            source: session.metadata().update_source(),
            pr_url: session.metadata().base().giteapr.clone(),
            max_parallel: session.config.max_parallel as usize,
        })
    }

    /// The report's [`QemIncident`], shared by `DashboardAutoOpenQA` and
    /// `KernelOpenQA`. `source` is the fallback when the dashboard has no record;
    /// `pr_url` names a git incident's project.
    pub(crate) async fn incident(&self) -> QemIncident {
        let client = QemDashboardClient::with_client(self.http.clone(), self.dashboard_api.clone());
        QemIncident::with_client(
            self.rrid.clone(),
            client,
            self.source,
            self.pr_url.as_deref(),
        )
        .await
    }

    /// A fresh, unpopulated [`DashboardAutoOpenQA`] for the auto workflow, its
    /// concurrent per-setting job fetches bounded by `max_parallel`. Call
    /// [`DashboardAutoOpenQA::run`] to populate it.
    pub(crate) fn auto(&self, incident: &QemIncident) -> DashboardAutoOpenQA {
        DashboardAutoOpenQA::new(
            self.openqa_instance.clone(),
            incident,
            self.rrid.clone(),
            self.max_parallel,
        )
    }
}

/// Fetches the kernel results of the primary and baremetal instances into
/// `out`, one instance at a time so a later failure leaves the earlier ones.
///
/// # Errors
///
/// [`CommandError::Other`] on the first build or fetch failure.
pub(crate) async fn fetch_kernel(
    ctx: &OpenqaCtx,
    incident: &QemIncident,
    transport: &reqwest::Client,
    out: &mut Vec<KernelOpenQA>,
) -> Result<(), CommandError> {
    for host in [&ctx.openqa_instance, &ctx.openqa_baremetal] {
        let oqa = build_kernel_openqa(incident, host, transport.clone())
            .map_err(openqa_fetch_err)?
            .run()
            .await
            .map_err(openqa_fetch_err)?;
        out.push(oqa);
    }
    Ok(())
}

/// Re-runs the kernel connectors in place. `KernelOpenQA::run` consumes `self`,
/// so this drains and rebuilds.
///
/// # Errors
///
/// [`CommandError::Other`] on the first failed fetch, leaving the holder empty.
pub(crate) async fn refresh_kernel(session: &mut Session) -> Result<(), CommandError> {
    let stale = std::mem::take(&mut session.metadata_mut().openqa_mut().kernel);
    let mut refreshed = Vec::with_capacity(stale.len());
    for oqa in stale {
        refreshed.push(oqa.run().await.map_err(openqa_fetch_err)?);
    }
    session.metadata_mut().openqa_mut().kernel = refreshed;
    Ok(())
}

/// Refreshes the report's "auto" openQA result in place. The holder starts
/// empty rather than being populated at load, so a `None` builds and runs a
/// fresh connector instead of panicking.
///
/// # Errors
///
/// [`CommandError::Other`] when the dashboard fetch fails.
pub(crate) async fn refresh_auto(
    session: &mut Session,
    ctx: &OpenqaCtx,
    incident: &QemIncident,
) -> Result<(), CommandError> {
    if let Some(auto) = session.metadata_mut().openqa_mut().auto.as_mut() {
        auto.run().await.map_err(dashboard_fetch_err)?;
    } else {
        let mut auto = ctx.auto(incident);
        auto.run().await.map_err(dashboard_fetch_err)?;
        session.metadata_mut().openqa_mut().auto = Some(auto);
    }
    Ok(())
}

/// Like [`require_update`], and also refuses a report naming no package
/// versions: `verb` would dispatch nothing and print a success an MCP client
/// cannot tell from a real one. Returns the package list.
///
/// # Errors
///
/// [`CommandError::Other`] when no report is loaded, or it names no packages.
pub(crate) fn require_packages(session: &Session, verb: &str) -> Result<Vec<String>, CommandError> {
    require_update(session)?;
    let packages = session.metadata().get_package_list();
    if packages.is_empty() {
        return Err(CommandError::Other(format!(
            "the loaded report names no package versions, so {verb} has nothing to \
             install; refusing to report success — check `list_packages -w` and the \
             report metadata"
        )));
    }
    Ok(packages)
}

/// Prints the OBS principal and signing-key lines `whoami` and `teregen_login`
/// open with.
pub(crate) fn print_identity(
    session: &mut Session,
    creds: &mtui_datasources::obs::oscrc::ObsCredentials,
) {
    let key = mtui_datasources::sshsig::key_display(
        creds.sshkey_path.as_deref(),
        creds.sshkey_fingerprint.as_deref(),
    );
    session
        .display
        .println(&format!("principal: {}", creds.user));
    session.display.println(&format!("signing key: {key}"));
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
    meta.is_loaded()
        .then(|| meta.rrid().cloned())
        .flatten()
        .ok_or_else(|| {
            CommandError::Other("Metadata not loaded, please use load_template first".to_owned())
        })
}

/// Guards a command body that reads or writes the report document.
///
/// # Errors
///
/// [`CommandError::NoDocument`] when the active report has no document.
pub(crate) fn require_document(session: &Session) -> Result<(), CommandError> {
    match session.metadata().base().document {
        Some(_) => Ok(()),
        None => Err(CommandError::NoDocument),
    }
}

/// Refuses a write (`commit`, `export`, `request_review`, and any recording of
/// a reviewer or marker onto the document) against a template that loaded with
/// a stale Gitea hash, unless `allow_stale` is set.
///
/// `load_template --force-continue` loads a stale template without repairing
/// the mismatch (`TestReportBase::stale_hash_warning` stays set), so without
/// this gate those writes could publish content known to disagree with the PR.
/// `approve` passes `allow_stale` once its own live `check_hash()` has passed.
///
/// **Gap, not a guarantee:** this reads a load-time snapshot; a report that
/// goes stale after loading passes silently.
pub(crate) fn stale_hash_gate(session: &Session, allow_stale: bool) -> Result<(), CommandError> {
    if allow_stale {
        return Ok(());
    }
    match &session.metadata().base().stale_hash_warning {
        Some(warning) => Err(CommandError::Other(format!(
            "{warning}; pass --allow-stale to proceed anyway"
        ))),
        None => Ok(()),
    }
}

/// Refuses, before any I/O, to drop `rrid`'s document edits that no `commit`
/// has uploaded, unless `discard_authored` is set.
///
/// A report that cannot be read (held by another dispatch) refuses too: a
/// contended read must never pass as clean.
pub(crate) fn document_edits_guard(
    session: &Session,
    rrid: &str,
    discard_authored: bool,
) -> Result<(), CommandError> {
    if discard_authored {
        return Ok(());
    }
    match session.document_dirty(rrid) {
        Some(false) => Ok(()),
        Some(true) => Err(CommandError::Other(format!(
            "{rrid} has document edits not yet committed; run `commit` first, \
             or pass --discard-authored"
        ))),
        None => Err(CommandError::Other(format!(
            "cannot check {rrid} for uncommitted document edits; retry"
        ))),
    }
}

/// Refuses `verb` (`approve`/`reject`), before any request, while `rrid`'s
/// document has edits no `commit` has uploaded: the OBS gate reads the server's
/// copy, so it would act on a verdict nobody else can see.
///
/// A report that cannot be read refuses too, as in [`document_edits_guard`].
pub(crate) fn uncommitted_report_guard(
    session: &Session,
    rrid: &str,
    verb: &str,
) -> Result<(), CommandError> {
    match session.document_dirty(rrid) {
        Some(false) => Ok(()),
        Some(true) => Err(CommandError::Other(format!(
            "{rrid} has document edits not yet committed, and {verb} reads the server's copy; \
             run `commit` first, or pass --reviewer NAME (who reviewed this report) to record \
             them and upload"
        ))),
        None => Err(CommandError::Other(format!(
            "cannot check {rrid} for uncommitted document edits; retry"
        ))),
    }
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

/// Connected host names starting with `text`.
#[must_use]
pub(crate) fn host_completion(session: &Session, text: &str) -> Vec<String> {
    session
        .targets()
        .names()
        .into_iter()
        .filter(|n| n.starts_with(text))
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

    // Drop any synonym group the already-typed tokens committed to.
    let mut tokens: Vec<String> = line.split(' ').map(str::to_owned).collect();
    if !tokens.is_empty() {
        tokens.remove(0);
    }
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].clone();
        let bytes = tok.as_bytes();
        if bytes.len() > 2 && bytes[0] == b'-' && bytes[1] != b'-' {
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
/// template RRIDs, and the connected host names, with `extra_flags` added as
/// synonym groups.
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

/// Flag spellings as synonym groups, one per Clap argument.
pub(crate) type CompletionFlags = Vec<Vec<String>>;

/// The spellings of the flags `ids` name in `command`'s parser, one synonym
/// group per ID in the order given (not Clap's declaration order): the short
/// spelling, the long one, then their visible aliases.
///
/// Built once per command (a `LazyLock` at the call site).
///
/// # Panics
///
/// When an ID is not an argument of the parser, or names one that has no
/// offered spelling (a positional, or a hidden flag).
#[must_use]
pub(crate) fn completion_flags(command: &dyn Command, ids: &[&str]) -> CompletionFlags {
    let parser = crate::engine::command_parser(command);
    ids.iter()
        .map(|id| {
            let arg = parser
                .get_arguments()
                .find(|a| a.get_id() == id)
                .unwrap_or_else(|| panic!("`{}` has no argument `{id}`", command.name()));
            let shorts = arg
                .get_short_and_visible_aliases()
                .unwrap_or_default()
                .into_iter()
                .map(|c| format!("-{c}"));
            let longs = arg
                .get_long_and_visible_aliases()
                .unwrap_or_default()
                .into_iter()
                .map(|l| format!("--{l}"));
            let group: Vec<String> = shorts.chain(longs).collect();
            assert!(
                !arg.is_hide_set() && !group.is_empty(),
                "`{}` argument `{id}` has no offered flag spelling",
                command.name()
            );
            group
        })
        .collect()
}

/// [`complete_choices`] over cached [`CompletionFlags`].
#[must_use]
pub(crate) fn complete_flag_groups(
    flags: &CompletionFlags,
    extra: Vec<String>,
    line: &str,
    text: &str,
) -> Vec<String> {
    let owned: Vec<Vec<&str>> = flags
        .iter()
        .map(|group| group.iter().map(String::as_str).collect())
        .collect();
    let groups: Vec<&[&str]> = owned.iter().map(Vec::as_slice).collect();
    complete_choices(&groups, extra, line, text)
}

/// [`complete_fanout`] over cached [`CompletionFlags`], which carry the
/// `-t/--target` and template groups themselves.
#[must_use]
pub(crate) fn complete_fanout_flags(
    session: &Session,
    flags: &CompletionFlags,
    line: &str,
    text: &str,
) -> Vec<String> {
    let mut extra = session.templates.rrids();
    extra.extend(session.targets().names());
    complete_flag_groups(flags, extra, line, text)
}

/// File-path variant of [`complete_choices`].
///
/// Directory entries under the directory part of `text` (basename-prefix
/// filtered, directories carrying a trailing `/`) merged with
/// [`complete_choices`]. An unreadable directory yields no candidates.
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
/// `~user`/`~user/…` → that user's home (getpwnam). An unknown user, no `$HOME`,
/// or a non-Unix target leaves `text` unexpanded.
fn expand_tilde(text: &str) -> String {
    let Some(rest) = text.strip_prefix('~') else {
        return text.to_owned();
    };

    if rest.is_empty() || rest.starts_with('/') {
        return match std::env::var_os("HOME") {
            Some(home) => format!("{}{rest}", home.to_string_lossy()),
            None => text.to_owned(),
        };
    }

    let (user, tail) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    resolve_user_home(user).map_or_else(|| text.to_owned(), |home| format!("{home}{tail}"))
}

/// The home directory of a named user via getpwnam, or `None` (always on
/// non-Unix).
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

/// Directory entries matching the basename prefix in `text`. Directories carry a
/// trailing `/`; a `~`/`~user` prefix expands; a bare prefix completes against
/// the current directory. An unreadable directory yields no candidates.
#[must_use]
pub(crate) fn complete_path(text: &str) -> Vec<String> {
    use std::path::Path;

    let expanded = expand_tilde(text);

    let (dir, prefix) = match expanded.rfind('/') {
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

/// The parsed `-t/--target` hostnames, or `None` when the flag was omitted
/// (callers read it as "all enabled hosts").
#[must_use]
pub(crate) fn hosts_arg(args: &ArgMatches) -> Option<Vec<String>> {
    args.try_get_many::<String>("hosts")
        .ok()
        .flatten()
        .map(|it| it.cloned().collect())
}

/// Whether the invocation named explicit `-t` hosts: the fan-out skip rule keys
/// on this, so a typo'd `-t` fails loudly.
#[must_use]
pub(crate) fn named_hosts(args: &ArgMatches) -> bool {
    hosts_arg(args).is_some_and(|v| !v.is_empty())
}

/// Whether `-t` selects particular hosts: omitted, or the deprecated `all`,
/// it means every host.
#[must_use]
pub(crate) fn selects_hosts(args: &ArgMatches) -> bool {
    hosts_arg(args).is_some_and(|hosts| !hosts.is_empty() && !hosts.iter().any(|h| h == "all"))
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
/// group and drops the unselected hosts. The caller drives the subset in place
/// via a [`Command::PerHost`](mtui_hosts::Command) map keyed on them.
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
        Some(hosts) if selects_hosts(args) => {
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

/// [`select_names`] with its error surfaced as a [`CommandError`].
///
/// # Errors
///
/// [`CommandError::Other`] carrying the [`HostError`](mtui_hosts::HostError)
/// text when a named host is not connected.
pub(crate) fn select_hosts(
    group: &HostsGroup,
    args: &ArgMatches,
    enabled: bool,
) -> Result<Vec<String>, CommandError> {
    select_names(group, args, enabled).map_err(|e| CommandError::Other(e.to_string()))
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
/// [`page_interactive`](crate::display::page_interactive) through the session's
/// [`Prompter`](mtui_hosts::Prompter); headless callers take
/// [`page`](crate::display::page), which forwards every line unpaged.
pub(crate) async fn page_output(session: &mut Session, output: &[String]) {
    if session.is_repl {
        let prompter = session.prompter().cloned();
        crate::display::page_interactive(output, &mut session.display, prompter.as_ref()).await;
    } else {
        let mut writer = |line: &str| session.display.println(line);
        crate::display::page(output, &mut writer);
    }
}

/// One `warning:` line per item this session left on a closed host, shared by
/// `remove_host`, `unload`, `quit` and a reload that replaces a template.
pub(crate) fn leftover_notices(
    leftovers: &std::collections::BTreeMap<String, mtui_hosts::HostLeftovers>,
) -> Vec<String> {
    use crate::display::sanitize_external;
    use mtui_hosts::CloseRelease;

    // A newline or tab in a caller-supplied name would forge a second line.
    let clean = |s: &str| sanitize_external(s, None).replace(['\n', '\t'], " ");
    let mut lines = Vec::new();
    for (host, left) in leftovers {
        let host = clean(host);
        let holds = [
            (
                left.operation_lock,
                "operation lock",
                "stale_age",
                "unlock -f",
            ),
            (
                left.pool_claim,
                "pool claim",
                "pool_stale_age",
                "unlock -f -p",
            ),
        ];
        for (why, what, age, unlock) in holds {
            let why = match why {
                None => continue,
                Some(CloseRelease::TakenOver) => {
                    lines.push(format!(
                        "warning: {host}: {what} now belongs to another owner, so the close left \
                         it in place; it is theirs to release"
                    ));
                    continue;
                }
                Some(CloseRelease::LinkDown) => "link down",
                Some(CloseRelease::Attempted) => "release failed",
                Some(CloseRelease::Unfinished) => "teardown abandoned",
            };
            lines.push(format!(
                "warning: {host}: {what} not released ({why}); unless the host reboots, it \
                 stays until it is older than [lock] {age} or someone runs `{unlock}`"
            ));
        }
        if left.repos_added {
            lines.push(format!(
                "warning: {host}: the update's repositories are still configured (this \
                 session's last repository change there was an add); with the update loaded, \
                 reconnect the host and run `set_repo --remove`"
            ));
        }
        if !left.installed.is_empty() {
            let packages: std::collections::BTreeSet<String> =
                left.installed.iter().map(|p| clean(p)).collect();
            let packages: Vec<String> = packages.into_iter().collect();
            let (them, some) = if packages.len() == 1 {
                ("it", "it may have been installed before")
            } else {
                ("them", "some may have been installed before")
            };
            lines.push(format!(
                "warning: {host}: `install` ran for {} and no later `uninstall` removed {them} \
                 ({some})",
                packages.join(", ")
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_hosts::{HostsGroup, MockConnection, Target};
    use mtui_types::enums::TargetState;

    fn cmd() -> clap::Command {
        add_hosts_arg(clap::Command::new("t").no_binary_name(true))
    }

    #[test]
    fn leftover_notices_names_an_abandoned_release_and_omits_clean_hosts() {
        use mtui_hosts::{CloseRelease, HostLeftovers};

        let leftovers = std::collections::BTreeMap::from([(
            "h1".to_owned(),
            HostLeftovers {
                operation_lock: Some(CloseRelease::Unfinished),
                ..Default::default()
            },
        )]);
        assert_eq!(
            leftover_notices(&leftovers),
            [
                "warning: h1: operation lock not released (teardown abandoned); unless the host \
              reboots, it stays until it is older than [lock] stale_age or someone runs \
              `unlock -f`"
            ]
        );
        let one = std::collections::BTreeMap::from([(
            "h1".to_owned(),
            HostLeftovers {
                installed: vec!["pkg-a".to_owned()],
                ..Default::default()
            },
        )]);
        assert_eq!(
            leftover_notices(&one),
            [
                "warning: h1: `install` ran for pkg-a and no later `uninstall` removed it (it \
                 may have been installed before)"
            ]
        );
        let clean = std::collections::BTreeMap::from([("h2".to_owned(), HostLeftovers::default())]);
        assert!(leftover_notices(&clean).is_empty());
    }

    #[test]
    fn leftover_notices_keep_each_notice_on_one_line() {
        use mtui_hosts::HostLeftovers;

        let leftovers = std::collections::BTreeMap::from([(
            "h\u{1b}[31m1\nwarning: forged".to_owned(),
            HostLeftovers {
                installed: vec!["pkg-a\nwarning: h2: forged".to_owned(), "pkg\tb".to_owned()],
                ..Default::default()
            },
        )]);
        assert_eq!(
            leftover_notices(&leftovers),
            [
                "warning: h1 warning: forged: `install` ran for pkg b, pkg-a warning: h2: forged \
                 and no later `uninstall` removed them (some may have been installed before)"
            ]
        );
    }

    #[test]
    fn leftover_notices_leave_a_taken_over_claim_to_its_owner() {
        use mtui_hosts::{CloseRelease, HostLeftovers};

        let leftovers = std::collections::BTreeMap::from([(
            "h1".to_owned(),
            HostLeftovers {
                pool_claim: Some(CloseRelease::TakenOver),
                ..Default::default()
            },
        )]);
        assert_eq!(
            leftover_notices(&leftovers),
            [
                "warning: h1: pool claim now belongs to another owner, so the close left it in \
                 place; it is theirs to release"
            ]
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

    struct Synth(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl Command for Synth {
        fn name(&self) -> &'static str {
            "synth"
        }
        fn scope(&self) -> crate::command::Scope {
            crate::command::Scope::Single
        }
        fn configure(&self, cmd: clap::Command) -> clap::Command {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            cmd.arg(
                Arg::new("alpha")
                    .short('a')
                    .long("alpha")
                    .visible_short_alias('A')
                    .visible_alias("first")
                    .alias("secret"),
            )
            .arg(Arg::new("beta").long("beta"))
            .arg(Arg::new("quiet").short('q').action(ArgAction::SetTrue))
            .arg(Arg::new("hidden").long("hidden").hide(true))
            .arg(Arg::new("pos"))
        }
        async fn call(&self, _: &mut Session, _: &ArgMatches) -> Result<(), CommandError> {
            Ok(())
        }
    }

    fn synth() -> Synth {
        Synth(std::sync::Arc::default())
    }

    #[test]
    fn completion_flags_group_visible_spellings_in_the_order_given() {
        assert_eq!(
            completion_flags(&synth(), &["beta", "alpha", "quiet"]),
            vec![
                vec!["--beta"],
                vec!["-a", "-A", "--alpha", "--first"],
                vec!["-q"],
            ]
        );
    }

    #[test]
    #[should_panic(expected = "has no argument `nope`")]
    fn completion_flags_rejects_an_unknown_id() {
        let _ = completion_flags(&synth(), &["nope"]);
    }

    #[test]
    #[should_panic(expected = "argument `hidden` has no offered flag spelling")]
    fn completion_flags_rejects_a_hidden_flag() {
        let _ = completion_flags(&synth(), &["hidden"]);
    }

    #[test]
    #[should_panic(expected = "argument `pos` has no offered flag spelling")]
    fn completion_flags_rejects_a_positional() {
        let _ = completion_flags(&synth(), &["pos"]);
    }

    #[test]
    fn complete_flag_groups_drops_a_whole_group_whichever_alias_was_typed() {
        let flags = completion_flags(&synth(), &["alpha", "beta"]);
        for line in ["synth --alpha ", "synth -A ", "synth --first "] {
            assert_eq!(complete_flag_groups(&flags, vec![], line, "-"), ["--beta"]);
        }
    }

    #[test]
    fn complete_flag_groups_expands_bundled_shorts_and_keeps_first_seen_order() {
        let flags = completion_flags(&synth(), &["alpha", "beta", "quiet"]);
        assert_eq!(
            complete_flag_groups(&flags, vec![], "synth -aq ", "-"),
            ["--beta"]
        );
        assert_eq!(
            complete_flag_groups(
                &flags,
                vec!["--beta".to_owned(), "h".to_owned()],
                "synth ",
                ""
            ),
            ["-a", "-A", "--alpha", "--first", "--beta", "-q", "h"]
        );
        assert_eq!(
            complete_flag_groups(&flags, vec!["-a".to_owned()], "synth ", "-a"),
            ["-a"]
        );
    }

    #[test]
    fn cached_flags_build_the_parser_once_however_often_completion_runs() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flags = std::sync::LazyLock::new(|| {
            completion_flags(&Synth(calls.clone()), &["alpha", "beta", "quiet"])
        });
        for _ in 0..5 {
            let _ = complete_flag_groups(&flags, vec![], "synth ", "-");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    fn selects_hosts_excludes_the_all_sentinel() {
        assert!(!selects_hosts(&parse(&[])));
        assert!(!selects_hosts(&parse(&["-t", "all"])));
        assert!(!selects_hosts(&parse(&["-t", "a", "-t", "all"])));
        assert!(selects_hosts(&parse(&["-t", "a"])));
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
    fn select_hosts_reports_an_unknown_host_as_other_with_the_host_error_text() {
        let g = group(&[("h1", TargetState::Enabled)]);
        let err = select_hosts(&g, &parse(&["-t", "ghost"]), true).unwrap_err();
        let expected = mtui_hosts::HostError::NotConnected {
            host: "ghost".to_owned(),
        }
        .to_string();
        assert!(
            matches!(&err, CommandError::Other(m) if *m == expected),
            "{err:?}"
        );
        assert_eq!(select_hosts(&g, &parse(&[]), true).unwrap(), vec!["h1"]);
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
    fn document_edits_guard_refuses_a_dirty_active_report_unless_discarded() {
        use crate::commands::testkit::session_with_hosts;
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert!(document_edits_guard(&session, "SUSE:Maintenance:1:1", false).is_ok());

        session.metadata_mut().base_mut().document_dirty = true;
        let err = document_edits_guard(&session, "SUSE:Maintenance:1:1", false).unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("--discard-authored")),
            "{err:?}"
        );
        assert!(document_edits_guard(&session, "SUSE:Maintenance:1:1", true).is_ok());
    }

    #[tokio::test]
    async fn document_edits_guard_refuses_a_report_it_cannot_read() {
        use crate::commands::testkit::{fake_report, session_with_hosts};
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        let handle = session.templates.handle("SUSE:Maintenance:2:2").unwrap();
        let _held = handle.lock().await;

        let err = document_edits_guard(&session, "SUSE:Maintenance:2:2", false).unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("retry")),
            "{err:?}"
        );
    }

    #[test]
    fn uncommitted_report_guard_names_the_commit_and_the_reviewer_path() {
        use crate::commands::testkit::session_with_hosts;
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        assert!(uncommitted_report_guard(&session, "SUSE:Maintenance:1:1", "reject").is_ok());

        session.metadata_mut().base_mut().document_dirty = true;
        let err = uncommitted_report_guard(&session, "SUSE:Maintenance:1:1", "reject").unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m)
                if m.contains("reject reads the server's copy")
                    && m.contains("run `commit`")
                    && m.contains("--reviewer NAME")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn uncommitted_report_guard_refuses_a_report_it_cannot_read() {
        use crate::commands::testkit::{fake_report, session_with_hosts};
        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        session
            .templates
            .add(fake_report("SUSE:Maintenance:2:2", &["h2"], "ok"));
        let handle = session.templates.handle("SUSE:Maintenance:2:2").unwrap();
        let _held = handle.lock().await;

        let err =
            uncommitted_report_guard(&session, "SUSE:Maintenance:2:2", "approve").unwrap_err();
        assert!(
            matches!(&err, CommandError::Other(m) if m.contains("retry")),
            "{err:?}"
        );
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
    fn host_completion_offers_connected_hosts_by_prefix() {
        use crate::commands::testkit::session_with_hosts;
        let (session, _buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["alpha", "alps", "beta"], "ok");
        let mut got = host_completion(&session, "al");
        got.sort();
        assert_eq!(got, vec!["alpha", "alps"]);
        assert_eq!(host_completion(&session, "beta"), vec!["beta"]);
        assert!(host_completion(&session, "nope").is_empty());
    }

    #[test]
    fn require_packages_refuses_an_unloaded_or_package_less_report_verbatim() {
        use crate::commands::testkit::{empty_session, session_with_hosts};
        let (session, _buf) = empty_session();
        assert!(matches!(
            require_packages(&session, "update"),
            Err(CommandError::Other(m)) if m == "Metadata not loaded, please use load_template first"
        ));

        let (mut session, _buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        let err = require_packages(&session, "prepare").unwrap_err();
        assert_eq!(
            err.to_string(),
            "the loaded report names no package versions, so prepare has nothing to \
             install; refusing to report success — check `list_packages -w` and the \
             report metadata"
        );
        crate::commands::testkit::seed_package(&mut session);
        assert!(!require_packages(&session, "prepare").unwrap().is_empty());
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
