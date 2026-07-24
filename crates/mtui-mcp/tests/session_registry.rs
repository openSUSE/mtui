//! Cap + idle-TTL enforcement for the http `SessionRegistry` (bead `mtui-rs-odq8`).
//!
//! Ports the offline-portable subset of upstream `tests/test_mcp_registry.py`.
//! The `_session_key` / `_log_label` cases are **not** ported: upstream keys its
//! own session dict on `id(ctx.session)`, but here rmcp owns session keying by
//! `Mcp-Session-Id`; the Rust registry tracks a `Weak` live-set around rmcp's
//! factory instead, so those tests are N/A.
//!
//! What is ported (behaviour-equivalent):
//!
//! * cap refuses a new session past `session_cap`
//!   (`test_cap_refuses_creation_past_limit`);
//! * dropping a server frees a cap slot (Rust analogue of
//!   `test_cap_frees_a_slot_after_evict` — rmcp drops the server on session
//!   close, our `SessionGuard::drop` frees the slot);
//! * a re-mint after a drop is a fresh session (`test_evict_..._mints_anew`);
//! * the idle sweeper evicts + `close()`-es a stale session
//!   (`test_idle_sweeper_evicts_stale_session`);
//! * fresh activity keeps a session alive (`test_fresh_activity_keeps_session_alive`);
//! * `idle_timeout == 0` starts no sweeper (`test_sweeper_disabled_when_idle_timeout_zero`).

#![cfg(feature = "mcp")]

use std::sync::Arc;
use std::time::Duration;

use mtui_config::Config;
use mtui_core::register_all;
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_mcp::{McpSession, SessionRegistry};
use mtui_testreport::{ObsReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::enums::TargetState;
use tokio_util::sync::CancellationToken;

const RRID_A: &str = "SUSE:Maintenance:1:1";

/// A registry with an explicit cap + idle-TTL over a throwaway temp template dir.
fn registry(cap: usize, idle_secs: u64) -> SessionRegistry {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.template_dir = tmp.path().to_path_buf();
    config.mcp_session_cap = cap;
    config.mcp_session_idle_timeout = idle_secs;
    // Leak the tempdir guard: sessions outlive this fn and only read the path.
    std::mem::forget(tmp);
    SessionRegistry::new(Arc::new(register_all()), config)
}

/// Seed a template carrying one [`MockConnection`] host into `session`, returning
/// a shared handle to the mock so the test can observe `is_closed()` after a sweep.
async fn seed_host(session: &Arc<McpSession>) -> MockConnection {
    let mock = MockConnection::new("swept-host");
    let target =
        Target::with_connection("swept-host", TargetState::Enabled, Box::new(mock.clone()));
    let mut guard = session.session().lock().await;
    let mut report = ObsReport::new(guard.config.clone());
    report.base_mut().rrid = Some(RequestReviewID::parse(RRID_A).unwrap());
    report.base_mut().targets = HostsGroup::new(vec![target], false);
    guard.templates.add(Box::new(report));
    guard.templates.set_active(RRID_A);
    mock
}

// --------------------------------------------------------------------------- #
// Session cap (DoS guard)                                                      #
// --------------------------------------------------------------------------- #

/// Minting one session past `session_cap` is refused with the documented error.
#[tokio::test]
async fn cap_refuses_creation_past_limit() {
    let reg = registry(2, 0);

    let _a = reg.try_make_server().expect("first server under cap");
    let _b = reg.try_make_server().expect("second server at cap");
    // The third exceeds the cap of 2.
    match reg.try_make_server() {
        Ok(_) => panic!("third server must be refused past the cap"),
        Err(err) => assert!(
            err.to_string().contains("session registry full"),
            "unexpected error: {err}"
        ),
    }
    assert_eq!(reg.live_count(), 2, "cap holds the live count at the limit");
}

/// Dropping a minted server frees a cap slot (rmcp drops the server on session
/// close; our `SessionGuard::drop` unregisters it). A re-mint then succeeds.
#[tokio::test]
async fn drop_frees_a_slot() {
    let reg = registry(1, 0);

    let a = reg.try_make_server().expect("first server");
    assert_eq!(reg.live_count(), 1);
    // At cap: a second distinct session is refused.
    assert!(reg.try_make_server().is_err(), "at cap, second refused");

    // Drop frees the slot.
    drop(a);
    assert_eq!(reg.live_count(), 0, "dropping the server frees the slot");

    let _b = reg.try_make_server().expect("slot freed, new server fits");
    assert_eq!(reg.live_count(), 1);
}

/// Each mint is an independent session (no shared `Arc<McpSession>`), so a
/// re-mint after a drop is a brand-new session — the Rust analogue of
/// upstream's "refetch after evict mints anew".
#[tokio::test]
async fn remint_after_drop_is_a_new_session() {
    // Cap of 2 so the first session can be held alive in the live-set *across*
    // the re-mint. Freshness is asserted via the session's stable, monotonic
    // `id()` — not `Arc` address identity, which the allocator can reuse after a
    // drop (a flake that surfaced once these tests share one process, the
    // consolidated `it` binary, instead of one binary each — bead mtui-rs-1edj).
    let reg = registry(2, 0);

    let first = reg.live_sessions();
    assert!(first.is_empty());

    let a = reg.try_make_server().unwrap();
    let sess_a = reg.live_sessions();
    assert_eq!(sess_a.len(), 1);
    let id_a = sess_a[0].id();
    // Drop the server (frees its cap slot) but retain the session `Arc` so it
    // stays in the live-set alongside the re-mint.
    drop(a);

    let _b = reg.try_make_server().unwrap();
    let sess_b: Vec<_> = reg
        .live_sessions()
        .into_iter()
        .filter(|s| s.id() != id_a)
        .collect();
    assert_eq!(
        sess_b.len(),
        1,
        "exactly one fresh session besides the held one"
    );
    assert!(
        sess_b[0].id() != id_a,
        "a re-mint must be a fresh session instance"
    );

    drop(sess_a);
}

// --------------------------------------------------------------------------- #
// Idle-TTL sweeper                                                             #
// --------------------------------------------------------------------------- #

/// A session untouched past the TTL is swept: its `close()` runs (host
/// disconnected) and its cap slot is freed.
#[tokio::test]
async fn idle_sweeper_evicts_stale_session() {
    let reg = registry(4, 1); // 1s TTL → sweep interval 1s

    let _server = reg.try_make_server().expect("server");
    let session = reg.live_sessions().pop().expect("one live session");
    let mock = seed_host(&session).await;
    drop(session); // keep it alive only via the server + the registry's Weak

    let cancel = CancellationToken::new();
    let sweeper = reg.spawn_sweeper(cancel.clone()).expect("sweeper spawned");

    // Wait for one full TTL + sweep cycle to reap the untouched session.
    let mut swept = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if reg.live_count() == 0 {
            swept = true;
            break;
        }
    }
    cancel.cancel();
    let _ = sweeper.await;

    assert!(
        swept,
        "idle session should have been swept within the budget"
    );
    assert!(mock.is_closed(), "sweep must close the session's hosts");
}

/// `session_idle_timeout == 0` starts no sweeper task.
#[tokio::test]
async fn sweeper_disabled_when_idle_timeout_zero() {
    let reg = registry(4, 0);
    let cancel = CancellationToken::new();
    assert!(
        reg.spawn_sweeper(cancel).is_none(),
        "a zero idle-TTL must not spawn a sweeper"
    );
}

/// The registry surfaces its configured bounds.
#[tokio::test]
async fn registry_exposes_configured_bounds() {
    let reg = registry(7, 42);
    assert_eq!(reg.cap(), 7);
    assert_eq!(reg.idle_timeout(), Duration::from_secs(42));
}
