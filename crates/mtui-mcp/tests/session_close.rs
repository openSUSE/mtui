//! Port of the `close()` host-teardown behaviours from upstream
//! `tests/test_mcp_session.py` (bead `mtui-rs-76e.13`).
//!
//! Three behaviours, matching upstream 1:1:
//!
//! * `close_releases_pool_claims` — `close()` releases every loaded template's
//!   host-arbitration pool claims (upstream `test_close_releases_pool_claims`).
//! * `close_disconnects_every_loaded_templates_hosts` — `close()` disconnects
//!   hosts on *all* loaded templates, not just the active one (upstream
//!   `test_close_disconnects_every_loaded_templates_hosts`).
//! * the wedged-close bounded-wait case is a colocated `#[cfg(test)]` unit test
//!   in `src/session.rs` (it needs the `pub(crate)` timeout seam) — see
//!   `close_with_timeout_survives_a_wedged_close` there.
//!
//! ## Rust deviation
//!
//! Upstream clears `report.targets` after closing and asserts `targets == {}`.
//! The Rust `HostsGroup::close` (like the REPL `quit`) closes each `Target` but
//! leaves it in the group with its now-dead connection, so these tests assert
//! the connection observes `is_closed()` rather than an emptied group.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_mcp::McpSession;
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::enums::{ExecutionMode, TargetState};

const RRID_A: &str = "SUSE:Maintenance:1:1";
const RRID_B: &str = "SUSE:Maintenance:2:1";

/// A session over a throwaway temp `template_dir`.
fn session() -> Arc<McpSession> {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    // Leak the tempdir guard: the session outlives this fn and only reads the
    // path; the OS reclaims it at process exit.
    std::mem::forget(tmp);
    McpSession::new(config)
}

/// Build a target wrapping a fresh [`MockConnection`], returning the target and
/// a shared handle to the mock (its state is `Arc`-shared) so the test can
/// observe `is_closed()` after the target is moved into a host group.
fn host_with_mock(name: &str) -> (Target, MockConnection) {
    let mock = MockConnection::new(name);
    let target = Target::with_connection(
        name,
        TargetState::Enabled,
        ExecutionMode::Parallel,
        Box::new(mock.clone()),
    );
    (target, mock)
}

/// `close()` releases pool claims on *every* loaded template.
#[tokio::test]
async fn close_releases_pool_claims() {
    let sess = session();
    {
        let mut guard = sess.session().lock().await;
        for rrid in [RRID_A, RRID_B] {
            let mut report = ObsReport::new(guard.config.clone());
            report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
            // Seed a pool claim so release has something to clear (the observable
            // analogue of upstream's `release_pool_claims.assert_called_once()`).
            report.base_mut().pool_claims.insert("some-host".to_owned());
            guard.templates.add(Box::new(report));
        }
        guard.templates.set_active(RRID_A);
    }

    sess.close().await;

    let guard = sess.session().lock().await;
    for rrid in [RRID_A, RRID_B] {
        let entry = guard.templates.handle(rrid).expect("template still loaded");
        let report = entry.try_lock().expect("entry uncontended after close");
        assert!(
            report.base().pool_claims.is_empty(),
            "close must release pool claims for {rrid}",
        );
    }
}

/// `close()` disconnects hosts on *all* loaded templates, not just the active
/// one.
#[tokio::test]
async fn close_disconnects_every_loaded_templates_hosts() {
    let sess = session();
    let (active_target, active_mock) = host_with_mock("active-host");
    let (other_target, other_mock) = host_with_mock("other-host");

    {
        let mut guard = sess.session().lock().await;

        let mut active = ObsReport::new(guard.config.clone());
        active.base_mut().rrid = Some(RequestReviewID::parse(RRID_A).unwrap());
        active.base_mut().targets = HostsGroup::new(vec![active_target], false);
        guard.templates.add(Box::new(active));

        let mut other = ObsReport::new(guard.config.clone());
        other.base_mut().rrid = Some(RequestReviewID::parse(RRID_B).unwrap());
        other.base_mut().targets = HostsGroup::new(vec![other_target], false);
        guard.templates.add(Box::new(other));

        guard.templates.set_active(RRID_A);
    }

    sess.close().await;

    // Both the active and the non-active template's hosts are disconnected.
    // (Rust deviation: the groups themselves are not emptied; the connections
    // observe the close.)
    assert!(active_mock.is_closed(), "active template's host was closed");
    assert!(
        other_mock.is_closed(),
        "non-active template's host was closed"
    );
}

/// A second `close()` is a cheap no-op (idempotent): it re-runs over
/// already-released claims and already-closed targets without panicking.
#[tokio::test]
async fn close_is_idempotent() {
    let sess = session();
    let (target, mock) = host_with_mock("h1");
    {
        let mut guard = sess.session().lock().await;
        let mut report = ObsReport::new(guard.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(RRID_A).unwrap());
        report.base_mut().pool_claims.insert("h1".to_owned());
        report.base_mut().targets = HostsGroup::new(vec![target], false);
        guard.templates.add(Box::new(report));
        guard.templates.set_active(RRID_A);
    }

    sess.close().await;
    sess.close().await; // second call: no-op, must not panic

    assert!(mock.is_closed());
    let guard = sess.session().lock().await;
    let entry = guard.templates.handle(RRID_A).unwrap();
    let report = entry.try_lock().expect("entry uncontended after close");
    assert!(report.base().pool_claims.is_empty());
}
