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
//! [`PackageQuerier`] (P2.8), and the install/uninstall [`Operation`] template
//! (skeleton + trait, P2.9) — the `lock → run → check → reboot → unlock`
//! flow driven over the object-safe [`OperationGroup`] seam — and, behind the
//! `shell` feature, the interactive PTY shell (P2.10): `Connection::shell` /
//! `Target::shell` returning an object-safe `ShellChannel` duplex over the
//! remote PTY. Only the transport primitive lives here; the raw-`termios` local
//! terminal bridge and the `shell` REPL command that consume it are a CLI
//! concern (Phase 6).

pub mod connection;
pub mod error;
pub mod target;

#[cfg(feature = "shell")]
pub use connection::ShellChannel;
pub use connection::{
    CommandTimeout, Connection, HostKeyPolicy, MockConnection, MockSftpOp, SshConnection,
};
pub use error::{HostError, Result};
pub use target::{
    Check, CheckArgs, Clock, Command, Doer, HostArbiter, HostPlan, HostsGroup, InstallOperation,
    LastOutput, Lockable, Operation, OperationGroup, Owner, POOL_LOCK_PATH, PackageQuerier,
    PlanProvider, PoolLock, RemoteLock, RepoManager, RepoOp, RunCommand, SetRepo, SystemClock,
    TARGET_LOCK_PATH, Target, TargetLock, UninstallOperation, get_arbiter, parse_os_release,
    parse_product, parse_system, run_parallel, sftp_get_all, sftp_put_all, sftp_remove_all,
    with_locked,
};
