//! `mtui-hosts` — SSH/SFTP host layer (russh), host groups, locks, targets.
//!
//! Phase 2 builds this crate up incrementally. The current surface is the
//! [`Connection`] abstraction, the russh-backed [`SshConnection`] (connect /
//! run-with-timeout / SFTP transfers), the scriptable [`MockConnection`] test
//! double, the [`HostError`] hierarchy, the [`Target`] state machine
//! (enabled/dryrun/disabled command gating + connection-only `connect`), and the
//! [`HostsGroup`] composite with its parallel/serial command + SFTP fan-out,
//! the remote-lock protocol ([`TargetLock`] / [`PoolLock`] / [`RemoteLock`],
//! P2.6), the in-process [`HostArbiter`] (P2.7), and the host-output parsers —
//! [`parse_system`] / [`parse_product`] / [`parse_os_release`] plus the
//! [`PackageQuerier`] (P2.8). Still to come in subsequent Phase 2 tasks: the
//! interactive PTY shell (P2.10).

pub mod connection;
pub mod error;
pub mod target;

pub use connection::{
    CommandTimeout, Connection, HostKeyPolicy, MockConnection, MockSftpOp, SshConnection,
};
pub use error::{HostError, Result};
pub use target::{
    Clock, Command, HostArbiter, HostsGroup, Lockable, Owner, POOL_LOCK_PATH, PackageQuerier,
    PoolLock, RemoteLock, RunCommand, SystemClock, TARGET_LOCK_PATH, Target, TargetLock,
    get_arbiter, parse_os_release, parse_product, parse_system, run_parallel, sftp_get_all,
    sftp_put_all, sftp_remove_all, with_locked,
};
