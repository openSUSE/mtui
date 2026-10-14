//! Repository URLs may embed credentials (`https://user:token@host/...`). The
//! URL itself must still reach `zypper` verbatim, but nothing mtui logs, stores
//! or reports about the command may carry the userinfo.
//!
//! The tracing capture installs one global subscriber and scopes the sink per
//! thread (see `mtui-datasources/tests/log_capture.rs` for why a thread-local
//! subscriber is not enough). Unlike that helper it records **every** field, not
//! only `message`, because command lines are logged as structured fields.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

use mtui_hosts::{HostsGroup, MockConnection, RepoFailure, Target};
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::rrid::RequestReviewID;
use mtui_types::shellquote::quote_args;
use mtui_types::system::{System, SystemProduct};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

pub(crate) const USER: &str = "audituser";
pub(crate) const CANARY: &str = "MTUI_SECRET_CANARY";
pub(crate) const URL: &str = "https://audituser:MTUI_SECRET_CANARY@repo.example/SLES-15-x86_64/";
const SAFE_URL: &str = "https://***@repo.example/SLES-15-x86_64/";

thread_local! {
    static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct CaptureLayer;

struct FieldVisitor(String);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl<S: tracing::Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        SINK.with(|s| {
            if let Some(buf) = s.borrow_mut().as_mut() {
                let mut v = FieldVisitor(event.metadata().level().to_string());
                event.record(&mut v);
                buf.push(v.0);
            }
        });
    }
}

/// Every event `f` emits on this thread — level and all fields — one per line.
pub(crate) async fn capture_events<F, Fut>(f: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
    });
    SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
    f().await;
    SINK.with(|s| s.borrow_mut().take())
        .unwrap_or_default()
        .join("\n")
}

/// Fails without echoing the credential-bearing text it was given.
pub(crate) fn assert_no_credentials(what: &str, text: &str) {
    assert!(
        !text.contains(CANARY) && !text.contains(USER),
        "{what} leaked repository credentials"
    );
}

fn rrid() -> RequestReviewID {
    RequestReviewID::parse("SUSE:Maintenance:1:1").expect("valid RRID")
}

fn sles() -> SystemProduct {
    SystemProduct::new("SLES", "15", "x86_64")
}

fn repos() -> BTreeMap<SystemProduct, String> {
    BTreeMap::from([(sles(), URL.to_owned())])
}

fn target(conn: MockConnection) -> Target {
    let mut t = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
    t.set_system(System::new(sles(), Default::default(), false), false);
    t
}

fn rr_command() -> String {
    format!("zypper rr {}", quote_args(&[URL]))
}

/// A mock whose `zypper rr` fails the way zypper does: echoing the repository
/// it was asked to remove on both streams, with the command recorded on the log
/// as the SSH layer would.
fn conn_with_failing_remove() -> MockConnection {
    let cmd = rr_command();
    MockConnection::new("h1").with_response(
        cmd.clone(),
        CommandLog::new(
            cmd,
            format!("Removing repository '{URL}'\n"),
            format!("Error: failed to remove {URL}\n"),
            1,
            0,
        ),
    )
}

fn assert_ran_with_exact_url(conn: &MockConnection, verb: &str) {
    let issued = conn
        .commands()
        .into_iter()
        .find(|c| c.starts_with(&format!("zypper {verb}")))
        .expect("the zypper command ran");
    let tokens = shlex::split(&issued).expect("command re-splits");
    assert!(
        tokens.iter().any(|t| t == URL),
        "the host must receive the exact credential-bearing URL as one argument"
    );
}

#[tokio::test]
async fn adding_a_repo_logs_no_credentials_and_still_sends_them_to_zypper() {
    let conn = MockConnection::new("h1");
    let handle = conn.clone();
    let mut t = target(conn);

    let events = capture_events(|| async {
        t.repo_manager().run_zypper("ar", &repos(), &rrid()).await;
    })
    .await;

    assert!(
        events.contains("Adding repo"),
        "the add event was not captured; secrecy assertions would be vacuous"
    );
    assert_no_credentials("tracing events", &events);
    assert!(events.contains(SAFE_URL), "the masked URL stays useful");
    assert_ran_with_exact_url(&handle, "ar");
}

#[tokio::test]
async fn failed_remove_masks_log_warning_failure_and_host_log() {
    let conn = conn_with_failing_remove();
    let handle = conn.clone();
    let mut t = target(conn);

    let events = capture_events(|| async {
        t.repo_manager().run_zypper("rr", &repos(), &rrid()).await;
    })
    .await;

    assert!(
        events.contains("Removing repo") && events.contains("removing repo"),
        "the remove and failure events were not captured; secrecy assertions would be vacuous"
    );
    assert_no_credentials("tracing events", &events);

    let failure = t
        .last_repo()
        .expect("a run recorded its verdict")
        .as_ref()
        .expect_err("the remove failed");
    assert!(matches!(failure, RepoFailure::Remove { .. }));
    assert_no_credentials("RepoFailure display", &failure.to_string());
    assert_no_credentials("RepoFailure debug", &format!("{failure:?}"));

    for entry in t.out().iter() {
        assert_no_credentials("host log command", &entry.command);
        assert_no_credentials("host log stdout", &entry.stdout);
        assert_no_credentials("host log stderr", &entry.stderr);
    }
    assert_ran_with_exact_url(&handle, "rr");
}

#[tokio::test]
async fn host_log_masks_credentials_from_any_connection() {
    let cmd = format!("zypper lr -u; echo {URL}");
    let conn = MockConnection::new("h1").with_response(
        cmd.clone(),
        CommandLog::new(cmd.clone(), format!("{URL}\n{URL}\n"), "", 0, 3),
    );
    let mut group = HostsGroup::new(vec![target(conn)], false);

    group.run(cmd.as_str()).await;

    let entry = group
        .get("h1")
        .expect("member present")
        .out()
        .last()
        .expect("the run was logged");
    assert_no_credentials("host log command", &entry.command);
    assert_no_credentials("host log stdout", &entry.stdout);
    assert_eq!(entry.stdout.matches("https://***@repo.example").count(), 2);
    assert_eq!((entry.exitcode, entry.runtime), (0, 3));
}
