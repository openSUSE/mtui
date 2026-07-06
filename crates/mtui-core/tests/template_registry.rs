//! Ports upstream `tests/test_template_registry.py`.

mod support;

use mtui_core::TemplateRegistry;
use mtui_testreport::NullReport;
use support::FakeReport;

fn registry() -> TemplateRegistry {
    TemplateRegistry::new(mtui_config::Config::default())
}

#[test]
fn empty_registry_is_falsey_and_active_is_null() {
    let r = registry();
    assert_eq!(r.len(), 0);
    assert!(r.is_empty());
    assert!(!r.active().is_loaded());
    assert_eq!(r.active().id(), "");
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
    assert_eq!(r.active().id(), "SUSE:Maintenance:1:1");
    assert!(r.contains("SUSE:Maintenance:1:1"));
}

#[test]
fn add_second_does_not_change_active() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    r.add(FakeReport::new("SUSE:Maintenance:2:2").boxed());
    assert_eq!(r.len(), 2);
    assert_eq!(r.active().id(), "SUSE:Maintenance:1:1");
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
    assert_eq!(r.active().id(), "SUSE:Maintenance:1:1");
    assert_eq!(r.rrids(), vec!["SUSE:Maintenance:1:1".to_owned()]);
}

#[test]
fn get_returns_report() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    assert_eq!(
        r.get("SUSE:Maintenance:1:1")
            .map(mtui_testreport::TestReport::id),
        Some("SUSE:Maintenance:1:1".to_owned())
    );
}

#[test]
fn get_unknown_is_none() {
    let r = registry();
    assert!(r.get("nope").is_none());
}

#[test]
fn set_active_flips_pointer() {
    let mut r = registry();
    r.add(FakeReport::new("SUSE:Maintenance:1:1").boxed());
    r.add(FakeReport::new("SUSE:Maintenance:2:2").boxed());
    assert!(r.set_active("SUSE:Maintenance:2:2"));
    assert_eq!(r.active().id(), "SUSE:Maintenance:2:2");
}

#[test]
fn set_active_unknown_returns_false() {
    let mut r = registry();
    assert!(!r.set_active("nope"));
}

#[test]
fn remove_nonactive_keeps_active() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    r.add(FakeReport::new("b").boxed());
    r.remove("b");
    assert_eq!(r.active().id(), "a");
    assert_eq!(r.len(), 1);
}

#[test]
fn remove_active_advances_pointer() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    r.add(FakeReport::new("b").boxed());
    r.remove("a");
    assert_eq!(r.active().id(), "b");
}

#[test]
fn remove_last_empties_and_active_is_null() {
    let mut r = registry();
    r.add(FakeReport::with_hosts("a", &["h1"]).boxed());
    r.remove("a"); // dropping the report tears down its connections (no panic)
    assert_eq!(r.len(), 0);
    assert!(!r.active().is_loaded());
}

#[test]
fn remove_unknown_is_noop() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    r.remove("nope"); // absent → no-op, active unchanged
    assert_eq!(r.len(), 1);
    assert_eq!(r.active().id(), "a");
}

#[test]
fn get_mut_and_active_mut_reach_loaded_report() {
    let mut r = registry();
    r.add(FakeReport::new("a").boxed());
    assert_eq!(r.get_mut("a").map(|b| b.id()), Some("a".to_owned()));
    assert_eq!(r.active_mut().id(), "a");
    assert!(r.get_mut("nope").is_none());
}

#[test]
fn active_mut_on_empty_returns_null() {
    let mut r = registry();
    assert!(!r.active_mut().is_loaded());
}

#[test]
fn active_rrid_tracks_pointer() {
    let mut r = registry();
    assert!(r.active_rrid().is_none());
    r.add(FakeReport::new("a").boxed());
    assert_eq!(r.active_rrid(), Some("a"));
}
