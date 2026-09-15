//! Integration tests for the teregen token store
//! (`mtui_datasources::teregen::tokens`).

use mtui_datasources::teregen::{CachedToken, TokenStore};

use crate::log_capture::capture_logs;

fn sample(base: &str, principal: &str) -> CachedToken {
    CachedToken {
        base: base.to_owned(),
        principal: principal.to_owned(),
        token: "a".repeat(64),
        minted_at: "2026-09-13T18:22:01Z".to_owned(),
    }
}

#[test]
fn round_trips_through_a_tempdir() {
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::at(dir.path().join("teregen-token.json"));
    let cached = sample("https://qam.suse.de/api/v2", "alice");
    store.store(&cached).unwrap();

    let loaded = store
        .load("https://qam.suse.de/api/v2", "alice")
        .expect("round trip");
    assert_eq!(loaded.token, cached.token);
    assert_eq!(loaded.base, cached.base);
    assert_eq!(loaded.principal, cached.principal);
    assert_eq!(loaded.minted_at, cached.minted_at);
}

#[test]
fn base_mismatch_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::at(dir.path().join("teregen-token.json"));
    store
        .store(&sample("https://staging.example/api/v2", "alice"))
        .unwrap();
    assert!(store.load("https://qam.suse.de/api/v2", "alice").is_none());
}

#[test]
fn principal_mismatch_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::at(dir.path().join("teregen-token.json"));
    store
        .store(&sample("https://qam.suse.de/api/v2", "alice"))
        .unwrap();
    assert!(store.load("https://qam.suse.de/api/v2", "bob").is_none());
}

#[test]
fn missing_file_yields_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::at(dir.path().join("does-not-exist.json"));
    assert!(store.load("base", "alice").is_none());
}

#[tokio::test]
async fn corrupt_json_yields_none_at_debug() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("teregen-token.json");
    std::fs::write(&path, b"not json").unwrap();
    let store = TokenStore::at(path);

    let mut found: Option<CachedToken> = None;
    let logs = capture_logs(|| async {
        found = store.load("base", "alice");
    })
    .await;

    assert!(found.is_none());
    assert!(logs.contains("malformed"), "logs: {logs}");
}

#[cfg(unix)]
#[test]
fn stored_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("teregen-token.json");
    let store = TokenStore::at(path.clone());
    store.store(&sample("base", "alice")).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "got {mode:o}");
}

#[cfg(unix)]
#[tokio::test]
async fn loose_permissions_warn_but_the_token_still_loads() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("teregen-token.json");
    let store = TokenStore::at(path.clone());
    store
        .store(&sample("https://qam.suse.de/api/v2", "alice"))
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let mut found: Option<CachedToken> = None;
    let logs = capture_logs(|| async {
        found = store.load("https://qam.suse.de/api/v2", "alice");
    })
    .await;

    assert!(logs.contains("group/world-accessible"), "logs: {logs}");
    assert!(found.is_some(), "a loose-permission file is still used");
}
