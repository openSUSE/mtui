//! Ports upstream `tests/test_template_registry.py`.

mod support;

use mtui_core::TemplateRegistry;
use mtui_testreport::NullReport;
use support::FakeReport;

fn registry() -> TemplateRegistry {
    TemplateRegistry::new(mtui_config::Config::default())
}

/// The id of the report loaded under `rrid`, read through its (uncontended)
/// entry lock — the null-object fallback now lives on `Session`, so the registry
/// is inspected via handles.
fn id_of(r: &TemplateRegistry, rrid: &str) -> Option<String> {
    let handle = r.handle(rrid)?;
    let report = handle.try_lock().expect("entry uncontended in test");
    Some(report.id())
}

/// The active report's id, or `None` when nothing is loaded.
fn active_id(r: &TemplateRegistry) -> Option<String> {
    let handle = r.active_handle()?;
    let report = handle.try_lock().expect("entry uncontended in test");
    Some(report.id())
}

#[test]
fn empty_registry_is_falsey_and_active_is_none() {
    let r = registry();
    assert_eq!(r.len(), 0);
    assert!(r.is_empty());
    assert!(r.active_handle().is_none());
    assert!(active_id(&r).is_none());
}

#[test]
fn id_is_stable_and_nonempty() {
    let r = registry();
    let first = r.id().to_owned();
    assert!(!first.is_empty());
    assert_eq!(r.id(), first);
}

#[test]
fn add_first_becomes_active() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    assert_eq!(r.len(), 1);
    assert!(!r.is_empty());
    assert_eq!(active_id(&r).as_deref(), Some("SUSE:Maintenance:1:1"));
    assert!(r.contains("SUSE:Maintenance:1:1"));
}

#[test]
fn add_second_does_not_change_active() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    r.add(FakeReport::new("SUSE:Maintenance:2:2").boxed());
    assert_eq!(r.len(), 2);
    assert_eq!(active_id(&r).as_deref(), Some("SUSE:Maintenance:1:1"));
}

#[test]
fn add_ignores_empty_rrid_sentinel() {
    let mut r = registry();
    // A NullReport (failed load) has an empty RRID; it must never be keyed in.
    r.add(Box::new(NullReport::new(mtui_config::Config::default())));
    assert_eq!(r.len(), 0);
    assert!(r.is_empty());
    assert!(!r.contains(""));
}

#[test]
fn add_sentinel_does_not_disturb_loaded_templates() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    r.add(Box::new(NullReport::new(mtui_config::Config::default()))); // failed load mid-session
    assert_eq!(r.len(), 1);
    assert_eq!(active_id(&r).as_deref(), Some("SUSE:Maintenance:1:1"));
    assert_eq!(r.rrids(), vec!["SUSE:Maintenance:1:1".to_owned()]);
}

#[test]
fn handle_returns_report() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    assert_eq!(
        id_of(&r, "SUSE:Maintenance:1:1"),
        Some("SUSE:Maintenance:1:1".to_owned())
    );
}

#[test]
fn handle_unknown_is_none() {
    let r = registry();
    assert!(r.handle("nope").is_none());
}

#[test]
fn set_active_flips_pointer() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    r.add(FakeReport::new("SUSE:Maintenance:2:2").boxed());
    assert!(r.set_active("SUSE:Maintenance:2:2"));
    assert_eq!(active_id(&r).as_deref(), Some("SUSE:Maintenance:2:2"));
}

#[test]
fn set_active_unknown_returns_false() {
    let mut r = registry();
    assert!(!r.set_active("nope"));
}

#[tokio::test]
async fn remove_nonactive_keeps_active() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    r.add(FakeReport::new("b").boxed());
    r.remove("b").await;
    assert_eq!(active_id(&r).as_deref(), Some("a"));
    assert_eq!(r.len(), 1);
}

#[tokio::test]
async fn remove_active_advances_pointer() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    r.add(FakeReport::new("b").boxed());
    r.remove("a").await;
    assert_eq!(active_id(&r).as_deref(), Some("b"));
}

#[tokio::test]
async fn remove_last_empties_and_active_is_null() {
    let mut r = registry();
    r.add(FakeReport::with_hosts("a", &["h1"]).boxed());
    r.remove("a").await; // teardown releases claims/locks + closes connections
    assert_eq!(r.len(), 0);
    assert!(r.active_handle().is_none());
}

#[tokio::test]
async fn remove_unknown_is_noop() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    r.remove("nope").await; // absent → no-op, active unchanged
    assert_eq!(r.len(), 1);
    assert_eq!(active_id(&r).as_deref(), Some("a"));
}

#[test]
fn handle_and_active_handle_reach_loaded_report() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    assert_eq!(id_of(&r, "a"), Some("a".to_owned()));
    assert_eq!(active_id(&r).as_deref(), Some("a"));
    assert!(r.handle("nope").is_none());
}

#[test]
fn active_handle_on_empty_is_none() {
    let r = registry();
    assert!(r.active_handle().is_none());
}

#[test]
fn active_rrid_tracks_pointer() {
    let mut r = registry();
    assert!(r.active_rrid().is_none());
    r.add(FakeReport::new("a").boxed());
    assert_eq!(r.active_rrid(), Some("a"));
}
