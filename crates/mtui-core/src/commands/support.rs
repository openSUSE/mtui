//! Shared helpers for command bodies.
//!
//! Ports the two cross-cutting `_command.py` helpers every host-phase command
//! reuses: the `-t/--target` argument (`_add_hosts_arg`) and the host selection
//! it drives (`parse_hosts`).

use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::HostsGroup;

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
