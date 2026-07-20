//! Per-target reporting collaborator.
//!
//! ## Reference
//!
//! Ported from upstream `mtui/hosts/target/reporter.py` (`Reporter`). Upstream
//! extracts the seven status sink-dispatch methods that used to live directly on
//! [`Target`] (`report_self`, `report_history`, `report_locks`,
//! `report_timeout`, `report_sessions`, `report_log`, `report_products`) into
//! one collaborator so [`Target`] can stay focused on the connection/lock
//! skeleton. Each method reads the target's most up-to-date values *at call
//! time* and forwards them to a caller-supplied sink; the sink is where the
//! display formatting lives (the REPL `list_*` / `show_log` / `list_products`
//! renderers). Method names drop the `report_` prefix — inside this collaborator
//! it is redundant — and `report_self` becomes [`self_`](Reporter::self_) to
//! avoid clashing with the `self` keyword.
//!
//! ## Sinks are closures
//!
//! Upstream passes plain callables. The Rust port takes a generic closure per
//! method (`impl FnOnce(...)`) with the exact upstream argument tuple, so a
//! sink is zero-cost and easy to unit-test with a captured accumulator. The
//! `Reporter` borrows its [`Target`] for the duration of the call (the Rust
//! analogue of upstream's per-access `Target.reporter` property, which allocates
//! a fresh binding each time), so every dispatch sees live field values.
//!
//! ## Lock sinks
//!
//! Upstream also exposes `report_locks` and `report_pool_locks`, which forward
//! the target's stored `_lock` / `_pool_lock` objects to a sink. Those lock
//! accessors are async `&mut self` in `mtui-hosts` (they read the remote
//! lockfile over SFTP), whereas a [`Reporter`] borrows its [`Target`]
//! immutably. So the async resolution is done up front by
//! [`Target::lock_status`](super::Target::lock_status) — producing a resolved
//! [`LockRow`] — and the [`locks`](Reporter::locks) sink forwards
//! `(hostname, system, &LockRow)` synchronously, mirroring upstream's
//! `report_locks` / `report_pool_locks` (the `pool` variant differs only in
//! which lock [`Target::lock_status`] resolves). The group driver
//! [`HostsGroup::report_locks`](super::HostsGroup::report_locks) sequences the
//! per-host resolve-then-forward.

use mtui_types::enums::{ExecutionMode, TargetState};
use mtui_types::hostlog::HostLog;
use mtui_types::system::System;

use super::{LockRow, Target};

/// Adapter that drives the per-target status sinks for one [`Target`].
///
/// Obtain one via [`Target::reporter`]; it borrows the target so every sink
/// dispatch reads the most up-to-date values.
pub struct Reporter<'a> {
    target: &'a Target,
}

impl<'a> Reporter<'a> {
    /// Binds a reporter to `target`.
    ///
    /// Prefer [`Target::reporter`] over calling this directly.
    #[must_use]
    pub(super) fn new(target: &'a Target) -> Self {
        Self { target }
    }

    /// Reports `(hostname, system, transactional, state, mode)` to `sink`.
    ///
    /// Ports `Reporter.self_` (upstream `report_self`): the full per-host status
    /// tuple used by the `list_host` renderer.
    pub fn self_<F>(&self, sink: F)
    where
        F: FnOnce(&str, &System, bool, TargetState, ExecutionMode),
    {
        let t = self.target;
        sink(
            t.hostname(),
            t.system(),
            t.transactional(),
            t.state(),
            t.mode(),
        );
    }

    /// Reports the last stdout split on `\n` to `sink`.
    ///
    /// Ports `Reporter.history` (upstream `report_history`): forwards
    /// `(hostname, system, lastout().split('\n'))`. An empty log yields a
    /// single empty-string element, matching upstream's `"".split("\n")`.
    pub fn history<F>(&self, sink: F)
    where
        F: FnOnce(&str, &System, Vec<&str>),
    {
        let t = self.target;
        sink(t.hostname(), t.system(), t.lastout().split('\n').collect());
    }

    /// Reports the current connection timeout (in whole seconds) to `sink`.
    ///
    /// Ports `Reporter.timeout` (upstream `report_timeout`). Upstream reads
    /// `connection.timeout`; the Rust [`Target`] owns the timeout itself
    /// (defaulted from config), which is the same value, so this reports
    /// [`Target`]'s field as a `u64` count of seconds to match upstream's `int`.
    pub fn timeout<F>(&self, sink: F)
    where
        F: FnOnce(&str, &System, u64),
    {
        let t = self.target;
        sink(t.hostname(), t.system(), t.timeout_secs());
    }

    /// Reports the last stdout verbatim to `sink`.
    ///
    /// Ports `Reporter.sessions` (upstream `report_sessions`): used by the
    /// `who`-style session listing, which needs the raw stdout string rather
    /// than the newline-split form [`history`](Self::history) produces.
    pub fn sessions<F>(&self, sink: F)
    where
        F: FnOnce(&str, &System, &str),
    {
        let t = self.target;
        sink(t.hostname(), t.system(), t.lastout());
    }

    /// Reports the full host log plus a caller-provided extra to `sink`.
    ///
    /// Ports `Reporter.log` (upstream `report_log`): forwards `(hostname, out,
    /// arg)`. `arg` is the caller's extra (upstream typically an output
    /// accumulator); it is generic so a caller can pass, e.g., `&mut Vec<_>`.
    pub fn log<F, A>(&self, sink: F, arg: A)
    where
        F: FnOnce(&str, &HostLog, A),
    {
        let t = self.target;
        sink(t.hostname(), t.out(), arg);
    }

    /// Reports `(hostname, system)` to `sink`.
    ///
    /// Ports `Reporter.products` (upstream `report_products`): the minimal
    /// two-tuple sink behind the `list_products` renderer.
    pub fn products<F>(&self, sink: F)
    where
        F: FnOnce(&str, &System),
    {
        let t = self.target;
        sink(t.hostname(), t.system());
    }

    /// Reports `(hostname, system, &row)` to `sink`.
    ///
    /// Ports `Reporter.locks` / `Reporter.pool_locks` (upstream `report_locks` /
    /// `report_pool_locks`). Upstream forwards the live lock object; the Rust
    /// port forwards an already-resolved [`LockRow`] (see
    /// [`Target::lock_status`](super::Target::lock_status)) because the lock
    /// accessors are async while a [`Reporter`] borrows its [`Target`]
    /// immutably. The operation-vs-pool distinction lives in how the caller
    /// resolves `row`, not here, so a single sink covers both upstream methods.
    pub fn locks<F>(&self, row: &LockRow, sink: F)
    where
        F: FnOnce(&str, &System, &LockRow),
    {
        let t = self.target;
        sink(t.hostname(), t.system(), row);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use mtui_types::enums::{ExecutionMode, TargetState};
    use mtui_types::hostlog::CommandLog;

    use crate::connection::MockConnection;
    use crate::target::Target;

    /// Builds an enabled target over a mock connection, ready to `run` and then
    /// be reported on.
    fn target_with(conn: MockConnection) -> Target {
        Target::with_connection(
            "test-host.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
            Box::new(conn),
        )
    }

    async fn ran(command: &str, stdout: &str) -> Target {
        let conn = MockConnection::new("test-host.example.com")
            .with_response(command, CommandLog::new(command, stdout, "", 0, 0));
        let mut t = target_with(conn);
        t.run(command).await;
        t
    }

    #[test]
    fn reporter_borrows_target_and_reads_live_fields() {
        // The reporter reflects the target's current state, not a snapshot at
        // construction — mirrors upstream's fresh-binding property.
        let mut t = Target::new(
            &mtui_config::Config::default(),
            "h.example.com",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        t.set_state(TargetState::Disabled);
        let captured = RefCell::new(None);
        t.reporter()
            .self_(|_, _, _, state, _| *captured.borrow_mut() = Some(state));
        assert_eq!(captured.into_inner(), Some(TargetState::Disabled));
    }

    #[test]
    fn self_forwards_full_status_tuple() {
        let t = Target::new(
            &mtui_config::Config::default(),
            "test-host.example.com",
            TargetState::Dryrun,
            ExecutionMode::Serial,
        );
        let out = RefCell::new(None);
        t.reporter()
            .self_(|host, _system, transactional, state, mode| {
                *out.borrow_mut() = Some((host.to_owned(), transactional, state, mode));
            });
        assert_eq!(
            out.into_inner(),
            Some((
                "test-host.example.com".to_owned(),
                false,
                TargetState::Dryrun,
                ExecutionMode::Serial,
            ))
        );
    }

    #[tokio::test]
    async fn history_splits_lastout_on_newline() {
        let t = ran("cmd", "line1\nline2").await;
        let out = RefCell::new(Vec::new());
        t.reporter().history(|host, _system, lines| {
            assert_eq!(host, "test-host.example.com");
            *out.borrow_mut() = lines.iter().map(|s| s.to_string()).collect();
        });
        assert_eq!(out.into_inner(), vec!["line1", "line2"]);
    }

    #[test]
    fn history_empty_log_yields_single_empty_string() {
        // Upstream `"".split("\n")` == `[""]`; Rust `"".split('\n')` matches.
        let t = Target::new(
            &mtui_config::Config::default(),
            "h",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        let out = RefCell::new(Vec::new());
        t.reporter().history(|_, _, lines| {
            *out.borrow_mut() = lines.iter().map(|s| s.to_string()).collect();
        });
        assert_eq!(out.into_inner(), vec![""]);
    }

    #[tokio::test]
    async fn sessions_forwards_full_lastout_string() {
        let t = ran("who", "alice tty1\n").await;
        let out = RefCell::new(String::new());
        t.reporter().sessions(|host, _system, last| {
            assert_eq!(host, "test-host.example.com");
            *out.borrow_mut() = last.to_owned();
        });
        assert_eq!(out.into_inner(), "alice tty1\n");
    }

    #[test]
    fn timeout_reports_target_timeout_seconds() {
        let mut c = mtui_config::Config::default();
        c.connection_timeout = 42;
        let t = Target::new(&c, "h", TargetState::Enabled, ExecutionMode::Parallel);
        let out = RefCell::new(None);
        t.reporter().timeout(|host, _system, secs| {
            assert_eq!(host, "h");
            *out.borrow_mut() = Some(secs);
        });
        assert_eq!(out.into_inner(), Some(42));
    }

    #[tokio::test]
    async fn log_passes_full_outlog_and_extra_arg() {
        let t = ran("echo hi", "hi\n").await;
        let mut sink_out: Vec<String> = Vec::new();
        t.reporter().log(
            |host, hostlog, acc: &mut Vec<String>| {
                assert_eq!(host, "test-host.example.com");
                assert_eq!(hostlog.len(), 1);
                acc.push("extra".to_owned());
            },
            &mut sink_out,
        );
        assert_eq!(sink_out, vec!["extra"]);
    }

    #[test]
    fn products_forwards_hostname_and_system() {
        let t = Target::new(
            &mtui_config::Config::default(),
            "prod-host",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        let out = RefCell::new(None);
        t.reporter().products(|host, system| {
            *out.borrow_mut() = Some((host.to_owned(), system.get_base().name.clone()));
        });
        let (host, base) = out.into_inner().expect("sink called");
        assert_eq!(host, "prod-host");
        assert_eq!(base, "unknown");
    }

    #[test]
    fn locks_forwards_hostname_system_and_row() {
        use crate::target::LockRow;
        let t = Target::new(
            &mtui_config::Config::default(),
            "lock-host",
            TargetState::Enabled,
            ExecutionMode::Parallel,
        );
        let row = LockRow {
            is_locked: true,
            is_mine: true,
            time: "some time".to_owned(),
            ..LockRow::default()
        };
        let out = RefCell::new(None);
        t.reporter().locks(&row, |host, _system, got| {
            *out.borrow_mut() = Some((host.to_owned(), got.clone()));
        });
        let (host, got) = out.into_inner().expect("sink called");
        assert_eq!(host, "lock-host");
        assert_eq!(got, row);
    }
}
