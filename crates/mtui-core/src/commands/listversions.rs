//! The `list_versions` command.

use std::collections::BTreeMap;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_types::rpmver::RPMVersion;
use mtui_types::system::System;

use super::support::{add_hosts_arg, per_host, select_names};
use crate::command::{Command, Scope};
use crate::display::VersionGroup;
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Builds the upstream zypper-search query for `packages` (verbatim so remote
/// output matches upstream's parser).
fn versions_query(packages: &[String]) -> String {
    format!(
        "for p in {}; do zypper -n search -s --match-exact -t package $p; done \
         | grep -e ^[iv] | awk -F '|' '{{ print $2 $4 }}' | sort -u",
        packages.join(" ")
    )
}

/// Parses one `name version` output line into `(package, version)` (upstream
/// `re_ver = (\S+)\s+(\S+)`).
fn parse_version_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(pkg), Some(ver)) => Some((pkg.to_owned(), ver.to_owned())),
        _ => None,
    }
}

/// Lists the available versions of packages in the enabled repositories.
///
/// Ports upstream `mtui.commands.simplelists.ListVersions` +
/// `TestReport.list_versions`. It runs `zypper search -s` per host, parses the
/// `name version` lines, then aggregates hosts that share the same version set
/// for a package into groups so the display renders each version ladder once per
/// host-group (upstream's `by_hosts_pkg` grouping). Packages default to the
/// report's package list when none are given via `-p/--package`.
pub struct ListVersions;

#[async_trait]
impl Command for ListVersions {
    fn name(&self) -> &'static str {
        "list_versions"
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd).arg(
            Arg::new("package")
                .short('p')
                .long("package")
                .action(ArgAction::Append)
                .value_name("PACKAGE")
                .help("package name to show versions for (repeatable)"),
        )
    }

    fn complete(&self, session: &Session, text: &str, _line: &str) -> Vec<String> {
        session
            .targets()
            .names()
            .into_iter()
            .filter(|n| n.starts_with(text))
            .collect()
    }

    async fn call(&self, session: &mut Session, args: &ArgMatches) -> CommandResult {
        let mut packages: Vec<String> = args
            .try_get_many::<String>("package")
            .ok()
            .flatten()
            .map(|it| it.cloned().collect())
            .unwrap_or_default();
        if packages.is_empty() {
            packages = session.metadata().get_package_list();
        }
        if packages.is_empty() {
            return Err(CommandError::Other("no packages to query".to_owned()));
        }

        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }
        targets
            .run(per_host(&versions_query(&packages), &hosts))
            .await;

        // by_host_pkg[host][pkg] = [version strings] (preserving output order).
        let mut by_host_pkg: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
        let mut systems: BTreeMap<String, System> = BTreeMap::new();
        for name in &hosts {
            let Some(t) = targets.get(name) else { continue };
            systems.insert(name.clone(), t.system().clone());
            let entry = by_host_pkg.entry(name.clone()).or_default();
            for line in t.lastout().split('\n') {
                if let Some((pkg, ver)) = parse_version_line(line) {
                    entry.entry(pkg).or_default().push(ver);
                }
            }
        }

        // by_pkg_vers[pkg][version-tuple] = [hosts]; then group by host-set.
        let mut by_pkg_vers: BTreeMap<String, BTreeMap<Vec<String>, Vec<String>>> = BTreeMap::new();
        for (host, pvs) in &by_host_pkg {
            for (pkg, vs) in pvs {
                by_pkg_vers
                    .entry(pkg.clone())
                    .or_default()
                    .entry(vs.clone())
                    .or_default()
                    .push(host.clone());
            }
        }

        // by_hosts_pkg[host-set] = [(pkg, versions)].
        let mut by_hosts_pkg: BTreeMap<Vec<String>, Vec<(String, Vec<String>)>> = BTreeMap::new();
        for (pkg, vshs) in &by_pkg_vers {
            for (vs, hs) in vshs {
                by_hosts_pkg
                    .entry(hs.clone())
                    .or_default()
                    .push((pkg.clone(), vs.clone()));
            }
        }

        let groups: Vec<VersionGroup> = by_hosts_pkg
            .into_iter()
            .map(|(hs, pvs)| {
                let host_systems = hs
                    .iter()
                    .filter_map(|h| systems.get(h).map(|s| (h.clone(), s.clone())))
                    .collect();
                let pkg_versions = pvs
                    .into_iter()
                    .map(|(pkg, vers)| {
                        let parsed = vers
                            .iter()
                            .filter_map(|v| RPMVersion::parse(v).ok())
                            .collect();
                        (pkg, parsed)
                    })
                    .collect();
                (host_systems, pkg_versions)
            })
            .collect();

        session.display.list_versions(&groups);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{empty_session, matches, session_with_hosts};

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListVersions.name(), "list_versions");
        assert_eq!(ListVersions.scope(), Scope::Fanout);
    }

    #[test]
    fn parse_version_line_extracts_pair() {
        assert_eq!(
            parse_version_line("bash 5.1-1"),
            Some(("bash".to_owned(), "5.1-1".to_owned()))
        );
        assert_eq!(parse_version_line(""), None);
    }

    #[tokio::test]
    async fn renders_version_ladder() {
        let (mut session, buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "bash 5.1-1\n");
        let args = matches(&ListVersions, &["-p", "bash", "-t", "h1"]);
        ListVersions.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("bash:"), "{out}");
        assert!(out.contains("5.1-1"), "{out}");
    }

    #[tokio::test]
    async fn no_packages_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ListVersions, &[]);
        let err = ListVersions.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }
}
