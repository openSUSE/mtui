//! Integration tests for remote history append (mtui-rs-0mop.5).
//!
//! `Target::add_history` records one `/var/log/mtui.log` line per call via the
//! `Connection::sftp_append` primitive instead of the former
//! read-concatenate-rewrite emulation. These tests pin the performance-relevant
//! contract offline against [`MockConnection`]:
//!
//! - the per-entry cost is **one append, zero reads/overwrites**, and stays flat
//!   as the history grows (the 0mop.5 "append call-count" oracle);
//! - the wire format and ordering are preserved byte-for-byte across entries;
//! - fan-out issues exactly one append per member host;
//! - an append failure is swallowed best-effort (bookkeeping never aborts the
//!   recorded operation).

use mtui_hosts::connection::Connection;
use mtui_hosts::{HostsGroup, MockConnection, MockSftpOp, Target};
use mtui_types::enums::{ExecutionMode, TargetState};

const LOG: &str = "/var/log/mtui.log";

fn enabled(conn: MockConnection) -> Target {
    Target::with_connection(
        "h1",
        TargetState::Enabled,
        ExecutionMode::Parallel,
        Box::new(conn),
    )
}

/// Recording N entries issues exactly N appends and never a read/overwrite —
/// the cost per entry is O(1), independent of how large the history already is.
#[tokio::test]
async fn add_history_is_one_append_per_entry_regardless_of_size() {
    // Pre-seed a large existing history: the old emulation would download and
    // re-upload all of it on every entry; append must not read it at all.
    let mut prior = Vec::new();
    for i in 0..10_000 {
        prior.extend_from_slice(format!("{i}:seed:noop\n").as_bytes());
    }
    let conn = MockConnection::new("h1").with_file(LOG, prior.clone());
    let handle = conn.clone();
    let mut t = enabled(conn);

    for i in 0..5 {
        t.add_history(&["install".to_owned(), format!("pkg-{i}")])
            .await;
    }

    // Exactly one append per entry, no `Open` (read) or `Write` (overwrite).
    let ops = handle.sftp_ops();
    assert_eq!(ops.len(), 5, "one op per entry: {ops:?}");
    assert!(
        ops.iter()
            .all(|op| matches!(op, MockSftpOp::Append(p) if p.as_os_str() == LOG)),
        "every op is an append to the log: {ops:?}"
    );

    // The seed is preserved and the five entries are appended in order.
    let contents = String::from_utf8(handle.file_contents(LOG).unwrap()).unwrap();
    assert!(contents.starts_with(&String::from_utf8(prior).unwrap()));
    assert_eq!(
        contents.lines().count(),
        10_005,
        "seed + five appended entries"
    );
    let tail: Vec<&str> = contents.lines().rev().take(5).collect();
    for (offset, line) in tail.iter().enumerate() {
        // rev(): first is pkg-4 … last is pkg-0.
        let want = format!("install:pkg-{}", 4 - offset);
        assert!(line.ends_with(&want), "ordered append: {line:?} ~ {want}");
    }
}

/// Two independent writers against the same file both persist their entry — the
/// append has no read-modify-write window to lose an update in.
#[tokio::test]
async fn concurrent_appenders_do_not_lose_entries() {
    // `MockConnection`'s file store is an `Arc<Mutex<..>>` shared across clones,
    // so two clones model two independent writers against one host's
    // /var/log/mtui.log (mirrors a Rust and a Python mtui sharing the fleet).
    let handle = MockConnection::new("h1");
    let mut alice = handle.clone();
    let mut bob = handle.clone();

    alice
        .sftp_append(std::path::Path::new(LOG), b"1:alice:install\n")
        .await
        .expect("alice append");
    bob.sftp_append(std::path::Path::new(LOG), b"2:bob:downgrade\n")
        .await
        .expect("bob append");

    let contents = String::from_utf8(handle.file_contents(LOG).unwrap()).unwrap();
    assert!(
        contents.contains("1:alice:install\n"),
        "alice kept: {contents:?}"
    );
    assert!(
        contents.contains("2:bob:downgrade\n"),
        "bob kept: {contents:?}"
    );
    assert_eq!(contents.lines().count(), 2, "no lost entry: {contents:?}");
}

/// A missing history file is created by the first append (O_CREAT).
#[tokio::test]
async fn add_history_creates_missing_log() {
    let conn = MockConnection::new("h1");
    let handle = conn.clone();
    let mut t = enabled(conn);

    assert!(handle.file_contents(LOG).is_none(), "no log yet");
    t.add_history(&["install".to_owned(), "pkg".to_owned()])
        .await;
    assert!(
        handle.file_contents(LOG).is_some(),
        "append created the log"
    );
}

/// An append failure is swallowed: bookkeeping never aborts the operation it
/// records, matching upstream's best-effort history write.
#[tokio::test]
async fn add_history_swallows_append_failure() {
    let conn = MockConnection::new("h1").with_sftp_append_error(LOG);
    let handle = conn.clone();
    let mut t = enabled(conn);

    // Must not panic / propagate.
    t.add_history(&["install".to_owned(), "pkg".to_owned()])
        .await;

    // The attempt was made (one append op), but nothing persisted.
    assert_eq!(handle.sftp_ops().len(), 1);
    assert!(handle.file_contents(LOG).is_none());
}

/// Fan-out records exactly one append per member host.
#[tokio::test]
async fn hostsgroup_add_history_appends_once_per_host() {
    let c1 = MockConnection::new("h1");
    let c2 = MockConnection::new("h2");
    let (m1, m2) = (c1.clone(), c2.clone());
    let t1 = enabled(c1);
    let t2 = Target::with_connection(
        "h2",
        TargetState::Enabled,
        ExecutionMode::Parallel,
        Box::new(c2),
    );
    let mut group = HostsGroup::new(vec![t1, t2], false);

    group
        .add_history(&["install".to_owned(), "pkg".to_owned()])
        .await;

    for m in [&m1, &m2] {
        let ops = m.sftp_ops();
        assert_eq!(ops.len(), 1, "one append per host: {ops:?}");
        assert!(matches!(&ops[0], MockSftpOp::Append(p) if p.as_os_str() == LOG));
    }
}
