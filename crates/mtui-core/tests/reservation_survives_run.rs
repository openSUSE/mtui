//! A `lock -c <text>` reservation outlives the operations that re-lock the
//! host: they must neither strip its comment nor release it.

mod support;

use mtui_core::display::{ColorMode, CommandPromptDisplay};
use mtui_core::{Session, dispatch_line, register_all};
use mtui_hosts::{MockConnection, TARGET_LOCK_PATH};
use mtui_types::hostlog::CommandLog;
use support::{Buffer, FakeReport};

const RRID: &str = "SUSE:Maintenance:1:1";

fn host(name: &str) -> MockConnection {
    MockConnection::new(name).with_default(CommandLog::new("", "ok", "", 0, 0))
}

fn session_over(conns: &[MockConnection]) -> Session {
    let display = CommandPromptDisplay::with_sink(Box::new(Buffer::default()), ColorMode::Never);
    let mut session = Session::with_display(mtui_config::Config::default(), false, display);
    session
        .templates
        .add(FakeReport::with_connections(RRID, conns).boxed());
    session
}

fn lockfile(conn: &MockConnection) -> Option<String> {
    conn.file_contents(TARGET_LOCK_PATH)
        .map(|b| String::from_utf8(b).unwrap())
}

#[tokio::test]
async fn run_leaves_a_reservation_in_place_and_unlock_releases_it() {
    let registry = register_all();
    let h1 = host("h1");
    let mut session = session_over(std::slice::from_ref(&h1));

    dispatch_line(&registry, &mut session, "lock -c reserved")
        .await
        .expect("lock");
    let reserved = lockfile(&h1).expect("reservation taken");
    assert!(reserved.ends_with(":reserved"), "{reserved}");

    dispatch_line(&registry, &mut session, "run sh -c true")
        .await
        .expect("run");
    assert_eq!(
        lockfile(&h1).as_deref(),
        Some(reserved.as_str()),
        "run rewrote or released the reservation"
    );

    dispatch_line(&registry, &mut session, "unlock")
        .await
        .expect("unlock");
    assert_eq!(lockfile(&h1), None, "a plain unlock releases it");
}

#[tokio::test]
async fn a_blocked_run_keeps_the_reservation_it_found_on_the_other_host() {
    let registry = register_all();
    let h1 = host("h1");
    let h2 = host("h2").with_file(
        TARGET_LOCK_PATH,
        b"1700000000:someone-else:4242:busy".to_vec(),
    );
    let mut session = session_over(&[h1.clone(), h2]);

    dispatch_line(&registry, &mut session, "lock -t h1 -c reserved")
        .await
        .expect("lock");
    let reserved = lockfile(&h1).expect("reservation taken");

    dispatch_line(&registry, &mut session, "run sh -c true")
        .await
        .expect_err("h2 is held by someone else");
    assert_eq!(
        lockfile(&h1).as_deref(),
        Some(reserved.as_str()),
        "the partial rollback removed the reservation"
    );
}
