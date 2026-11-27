//! The install/uninstall flows are shared by every concrete report and absent
//! from the null object; this pins both so a change to the shared flow cannot
//! pass on one report alone.

use std::collections::BTreeSet;

use mtui_config::options::Config;
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_testreport::{NullReport, ObsReport, PiReport, SlReport, TestReport};
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::system::{System, SystemProduct};

fn sles_group() -> (HostsGroup, MockConnection) {
    sles_group_on(MockConnection::new("h1").with_default(CommandLog::new("", "done", "", 0, 1)))
}

fn sles_group_on(conn: MockConnection) -> (HostsGroup, MockConnection) {
    let handle = conn.clone();
    let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
    t.set_system(
        System::new(
            SystemProduct::new("SLES", "15.5", "x86_64"),
            BTreeSet::new(),
            false,
        ),
        false,
    );
    (HostsGroup::new(vec![t], false), handle)
}

fn concrete_reports() -> Vec<(&'static str, Box<dyn TestReport + Sync>)> {
    vec![
        ("sl", Box::new(SlReport::new(Config::default()))),
        ("pi", Box::new(PiReport::new(Config::default()))),
        ("obs", Box::new(ObsReport::new(Config::default()))),
    ]
}

#[tokio::test]
async fn every_concrete_report_installs_through_the_shared_flow() {
    for (name, report) in concrete_reports() {
        let (mut group, handle) = sles_group();
        let res = report
            .perform_install(&mut group, &["pkg-a".to_owned()], true, &mut Vec::new())
            .await;
        assert!(res.is_ok(), "{name}: {res:?}");
        assert_eq!(
            handle.commands(),
            vec!["zypper -n in -y -l pkg-a".to_owned()],
            "{name}"
        );
    }
}

#[tokio::test]
async fn every_concrete_report_uninstalls_through_the_shared_flow() {
    for (name, report) in concrete_reports() {
        let (mut group, handle) = sles_group();
        let res = report
            .perform_uninstall(&mut group, &["pkg-a".to_owned()], true, &mut Vec::new())
            .await;
        assert!(res.is_ok(), "{name}: {res:?}");
        assert_eq!(
            handle.commands(),
            vec!["zypper -n rm pkg-a".to_owned()],
            "{name}"
        );
    }
}

#[tokio::test]
async fn the_null_report_runs_nothing_on_any_host() {
    let report = NullReport::new(Config::default());
    let (mut group, handle) = sles_group();
    let pkgs = ["pkg-a".to_owned()];
    let mut diag = Vec::new();

    report
        .perform_install(&mut group, &pkgs, false, &mut diag)
        .await
        .unwrap();
    report
        .perform_uninstall(&mut group, &pkgs, false, &mut diag)
        .await
        .unwrap();
    report
        .perform_prepare(&mut group, &pkgs, false, false, false, false, &mut diag)
        .await
        .unwrap();
    report
        .perform_downgrade(&mut group, &pkgs, false, &mut diag)
        .await
        .unwrap();
    report
        .perform_update(&mut group, false, false, &mut diag)
        .await
        .unwrap();

    assert!(handle.commands().is_empty(), "{:?}", handle.commands());
}

/// A SLES 15 host whose `probe` dry run removes `glibc`; every other command
/// answers a downgrade candidate for `pkg-a`.
fn group_refusing(probe: &str) -> (HostsGroup, MockConnection) {
    group_removing(probe, "glibc")
}

/// A SLES 15 host whose `probe` dry run removes `removed`.
fn group_removing(probe: &str, removed: &str) -> (HostsGroup, MockConnection) {
    let xml = format!(
        "<?xml version='1.0'?>\n<stream>\n<install-summary>\n<to-remove>\n\
         <solvable type=\"package\" name=\"{removed}\"/>\n</to-remove>\n\
         </install-summary>\n</stream>\n"
    );
    sles_group_on(
        MockConnection::new("h1")
            .with_default(CommandLog::new("", "pkg-a = 1.0-1\n", "", 0, 1))
            .with_response(probe, CommandLog::new(probe, xml, "", 0, 1)),
    )
}

#[derive(Debug, Clone, Copy)]
enum Guarded {
    Uninstall,
    Downgrade,
    PrepareForce,
}

#[tokio::test]
async fn every_concrete_report_refuses_a_protected_removal_through_the_shared_flow() {
    for (name, report) in concrete_reports() {
        for (path, probe) in [
            (Guarded::Uninstall, "zypper -x -n rm --dry-run pkg-a"),
            (
                Guarded::Downgrade,
                "zypper -x -n in --dry-run -C --force-resolution --oldpackage -y 'pkg-a=1.0-1'",
            ),
            (
                Guarded::PrepareForce,
                "zypper -x -n in --dry-run -y -l --force-resolution pkg-a",
            ),
        ] {
            let (mut group, handle) = group_refusing(probe);
            let pkgs = ["pkg-a".to_owned()];
            let mut diag = Vec::new();

            let res = match path {
                Guarded::Uninstall => {
                    report
                        .perform_uninstall(&mut group, &pkgs, false, &mut diag)
                        .await
                }
                Guarded::Downgrade => {
                    report
                        .perform_downgrade(&mut group, &pkgs, false, &mut diag)
                        .await
                }
                Guarded::PrepareForce => {
                    report
                        .perform_prepare(&mut group, &pkgs, true, false, false, false, &mut diag)
                        .await
                }
            };

            let err = res.expect_err("glibc would go");
            assert!(err.to_string().contains("glibc"), "{name} {path:?}: {err}");
            let commands = handle.commands();
            assert!(
                commands.iter().any(|c| c == probe),
                "{name} {path:?}: {commands:?}"
            );
            assert!(
                !commands.iter().any(|c| !c.contains("--dry-run")
                    && (c.contains("zypper -n rm")
                        || c.contains("--oldpackage")
                        || c.contains("zypper -n in"))),
                "{name} {path:?}: the transaction ran: {commands:?}"
            );
        }
    }
}

/// The report's own config carries the protected names to the guard, so no
/// caller of a `perform_*` flow can reach the dry-run judge without them.
#[tokio::test]
async fn every_concrete_report_refuses_a_package_its_config_protects() {
    let mut config = Config::default();
    config.target_protected_packages = vec!["netsvc".to_owned()];
    let reports: Vec<(&str, Box<dyn TestReport + Sync>)> = vec![
        ("sl", Box::new(SlReport::new(config.clone()))),
        ("pi", Box::new(PiReport::new(config.clone()))),
        ("obs", Box::new(ObsReport::new(config))),
    ];
    for (name, report) in reports {
        for (path, probe) in [
            ("install", "zypper -x -n in --dry-run -y -l pkg-a"),
            ("uninstall", "zypper -x -n rm --dry-run pkg-a"),
            (
                "downgrade",
                "zypper -x -n in --dry-run -C --force-resolution --oldpackage -y 'pkg-a=1.0-1'",
            ),
            (
                "prepare --force",
                "zypper -x -n in --dry-run -y -l --force-resolution pkg-a",
            ),
        ] {
            let (mut group, _handle) = group_removing(probe, "netsvc");
            let pkgs = ["pkg-a".to_owned()];
            let mut diag = Vec::new();
            let res = match path {
                "install" => {
                    report
                        .perform_install(&mut group, &pkgs, false, &mut diag)
                        .await
                }
                "uninstall" => {
                    report
                        .perform_uninstall(&mut group, &pkgs, false, &mut diag)
                        .await
                }
                "downgrade" => {
                    report
                        .perform_downgrade(&mut group, &pkgs, false, &mut diag)
                        .await
                }
                _ => {
                    report
                        .perform_prepare(&mut group, &pkgs, true, false, false, false, &mut diag)
                        .await
                }
            };
            let err = res.expect_err("netsvc would go");
            assert!(
                err.to_string().contains("protected package(s) netsvc ("),
                "{name} {path}: {err}"
            );
        }
    }
}
