//! Integration test for `impl OperationGroup for HostsGroup` — the
//! composition-root binding that drives the install/uninstall
//! [`Operation`](mtui_hosts::Operation) template against a real
//! [`HostsGroup`](mtui_hosts::HostsGroup) via an injected
//! [`PlanProvider`](mtui_hosts::PlanProvider).
//!
//! Upstream's `test_operation.py` drives the template against a fully-mocked
//! group; here we drive it against the real group so the binding itself
//! (plans → run → check → reboot → last_output) is exercised, with hosts backed
//! by [`MockConnection`] so it stays offline and fast.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use mtui_hosts::{
    Check, CheckArgs, Doer, HostError, HostsGroup, InstallOperation, Operation, OperationGroup,
    PlanProvider, Target, UninstallOperation,
};
use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::hostlog::CommandLog;
use mtui_types::system::{System, SystemProduct};

/// A [`PlanProvider`] that returns a fixed doer and records every
/// `(role, release, transactional)` lookup, so tests can assert the key
/// derivation. A configured `missing` release surfaces the role's error.
struct RecordingProvider {
    lookups: Lookups,
    checked: Checked,
}

/// Shared log of `(role, release, transactional)` provider lookups.
type Lookups = Arc<Mutex<Vec<(String, String, bool)>>>;
/// Shared log of hostnames whose check fired.
type Checked = Arc<Mutex<Vec<String>>>;

impl RecordingProvider {
    fn new() -> (Self, Lookups, Checked) {
        let lookups: Lookups = Arc::new(Mutex::new(Vec::new()));
        let checked: Checked = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                lookups: lookups.clone(),
                checked: checked.clone(),
            },
            lookups,
            checked,
        )
    }
}

impl PlanProvider for RecordingProvider {
    fn doer(&self, role: &str, release: &str, transactional: bool) -> Result<Doer, HostError> {
        self.lookups
            .lock()
            .unwrap()
            .push((role.to_owned(), release.to_owned(), transactional));
        Ok(Doer::new(
            "zypper -n in -y -l $packages",
            "systemctl reboot",
        ))
    }

    fn check(&self, _role: &str, _release: &str, _transactional: bool) -> Check {
        let checked = self.checked.clone();
        Box::new(move |a: CheckArgs<'_>| {
            checked.lock().unwrap().push(a.hostname.to_owned());
        })
    }
}

/// A [`MockConnection`]-backed target with a parsed base product so
/// `get_release()` resolves, and a settable transactional flag. Returns the
/// target alongside a handle to the mock so callers can assert issued commands.
fn target(
    hostname: &str,
    base_name: &str,
    version: &str,
    transactional: bool,
) -> (Target, mtui_hosts::MockConnection) {
    let conn = mtui_hosts::MockConnection::new(hostname)
        .with_default(CommandLog::new("", "done", "", 0, 1));
    let handle = conn.clone();
    let mut t = Target::with_connection(
        hostname,
        TargetState::Enabled,
        ExecutionMode::Parallel,
        Box::new(conn),
    );
    let system = System::new(
        SystemProduct::new(base_name, version, "x86_64"),
        BTreeSet::new(),
        false,
    );
    t.set_system(system, transactional);
    (t, handle)
}

#[tokio::test]
async fn install_drives_doer_and_reboots_only_transactional() {
    let (provider, lookups, checked) = RecordingProvider::new();
    // h1: ordinary SLES 15 host. h2: transactional SL-Micro host.
    let (h1, m1) = target("h1", "SLES", "15.5", false);
    let (h2, m2) = target("h2", "SL-Micro", "6.0", true);
    let mut group = HostsGroup::new(vec![h1, h2], false).with_plan_provider(Arc::new(provider));

    InstallOperation::new(vec!["pkg-a".to_owned(), "pkg-b".to_owned()])
        .run(&mut group)
        .await;

    // The provider was consulted for each host with the release derived from
    // its parsed system: SLES 15.5 -> "15", SL-Micro -> "slmicro".
    let mut looked = lookups.lock().unwrap().clone();
    looked.sort();
    assert_eq!(
        looked,
        vec![
            ("installer".to_owned(), "15".to_owned(), false),
            ("installer".to_owned(), "slmicro".to_owned(), true),
        ]
    );

    // The install command ran on both hosts with $packages substituted.
    assert_eq!(
        m1.commands(),
        vec!["zypper -n in -y -l pkg-a pkg-b".to_owned()]
    );
    // The non-transactional host was never rebooted.
    assert!(m1.fired_commands().is_empty());
    assert_eq!(m1.reconnect_count(), 0);

    // The transactional host ran only the install as a normal command; the
    // reboot is dispatched fire-and-forget (the reboot drops the connection),
    // then the host is reconnected — the P2.9 `_reboot` lifecycle.
    assert_eq!(
        m2.commands(),
        vec!["zypper -n in -y -l pkg-a pkg-b".to_owned()]
    );
    assert_eq!(m2.fired_commands(), vec!["systemctl reboot".to_owned()]);
    assert_eq!(m2.reconnect_count(), 1);

    // The check fired once per host.
    let mut who = checked.lock().unwrap().clone();
    who.sort();
    assert_eq!(who, vec!["h1".to_owned(), "h2".to_owned()]);
}

#[tokio::test]
async fn install_shell_quotes_malicious_package_name_end_to_end() {
    // Trust-boundary regression: a package name carrying shell metacharacters,
    // driven through the real HostsGroup install path, must reach the host as a
    // single quoted argument — never as an injected root command.
    let (provider, _lookups, _checked) = RecordingProvider::new();
    let (h1, m1) = target("h1", "SLES", "15.5", false);
    let mut group = HostsGroup::new(vec![h1], false).with_plan_provider(Arc::new(provider));

    InstallOperation::new(vec!["foo; rm -rf /".to_owned()])
        .run(&mut group)
        .await;

    let cmd = m1.commands().into_iter().next().expect("one command ran");
    assert!(
        !cmd.ends_with("foo; rm -rf /"),
        "metacharacters leaked unquoted: {cmd:?}"
    );
    // The command re-splits to the template words plus the literal package name.
    let tokens = shlex::split(&cmd).expect("command re-splits");
    assert_eq!(
        tokens,
        vec![
            "zypper".to_owned(),
            "-n".to_owned(),
            "in".to_owned(),
            "-y".to_owned(),
            "-l".to_owned(),
            "foo; rm -rf /".to_owned(),
        ],
        "package name not a single literal token: {cmd:?}"
    );
}

#[tokio::test]
async fn uninstall_uses_uninstaller_role() {
    let (provider, lookups, _checked) = RecordingProvider::new();
    let (h1, _m1) = target("h1", "SLES", "15.5", false);
    let mut group = HostsGroup::new(vec![h1], false).with_plan_provider(Arc::new(provider));

    UninstallOperation::new(vec!["pkg".to_owned()])
        .run(&mut group)
        .await;

    let looked = lookups.lock().unwrap().clone();
    assert_eq!(
        looked,
        vec![("uninstaller".to_owned(), "15".to_owned(), false)]
    );
}

#[tokio::test]
async fn plans_without_provider_is_no_plan_provider_error() {
    let (h1, _m1) = target("h1", "SLES", "15.5", false);
    let mut group = HostsGroup::new(vec![h1], false);
    match group.plans("installer") {
        Err(HostError::NoPlanProvider) => {}
        Err(other) => panic!("expected NoPlanProvider, got {other}"),
        Ok(_) => panic!("expected NoPlanProvider error, got plans"),
    }
}

#[tokio::test]
async fn plans_with_unknown_system_surfaces_missing_doer() {
    let (provider, _lookups, _checked) = RecordingProvider::new();
    // "mystery" maps to no known release -> get_release() errors -> the role's
    // Missing*Error with an empty release (upstream: no doer for empty key).
    let (h1, _m1) = target("h1", "mystery", "1.0", false);
    let mut group = HostsGroup::new(vec![h1], false).with_plan_provider(Arc::new(provider));

    match group.plans("installer") {
        Err(HostError::MissingInstaller { release }) => assert_eq!(release, ""),
        Err(other) => panic!("expected MissingInstaller, got {other}"),
        Ok(_) => panic!("expected MissingInstaller error, got plans"),
    }
}

#[tokio::test]
async fn select_preserves_injected_provider() {
    let (provider, lookups, _checked) = RecordingProvider::new();
    let (h1, _m1) = target("h1", "SLES", "15.5", false);
    let (h2, _m2) = target("h2", "SLES", "15.5", false);
    let group = HostsGroup::new(vec![h1, h2], false).with_plan_provider(Arc::new(provider));

    // Selecting a subset must carry the provider through so the sub-group can
    // still drive an operation.
    let mut sub = group.select(Some(&["h1".to_owned()]), false).unwrap();
    InstallOperation::new(vec!["pkg".to_owned()])
        .run(&mut sub)
        .await;

    assert_eq!(
        lookups.lock().unwrap().clone(),
        vec![("installer".to_owned(), "15".to_owned(), false)]
    );
}
