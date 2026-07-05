//! `mtui-hosts` — SSH/SFTP host layer (russh), host groups, locks, targets.
//!
//! Phase 2 builds this crate up incrementally. The current surface is the
//! [`Connection`] abstraction plus its scriptable [`MockConnection`] test
//! double and the [`HostError`] hierarchy; the russh-backed implementation,
//! `Target` state machine, `HostsGroup` fan-out, locks, and arbiter land in
//! subsequent Phase 2 tasks.

pub mod connection;
pub mod error;

pub use connection::{CommandTimeout, Connection, HostKeyPolicy, MockConnection};
pub use error::{HostError, Result};
