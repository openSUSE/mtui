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
