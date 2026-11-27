//! An `update` whose patch left a tracked package off the update's version
//! names it in the command output, under a qualified confirmation, through the
//! real dispatch path both the REPL and MCP use.

mod support;

use std::collections::BTreeSet;

use mtui_config::Config;
use mtui_core::display::{ColorMode, CommandPromptDisplay};
use mtui_core::{Session, dispatch_line, register_all};
use mtui_hosts::{HostsGroup, MockConnection, Target};
use mtui_testreport::{SlReport, TestReport};
use mtui_types::RequestReviewID;
use mtui_types::enums::TargetState;
use mtui_types::hostlog::CommandLog;
use mtui_types::package::Package;
use mtui_types::system::{System, SystemProduct};
use support::Buffer;

const RRID: &str = "SUSE:Maintenance:42:7";
const QUERY: &str = "rpm -q --queryformat \"%{Name} %{Version}-%{Release}\n\" pkg-a";

/// A session with a loaded SL report shipping `pkg-a` at `2.0-1`, over one
/// SLES host whose `pkg-a` stays at `installed` through the patch.
fn session(buf: &Buffer, installed: &str) -> Session {
    let conn = MockConnection::new("h1")
        .with_default(CommandLog::new("", "", "", 0, 0))
        .with_response(
            QUERY,
            CommandLog::new(QUERY, format!("pkg-a {installed}\n"), "", 0, 0),
        );
    let mut target = Target::with_connection("h1", TargetState::Enabled, Box::new(conn));
    let product = SystemProduct::new("SLES", "15.5", "x86_64");
    target.set_system(System::new(product.clone(), BTreeSet::new(), false), false);
    let mut pkg = Package::new("pkg-a");
    pkg.set_required(Some("2.0-1")).unwrap();
    target.set_packages(vec![pkg]);

    let config = Config::default();
    let mut report = SlReport::new(config.clone());
    report.base_mut().rrid = Some(RequestReviewID::parse(RRID).unwrap());
    report.base_mut().packages.insert(
        "SLES:15".to_owned(),
        [("pkg-a".to_owned(), "2.0-1".to_owned())]
            .into_iter()
            .collect(),
    );
    report
        .base_mut()
        .update_repos
        .insert(product, "https://example/repo".to_owned());
    report.base_mut().targets = HostsGroup::new(vec![target], false);
    let display = CommandPromptDisplay::with_sink(Box::new(buf.clone()), ColorMode::Never);
    let mut session = Session::with_display(config, false, display);
    session.templates.add(Box::new(report));
    session
}

#[tokio::test]
async fn an_unchanged_package_qualifies_the_confirmation_and_is_named() {
    let buf = Buffer::default();
    let mut session = session(&buf, "1.0-1");
    dispatch_line(&register_all(), &mut session, "update --noprepare")
        .await
        .expect("the patch passed its checks");
    let out = buf.contents();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.first().copied(),
        Some(
            "update completed on h1: the patch passed its checks, with 1 degradation reported below"
        ),
        "{out}"
    );
    assert!(
        lines.contains(
            &"h1: pkg-a was not updated by the patch (before the patch: 1.0-1; after it: 1.0-1; \
              the update's version: 2.0-1)"
        ),
        "{out}"
    );
}

/// At the update's version after the patch is not enough: it was there
/// before the patch, so this run did not install it.
#[tokio::test]
async fn a_package_already_at_the_updates_version_is_named_too() {
    let buf = Buffer::default();
    let mut session = session(&buf, "2.0-1");
    dispatch_line(&register_all(), &mut session, "update --noprepare")
        .await
        .expect("the patch passed its checks");
    assert!(
        buf.contents()
            .contains("h1: pkg-a was already at the update's version before the patch"),
        "{}",
        buf.contents()
    );
}
