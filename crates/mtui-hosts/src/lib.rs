//! `mtui-hosts` — SSH/SFTP host layer (russh), host groups, locks, targets.
//!
//! The crate provides the [`Connection`] abstraction with the russh-backed
//! [`SshConnection`] and scriptable [`MockConnection`], the [`HostError`] hierarchy,
//! the [`Target`] state machine, the [`HostsGroup`] parallel/serial fan-out, the
//! remote-lock protocol ([`TargetLock`] / [`RemoteLock`]), the in-process
//! [`HostArbiter`], host-output parsers ([`parse_system`],
//! [`parse_product`](target::parsers::product::parse_product),
//! [`parse_os_release`](target::parsers::product::parse_os_release)), the
//! [`PackageQuerier`], and the install/uninstall [`Operation`] template over the
//! [`OperationGroup`] seam. Behind the `shell` feature, `Connection::shell` /
//! `Target::shell` return a `ShellChannel` PTY duplex; the local terminal bridge is
//! a CLI concern.

pub mod connection;
pub mod error;
pub mod prompter;
pub mod target;

#[cfg(feature = "shell")]
pub use connection::ShellChannel;
pub use connection::{
    CommandTimeout, Connection, HostKeyPolicy, MAX_STREAM_BYTES, MockConnection, MockEvent,
    MockSftpOp, SftpRead, SshConnection, TimeoutPrompt,
};
pub use error::{HostError, LockRefusal, Result, UploadRefusal};
pub use prompter::{Prompter, Reader};
pub use target::{
    Check, CheckArgs, CheckFailure, Clock, Command, ContendedSurface, Doer, HostArbiter,
    HostOutput, HostPlan, HostsGroup, LockOutcome, LockOwner, LostHold, LostHoldKind,
    LostHoldProof, OpLockHolders, Operation, OperationGroup, OperationKind, OperationReport, Owner,
    POOL_LOCK_PATH, PackageQuerier, PlanProvider, RebootFailure, RebootFailureCause, RebootReport,
    RemoteLock, RepoFailure, RepoManager, RepoOp, RepoOutcome, SetRepo, Sink, SpinnerGuard,
    Suspend, SystemClock, TARGET_LOCK_PATH, Target, TargetLock, Transactional, TtySpinner,
    contended_lock_reason, get_arbiter, parse_system, set_test_sink, sibling_holder_name, spinner,
    suspend,
};
