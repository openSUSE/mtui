//! #525: a successful workflow fan-out must reach the MCP client as text.
//!
//! The registry-wide guard in `mtui-core` proves the *display* is written; this
//! proves the MCP pipe carries it, on both the foreground and the background-job
//! path, driving the real report flows over `MockConnection` hosts rather than a
//! test double (the double is what hid the defect in the first place).

#![cfg(feature = "mcp")]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use mtui_config::Config;
use mtui_core::{Registry, register_all};
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_mcp::{JobState, McpSession};
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::system::{System, SystemProduct};
use tempfile::TempDir;

const RRID: &str = "SUSE:Maintenance:42:7";
const HOSTS: [&str; 2] = ["h1", "h2"];

/// An enabled SLES 15.5 host whose every command exits 0 printing the
/// downgrade probe's answer, so all five flows resolve a doer and pass their
/// checks — the clean-transaction case, which emits no diagnostics.
fn sles_target(hostname: &str) -> Target {
    let conn = MockConnection::new(hostname).with_default(CommandLog::new(
        "",
        "pkg-a = 1.0-1\n",
        "",
        0,
        0,
    ));
    let mut target = Target::with_connection(hostname, TargetState::Enabled, Box::new(conn));
    target.set_system(
        System::new(
            SystemProduct::new("SLES", "15.5", "x86_64"),
            BTreeSet::new(),
            false,
        ),
        false,
    );
    target
}

/// A session with one loaded, active report carrying [`HOSTS`] and a package,
/// over a throwaway `template_dir` the caller keeps alive.
async fn session_with_hosts() -> (Arc<McpSession>, TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();

    let session = McpSession::new(config);
    {
        let mut guard = session.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
        // Non-empty, or `update`'s #396 pre-flight refuses before dispatching.
        report.base_mut().packages.insert(
            "SLES:15".to_owned(),
            [("pkg-a".to_owned(), "2.0-1".to_owned())]
                .into_iter()
                .collect(),
        );
        let targets: Vec<Target> = HOSTS.iter().map(|h| sles_target(h)).collect();
        report.base_mut().targets = HostsGroup::new(targets, false);
        guard.templates.add(Box::new(report));
        guard.templates.set_active(RRID);
    }
    (session, tmp)
}

/// Awaits a job's terminal state, polling as the worker records it.
async fn await_terminal(session: &McpSession, job_id: &str) -> JobState {
    for _ in 0..1000 {
        let state = session.job_status(job_id).expect("job exists").state;
        if state != JobState::Running {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("job {job_id} did not reach a terminal state");
}

/// The five workflow fan-outs and the argv that drives each to a clean success.
fn cases() -> Vec<(&'static str, Vec<String>)> {
    let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    vec![
        ("update", argv(&[])),
        ("prepare", argv(&["-u"])),
        ("install", argv(&["pkg-a"])),
        ("uninstall", argv(&["pkg-a"])),
        ("downgrade", argv(&[])),
    ]
}

/// A foreground tool call never returns an empty success block, and its text
/// names every host the op ran over.
#[tokio::test]
async fn foreground_success_returns_the_hosts_it_ran_over() {
    let registry: Registry = register_all();
    for (name, argv) in cases() {
        let (sess, _tmp) = session_with_hosts().await;
        let out = sess
            .run_command(&registry, name, &argv)
            .await
            .unwrap_or_else(|e| panic!("{name} should succeed: {e:?}"));
        assert!(
            !out.trim().is_empty(),
            "{name} returned an empty success block"
        );
        assert!(
            out.contains(&format!("{name} completed on")),
            "got: {out:?}"
        );
        for host in HOSTS {
            assert!(out.contains(host), "{name} did not name {host}: {out:?}");
        }
    }
}

/// The same for `background: true` — the worker goes through `run_command`, but
/// the text reaches the client via `job_result`.
#[tokio::test]
async fn backgrounded_success_returns_the_hosts_it_ran_over() {
    let registry = Arc::new(register_all());
    for (name, argv) in cases() {
        let (sess, _tmp) = session_with_hosts().await;
        let job_id = sess
            .start_job(Arc::clone(&registry), name, argv)
            .unwrap_or_else(|e| panic!("{name} should start: {e:?}"));
        assert_eq!(
            await_terminal(&sess, &job_id).await,
            JobState::Done,
            "{name}"
        );
        let out = sess
            .job_result(&job_id)
            .unwrap_or_else(|e| panic!("{name} job_result: {e:?}"));
        assert!(!out.trim().is_empty(), "{name} job yielded no text");
        assert!(
            out.contains(&format!("{name} completed on")),
            "got: {out:?}"
        );
        for host in HOSTS {
            assert!(out.contains(host), "{name} did not name {host}: {out:?}");
        }
    }
}
