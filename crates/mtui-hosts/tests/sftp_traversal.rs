//! Malicious-server integration test for SFTP folder downloads.
//!
//! A remote peer controls the directory-entry names returned by `read_dir`.
//! `Connection::sftp_get_folder` concatenates each name into a *local* write
//! path (`<local><name>.<hostname>`); a crafted name (`../evil`, `/etc/passwd`,
//! `a/b`) would escape the download destination and overwrite arbitrary local
//! files (path traversal). This drives the folder download through the real
//! [`Target::sftp_get`] dispatch against a hostile [`MockConnection`] listing
//! and asserts that every write lands at the single intended
//! `<local_dir>/<name>.<host>` shape — no traversal, no absolute escape, no
//! nested separator.

use mtui_hosts::{MockConnection, Target};
use mtui_types::enums::TargetState;

const HOST: &str = "refhost1";
const REMOTE_DIR: &str = "/var/log/updates";
const LOCAL_DIR: &str = "/tmp/mtui-dl";

/// The mock's `files` table doubles as an observable filesystem: the folder
/// download copies `files["<remote>/<name>"]` into
/// `files["<local>/<name>.<host>"]` for each *accepted* entry, and never writes
/// a rejected entry. Runs the download and returns a handle sharing that table.
async fn run_folder_download(entries: &[&str]) -> MockConnection {
    let mut conn = MockConnection::new(HOST).with_listing(REMOTE_DIR, entries.to_vec());
    // Seed a source blob for every advertised entry so an *accepted* one has
    // bytes to copy (rejected ones are skipped before any read).
    for name in entries {
        conn = conn.with_file(
            format!("{REMOTE_DIR}/{name}"),
            format!("data:{name}").into_bytes(),
        );
    }
    let handle = conn.clone();

    let mut target = Target::with_connection(HOST, TargetState::Enabled, Box::new(conn));
    // Trailing `/` selects the folder branch; `Target::sftp_get` appends `/` to
    // `local` before calling `sftp_get_folder`.
    target
        .sftp_get(&format!("{REMOTE_DIR}/"), std::path::Path::new(LOCAL_DIR))
        .await;
    handle
}

/// A local key was written iff the mock recorded a target under
/// `<LOCAL_DIR>/<name>.<HOST>`. The folder branch passes `local` reformatted to
/// end with `/`, so the download prefix is exactly `<LOCAL_DIR>/`.
fn was_written(handle: &MockConnection, name: &str) -> bool {
    let key = format!("{LOCAL_DIR}/{name}.{HOST}");
    handle.file_contents(std::path::Path::new(&key)).is_some()
}

/// Every download-target key present in the mock filesystem (i.e. keys under the
/// local download prefix, distinguishable from the seeded remote sources which
/// live under `REMOTE_DIR`).
fn download_targets(handle: &MockConnection) -> Vec<String> {
    let prefix = format!("{LOCAL_DIR}/");
    handle
        .file_paths()
        .into_iter()
        .filter_map(|p| p.to_str().map(str::to_owned))
        .filter(|k| k.starts_with(&prefix))
        .collect()
}

#[tokio::test]
async fn folder_download_rejects_traversal_and_absolute_entries() {
    let handle = run_folder_download(&[
        "good.log",
        "../evil",
        "../../etc/cron.d/x",
        "/etc/passwd",
        "a/b",
        "..",
        ".",
    ])
    .await;

    // The one legitimate entry landed under the destination.
    assert!(
        was_written(&handle, "good.log"),
        "legitimate entry must be written"
    );

    // The ONLY download target is the legitimate one — no hostile name produced
    // a write anywhere. This is the trust-boundary guarantee.
    let mut targets = download_targets(&handle);
    targets.sort();
    assert_eq!(
        targets,
        vec![format!("{LOCAL_DIR}/good.log.{HOST}")],
        "no traversal/absolute/nested entry may produce a write",
    );

    // Explicit spot-checks on the specific escape shapes.
    for escaped in [
        format!("{LOCAL_DIR}/../evil.{HOST}"),
        format!("../evil.{HOST}"),
        format!("{LOCAL_DIR}/../../etc/cron.d/x.{HOST}"),
        format!("{LOCAL_DIR}/a/b.{HOST}"),
    ] {
        assert!(
            handle
                .file_contents(std::path::Path::new(&escaped))
                .is_none(),
            "traversal target {escaped:?} must not have been written",
        );
    }
}

#[tokio::test]
async fn folder_download_accepts_ordinary_and_unicode_names() {
    let handle =
        run_folder_download(&["good.log", "second.log", ".hidden", "Ünïcode.txt", "a b c"]).await;

    for name in ["good.log", "second.log", ".hidden", "Ünïcode.txt", "a b c"] {
        assert!(
            was_written(&handle, name),
            "ordinary name {name:?} should be written"
        );
    }
    assert_eq!(download_targets(&handle).len(), 5);
}
