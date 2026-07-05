//! Golden snapshot of the remote-lock wire format.
//!
//! The `/var/lock/mtui.lock` (and `/var/lock/mtui-pool.lock`) line layout is a
//! **cross-implementation contract**: a Python `mtui` and this Rust `mtui` may
//! share a host fleet, so the exact bytes must not drift. This test freezes the
//! serialized form for both the operation lock (`TargetLock`) and the pool
//! claim lock (`PoolLock`), including the pool comment that carries the RRID.
//!
//! Port of the intent behind upstream `tests/test_locks.py`'s serialization
//! assertions (`to_lockfile` / `from_lockfile`), pinned as a snapshot.

use mtui_hosts::RemoteLock;

#[test]
fn lockfile_wire_format_is_stable() {
    // Operation lock (/var/lock/mtui.lock): timestamp:user:pid, no comment.
    let op = RemoteLock {
        timestamp: "1700000000".into(),
        user: "alice".into(),
        pid: 4242,
        comment: String::new(),
    };

    // Exclusive operation lock: the same layout with a trailing comment.
    let op_exclusive = RemoteLock {
        timestamp: "1700000000".into(),
        user: "alice".into(),
        pid: 4242,
        comment: "update in progress".into(),
    };

    // Pool claim lock (/var/lock/mtui-pool.lock): the comment carries the RRID
    // as `mtui pool <RRID> [<owner>]`.
    let pool = RemoteLock {
        timestamp: "1700000000".into(),
        user: "alice".into(),
        pid: 4242,
        comment: "mtui pool SUSE:Maintenance:1:2 [alice]".into(),
    };

    let rendered = format!(
        "operation:          {}\noperation exclusive: {}\npool claim:          {}\n",
        op.to_lockfile(),
        op_exclusive.to_lockfile(),
        pool.to_lockfile(),
    );

    insta::assert_snapshot!("lockfile_wire_format", rendered);
}

#[test]
fn lockfile_roundtrip_is_lossless() {
    // A pool line (comment with embedded colons) round-trips byte-for-byte,
    // proving the parser is the exact inverse of the serializer — the property
    // the shared-fleet contract depends on.
    let line = "1700000000:alice:4242:mtui pool SUSE:Maintenance:1:2 [alice]";
    let parsed = RemoteLock::from_lockfile(line).expect("parse");
    assert_eq!(parsed.comment, "mtui pool SUSE:Maintenance:1:2 [alice]");
    assert_eq!(parsed.to_lockfile(), line);
}
