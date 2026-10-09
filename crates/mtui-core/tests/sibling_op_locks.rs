//! Two templates loaded in one session share a refhost: one's operation lock
//! must exclude the other's `run`, and survive its abort.

mod support;

use std::collections::BTreeSet;

use mtui_core::display::{ColorMode, CommandPromptDisplay};
use mtui_core::{Session, dispatch_line, register_all};
use mtui_hosts::{MockConnection, TARGET_LOCK_PATH};
use mtui_types::hostlog::CommandLog;
use support::{Buffer, FakeReport};

const A: &str = "SUSE:Maintenance:1:1";
const B: &str = "SUSE:Maintenance:2:2";

fn two_templates_on_one_host() -> (Session, Buffer, MockConnection) {
    let conn = MockConnection::new("h1").with_default(CommandLog::new("", "ok", "", 0, 0));
    let buf = Buffer::default();
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    let mut session = Session::with_display(mtui_config::Config::default(), false, display);
    session
        .templates
        .add(FakeReport::with_shared_connection(A, &conn).boxed());
    session
        .templates
        .add(FakeReport::with_shared_connection(B, &conn).boxed());
    (session, buf, conn)
}

async fn take_operation_lock(session: &mut Session, rrid: &str) {
    let names: BTreeSet<String> = ["h1".to_owned()].into();
    assert!(session.activate(rrid).is_active());
    session.targets_mut().lock_selected("", &names).await;
    session.release_active_guard();
}

#[tokio::test]
async fn run_on_one_template_refuses_while_a_sibling_holds_the_host_and_keeps_its_lock() {
    let registry = register_all();
    let (mut session, _buf, conn) = two_templates_on_one_host();
    take_operation_lock(&mut session, A).await;
    assert!(conn.file_contents(TARGET_LOCK_PATH).is_some());

    let err = dispatch_line(&registry, &mut session, &format!("run -T {B} -t h1 true"))
        .await
        .expect_err("a sibling's operation lock must block the run");

    assert!(err.to_string().contains("could not lock"), "{err}");
    assert!(
        conn.file_contents(TARGET_LOCK_PATH).is_some(),
        "the sibling's lockfile was deleted"
    );
}

#[tokio::test]
async fn run_on_one_template_succeeds_once_the_sibling_released_the_host() {
    let registry = register_all();
    let (mut session, _buf, conn) = two_templates_on_one_host();
    take_operation_lock(&mut session, A).await;

    let names: BTreeSet<String> = ["h1".to_owned()].into();
    assert!(session.activate(A).is_active());
    session.targets_mut().unlock_taken(&names).await;
    session.release_active_guard();
    assert!(conn.file_contents(TARGET_LOCK_PATH).is_none());

    dispatch_line(&registry, &mut session, &format!("run -T {B} -t h1 true"))
        .await
        .expect("a free host runs");
    assert!(conn.file_contents(TARGET_LOCK_PATH).is_none());
}
