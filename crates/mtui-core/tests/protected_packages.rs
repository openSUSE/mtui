//! `[target] protected_packages` reaches the removal guard of every guarded
//! command through the real dispatch path: a config file names a package, and
//! `install`, `uninstall`, `downgrade` and `prepare --force` each refuse a transaction
//! whose dry run removes it — and still refuse a built-in protected one.

mod support;

use std::collections::{BTreeSet, HashMap};

use mtui_config::Config;
use mtui_core::display::{ColorMode, CommandPromptDisplay};
use mtui_core::{Session, dispatch_line, register_all};
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_testreport::{SlReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::system::{System, SystemProduct};
use support::Buffer;

const RRID: &str = "SUSE:Maintenance:1:1";

/// Each guarded command line (`downgrade` and `prepare` act on the update's
/// `pkg-a`), its dry run, and a substring only its committed
/// transaction carries.
const GUARDED: [(&str, &str, &str); 4] = [
    (
        "install pkg-a",
        "zypper -x -n in --dry-run -y -l pkg-a",
        "zypper -n in -y -l pkg-a",
    ),
    (
        "uninstall pkg-a",
        "zypper -x -n rm --dry-run pkg-a",
        "zypper -n rm pkg-a",
    ),
    (
        "downgrade",
        "zypper -x -n in --dry-run -C --force-resolution --oldpackage -y 'pkg-a=1.0-1'",
        "--oldpackage",
    ),
    (
        "prepare --force",
        "zypper -x -n in --dry-run -y -l --force-resolution pkg-a",
        "zypper -n in -y -l --force-resolution pkg-a",
    ),
];

/// The config a file with one configured name and one malformed entry loads to.
fn config_from_file() -> Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mtui.toml");
    std::fs::write(
        &path,
        "[target]\nprotected_packages = [\" netsvc \", 3, \"bad name\"]\n",
    )
    .unwrap();
    let config = Config::load(Some(path));
    assert_eq!(config.target_protected_packages, vec!["netsvc".to_owned()]);
    config
}

/// A complete `zypper -x` dry-run stream whose summary removes `names`.
fn dry_run_removing(names: &[&str]) -> String {
    let solvables: String = names
        .iter()
        .map(|n| format!("<solvable type=\"package\" name=\"{n}\"/>\n"))
        .collect();
    format!(
        "<?xml version='1.0'?>\n<stream>\n<install-summary>\n<to-remove>\n{solvables}\
         </to-remove>\n</install-summary>\n</stream>\n"
    )
}

/// A session with a loaded SL report over one SLES 15 host whose `probe` dry
/// run removes `removed`; every other command answers a downgrade candidate.
fn session(
    config: Config,
    buf: &Buffer,
    probe: &str,
    removed: &[&str],
) -> (Session, MockConnection) {
    let conn = MockConnection::new("h1")
        .with_default(CommandLog::new("", "pkg-a = 1.0-1\n", "", 0, 1))
        .with_response(
            probe,
            CommandLog::new(probe, dry_run_removing(removed), "", 0, 1),
        );
    let handle = conn.clone();
    let mut target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
    target.set_system(
        System::new(
            SystemProduct::new("SLES", "15.5", "x86_64"),
            BTreeSet::new(),
            false,
        ),
        false,
    );
    let mut report = SlReport::new(config.clone());
    report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
    report.base_mut().targets = HostsGroup::new(vec![target], false);
    report.base_mut().packages.insert(
        "standard".to_owned(),
        HashMap::from([("pkg-a".to_owned(), "1.0".to_owned())]),
    );
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    let mut session = Session::with_display(config, false, display);
    session.templates.add(Box::new(report));
    (session, handle)
}

/// Dispatches `line` and returns its error text (empty on success) and the
/// commands the host saw.
async fn run(config: Config, line: &str, probe: &str, removed: &[&str]) -> (String, Vec<String>) {
    let buf = Buffer::default();
    let (mut session, handle) = session(config, &buf, probe, removed);
    let err = dispatch_line(&register_all(), &mut session, line)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    (err, handle.commands())
}

#[tokio::test]
async fn every_guarded_command_refuses_a_configured_package() {
    let config = config_from_file();
    for (line, probe, commit) in GUARDED {
        let (err, commands) = run(config.clone(), line, probe, &["pkg-a", "netsvc"]).await;
        assert!(
            err.contains("protected package(s) netsvc ("),
            "{line}: {err}"
        );
        assert!(commands.iter().any(|c| c == probe), "{line}: {commands:?}");
        assert!(
            !commands
                .iter()
                .any(|c| c.contains(commit) && !c.contains("--dry-run")),
            "{line}: the transaction ran: {commands:?}"
        );
    }
}

/// Catches a configured list that replaces the built-in one instead of
/// extending it.
#[tokio::test]
async fn a_configured_list_extends_rather_than_replaces_the_built_ins() {
    let config = config_from_file();
    for (line, probe, commit) in GUARDED {
        let (err, commands) = run(config.clone(), line, probe, &["pkg-a", "glibc"]).await;
        assert!(
            err.contains("protected package(s) glibc ("),
            "{line}: {err}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| c.contains(commit) && !c.contains("--dry-run")),
            "{line}: the transaction ran: {commands:?}"
        );
    }
}

/// The control: the same fixture without a protected removal commits, so the
/// refusals above are the guard's and not the fixture's.
#[tokio::test]
async fn an_unconfigured_package_is_not_refused() {
    let config = config_from_file();
    for (line, probe, commit) in GUARDED {
        let (err, commands) = run(config.clone(), line, probe, &["pkg-a", "libbar"]).await;
        assert!(!err.contains("protected package"), "{line}: {err}");
        assert!(
            commands
                .iter()
                .any(|c| c.contains(commit) && !c.contains("--dry-run")),
            "{line}: the transaction did not run: {commands:?}"
        );
    }
}
