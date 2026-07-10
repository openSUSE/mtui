//! The `list_packages` command.

use std::cmp::Ordering;

use async_trait::async_trait;
use clap::{Arg, ArgAction, ArgMatches};
use mtui_hosts::PackageQuerier;

use super::support::{add_hosts_arg, select_names};
use crate::command::{Command, Scope};
use crate::error::{CommandError, CommandResult};
use crate::session::Session;

/// Lists packages and their installed versions on the reference hosts.
///
/// Ports upstream `mtui.commands.listpackages.ListPackages`. For each selected
/// host it queries the installed version of every package (the report's package
/// list plus any `-p/--package` extras) and prints a colored state versus the
/// version the report requires:
/// * blue "not installed" — package absent,
/// * yellow "update needed" — installed older than required,
/// * green "updated" — installed at or above required,
/// * red "too recent" — installed newer than required.
///
/// `-w/--wanted` instead prints the versions the report wants, without touching
/// any host.
pub struct ListPackages;

/// The rendered state of a package on a host.
///
/// Distinguishes three outcomes upstream keeps separate but which a bare
/// `Option<Ordering>` cannot express:
/// * [`PkgState::Blank`] — installed but nothing to compare against (no template
///   loaded); upstream renders an empty state column.
/// * [`PkgState::NotInstalled`] — absent, or present in `-p` but not in the
///   report (upstream's `KeyError`/`None` branch).
/// * [`PkgState::Cmp`] — installed and comparable to a required version.
#[derive(Clone, Copy)]
enum PkgState {
    Blank,
    NotInstalled,
    Cmp(Ordering),
}

/// Maps a [`PkgState`] to the upstream colored state label.
fn state_label(display: &crate::display::CommandPromptDisplay, state: PkgState) -> String {
    match state {
        PkgState::Blank => String::new(),
        PkgState::NotInstalled => display.blue("not installed"),
        PkgState::Cmp(Ordering::Less) => display.yellow("update needed"),
        PkgState::Cmp(Ordering::Equal) => display.green("updated"),
        PkgState::Cmp(Ordering::Greater) => display.red("too recent"),
    }
}

#[async_trait]
impl Command for ListPackages {
    fn name(&self) -> &'static str {
        "list_packages"
    }

    fn about(&self) -> Option<&'static str> {
        Some("Lists packages and their installed versions on the reference hosts.")
    }

    fn scope(&self) -> Scope {
        Scope::Fanout
    }

    fn configure(&self, cmd: clap::Command) -> clap::Command {
        add_hosts_arg(cmd)
            .arg(
                Arg::new("package")
                    .short('p')
                    .long("package")
                    .action(ArgAction::Append)
                    .value_name("PACKAGE")
                    .help("package name to list (repeatable)"),
            )
            .arg(
                Arg::new("wanted")
                    .short('w')
                    .long("wanted")
                    .action(ArgAction::SetTrue)
                    .help("print versions wanted by the testreport"),
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
        if args.get_flag("wanted") {
            return self.run_wanted(session);
        }

        let extra: Vec<String> = args
            .try_get_many::<String>("package")
            .ok()
            .flatten()
            .map(|it| it.cloned().collect())
            .unwrap_or_default();

        let mut pkgs = session.metadata().get_package_list();
        pkgs.extend(extra);
        if pkgs.is_empty() {
            return Err(CommandError::MissingPackages);
        }

        // Whether a template is loaded decides how state is labeled (upstream
        // `if self.metadata:`). Read it before the mutable-targets borrow below.
        let loaded = session.metadata().is_loaded();

        let targets = session.targets_mut();
        let hosts =
            select_names(targets, args, true).map_err(|e| CommandError::Other(e.to_string()))?;
        if hosts.is_empty() {
            return Err(CommandError::NoRefhostsDefined);
        }

        // Query every selected host concurrently (upstream fans this out; a
        // serial `.await` per host turned an 11-host group into 11 sequential
        // SSH round-trips). `values_mut()` hands back disjoint `&mut Target`, so
        // each host's `rpm -q` is an independent future driven together by
        // `join_all`; the per-host render then reads the snapshotted result.
        let selected: std::collections::HashSet<&str> = hosts.iter().map(String::as_str).collect();
        let queries = targets
            .targets_mut()
            .filter(|t| selected.contains(t.hostname()))
            .map(|t| {
                let name = t.hostname().to_owned();
                let system = t.system().to_string();
                let pkgs = &pkgs;
                async move {
                    let versions = PackageQuerier::new(t).versions(pkgs).await;
                    let mut lines: Vec<(String, String, PkgState)> = Vec::new();
                    for pkg in pkgs {
                        let current = versions.get(pkg).cloned().flatten();
                        let wanted = t
                            .packages()
                            .iter()
                            .find(|p| &p.name == pkg)
                            .and_then(|p| p.required().cloned());
                        // Mirror upstream's three branches:
                        // * no template: installed -> blank, absent -> not installed.
                        // * template + required present: compare current vs required.
                        // * template but package not in report: not installed.
                        let state = match (&current, &wanted) {
                            (None, _) => PkgState::NotInstalled,
                            (Some(_), _) if !loaded => PkgState::Blank,
                            (Some(c), Some(w)) => PkgState::Cmp(c.cmp(w)),
                            (Some(_), None) => PkgState::NotInstalled,
                        };
                        let version = current.map_or_else(String::new, |v| v.to_string());
                        lines.push((pkg.clone(), version, state));
                    }
                    (name, system, lines)
                }
            })
            .collect::<Vec<_>>();
        let mut rendered: Vec<(String, String, Vec<(String, String, PkgState)>)> =
            futures::future::join_all(queries).await;
        // `values_mut()` yields sorted-by-hostname order; restore the caller's
        // requested host order so `-t a,b` renders in the order given.
        rendered
            .sort_by_key(|(name, _, _)| hosts.iter().position(|h| h == name).unwrap_or(usize::MAX));

        for (name, system, lines) in rendered {
            session
                .display
                .println(&format!("packages on {name} ({system}):"));
            for (pkg, version, state) in lines {
                let state = state_label(&session.display, state);
                session
                    .display
                    .println(&format!("{pkg:30}: {version:20} {state}"));
            }
            session.display.println("");
        }
        Ok(())
    }
}

impl ListPackages {
    /// The `-w/--wanted` path: print the versions the report wants (upstream
    /// `_run_just_wanted`), grouped by product, without touching any host.
    fn run_wanted(&self, session: &mut Session) -> CommandResult {
        let packages = session.metadata().base().packages.clone();
        let mut products: Vec<&String> = packages.keys().collect();
        products.sort();
        for product in products {
            session
                .display
                .println(&format!("Packages for version {product}:"));
            let mut names: Vec<&String> = packages[product].keys().collect();
            names.sort();
            for name in names {
                let version = &packages[product][name];
                session
                    .display
                    .println(&format!("{name:30}: {version:20} "));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testkit::{
        empty_session, matches, session_host_no_template, session_with_hosts,
    };

    #[test]
    fn name_and_fanout_scope() {
        assert_eq!(ListPackages.name(), "list_packages");
        assert_eq!(ListPackages.scope(), Scope::Fanout);
    }

    #[tokio::test]
    async fn loaded_template_extra_not_in_report_is_not_installed() {
        // Template loaded (FakeReport) but `-p bash` is not in the report, so it
        // has no required version: upstream's KeyError branch → "not installed",
        // NOT "updated" (case 3).
        let (mut session, buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "bash 5.1-1\n");
        assert!(session.metadata().is_loaded());
        let args = matches(&ListPackages, &["-p", "bash", "-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("packages on h1"), "{out}");
        assert!(out.contains("bash"), "{out}");
        assert!(out.contains("not installed"), "{out}");
        assert!(!out.contains("updated"), "{out}");
    }

    #[tokio::test]
    async fn no_template_installed_is_blank_state() {
        // No template loaded: an installed package has no required version to
        // compare against, so the state column is blank — never "updated",
        // never "not installed" (case 1).
        let (mut session, buf) = session_host_no_template(&["h1"], "bash 5.1-1\n");
        assert!(!session.metadata().is_loaded());
        let args = matches(&ListPackages, &["-p", "bash", "-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("bash"), "{out}");
        assert!(out.contains("5.1-1"), "{out}");
        assert!(!out.contains("updated"), "{out}");
        assert!(!out.contains("not installed"), "{out}");
        // Lock the exact no-template line: version, then a blank state column
        // (trailing whitespace where the colored word would be).
        assert!(
            out.lines().any(|l| l.starts_with("bash")
                && l.contains("5.1-1")
                && l.trim_end() == format!("{:30}: {:20}", "bash", "5.1-1").trim_end()),
            "{out}"
        );
    }

    #[tokio::test]
    async fn no_template_absent_is_not_installed() {
        // No template loaded + absent package → "not installed" (case 1, absent).
        let (mut session, buf) =
            session_host_no_template(&["h1"], "package ghostpkg is not installed\n");
        let args = matches(&ListPackages, &["-p", "ghostpkg", "-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("not installed"), "{out}");
        assert!(!out.contains("updated"), "{out}");
    }

    #[tokio::test]
    async fn not_installed_is_blue_state() {
        let (mut session, buf) = session_with_hosts(
            "SUSE:Maintenance:1:1",
            &["h1"],
            "package ghostpkg is not installed\n",
        );
        let args = matches(&ListPackages, &["-p", "ghostpkg", "-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("not installed"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn no_packages_errors() {
        let (mut session, _buf) = empty_session();
        let args = matches(&ListPackages, &[]);
        let err = ListPackages.call(&mut session, &args).await.unwrap_err();
        assert!(matches!(err, CommandError::MissingPackages));
        assert_eq!(
            err.to_string(),
            "Missing packages: TestReport not loaded and no -p given."
        );
    }

    #[tokio::test]
    async fn update_needed_when_installed_older_than_required() {
        use mtui_types::package::Package;
        let (mut session, buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "bash 5.0-1\n");
        // The report requires a newer version than what is installed.
        {
            let t = session.targets_mut().get_mut("h1").unwrap();
            let mut pkg = Package::new("bash");
            pkg.set_required(Some("5.1-1")).unwrap();
            t.set_packages(vec![pkg]);
        }
        let args = matches(&ListPackages, &["-p", "bash", "-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        assert!(
            buf.contents().contains("update needed"),
            "{}",
            buf.contents()
        );
    }

    #[tokio::test]
    async fn too_recent_when_installed_newer_than_required() {
        use mtui_types::package::Package;
        let (mut session, buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "bash 6.0-1\n");
        {
            let t = session.targets_mut().get_mut("h1").unwrap();
            let mut pkg = Package::new("bash");
            pkg.set_required(Some("5.1-1")).unwrap();
            t.set_packages(vec![pkg]);
        }
        let args = matches(&ListPackages, &["-p", "bash", "-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        assert!(buf.contents().contains("too recent"), "{}", buf.contents());
    }

    #[tokio::test]
    async fn seeded_report_package_installed_is_not_labeled_not_installed() {
        // Regression for the reported bug: a report package that is *installed*
        // (rpm -q returns a version) and seeded with its required version must
        // render a comparison state — never "not installed" while a version is
        // shown. Before seeding was wired, `wanted` was always None and this
        // fell into the (Some, None) => NotInstalled arm.
        use std::collections::HashMap;

        use mtui_types::package::Package;
        let (mut session, buf) = session_with_hosts(
            "SUSE:Maintenance:44759:413589",
            &["h1"],
            "hplip 3.26.4-150600.4.9.1\n",
        );
        {
            // Populate the report's package metadata so `get_package_list()`
            // surfaces hplip (mirrors a loaded report), and seed the target's
            // tracked package with its required version (what
            // `connect_and_add_hosts` now does in production).
            let base = session.metadata_mut().base_mut();
            let mut per_product = HashMap::new();
            per_product.insert("hplip".to_owned(), "3.26.4-150600.4.12.1".to_owned());
            base.packages.insert("15-SP6".to_owned(), per_product);

            let t = session.targets_mut().get_mut("h1").unwrap();
            let mut pkg = Package::new("hplip");
            pkg.set_required(Some("3.26.4-150600.4.12.1")).unwrap();
            t.set_packages(vec![pkg]);
        }
        let args = matches(&ListPackages, &["-t", "h1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        // Installed older than required → "update needed", and crucially the
        // installed line must NOT say "not installed".
        assert!(out.contains("3.26.4-150600.4.9.1"), "version shown: {out}");
        assert!(out.contains("update needed"), "{out}");
        assert!(
            !out.lines()
                .any(|l| l.contains("3.26.4-150600.4.9.1") && l.contains("not installed")),
            "installed package must never be labeled 'not installed': {out}"
        );
    }

    #[tokio::test]
    async fn wanted_prints_report_versions_without_hosts() {
        use std::collections::HashMap;
        let (mut session, buf) = session_with_hosts("SUSE:Maintenance:1:1", &["h1"], "ok");
        {
            let base = session.templates.active_mut().base_mut();
            let mut per_product = HashMap::new();
            per_product.insert("bash".to_owned(), "5.1-1".to_owned());
            base.packages.insert("SLES-15.5".to_owned(), per_product);
        }
        let args = matches(&ListPackages, &["-w"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        assert!(out.contains("Packages for version SLES-15.5:"), "{out}");
        assert!(out.contains("bash"), "{out}");
        assert!(out.contains("5.1-1"), "{out}");
    }

    #[tokio::test]
    async fn multi_host_output_follows_requested_order_not_sorted() {
        // The parallel query iterates `values_mut()` (hostname-sorted); the
        // render must restore the caller's `-t` order. Request z8 before a1 and
        // assert z8's block prints first.
        let (mut session, buf) =
            session_with_hosts("SUSE:Maintenance:1:1", &["a1", "z8"], "bash 5.1-1\n");
        let args = matches(&ListPackages, &["-p", "bash", "-t", "z8", "-t", "a1"]);
        ListPackages.call(&mut session, &args).await.unwrap();
        let out = buf.contents();
        let z_pos = out.find("packages on z8").expect("z8 block present");
        let a_pos = out.find("packages on a1").expect("a1 block present");
        assert!(
            z_pos < a_pos,
            "requested order z8 before a1 must be kept:\n{out}"
        );
    }
}
