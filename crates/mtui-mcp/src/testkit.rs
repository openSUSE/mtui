//! Fixtures shared by the crate's unit tests.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use clap::ArgMatches;
use mtui_config::Config;
use mtui_core::{Command, CommandResult, Registry, Scope, Session, register_all};
use tokio::sync::oneshot;

use crate::otel::QueuedAudit;

/// A default config for the user the tests' `whoami` banners expect.
pub(crate) fn test_config() -> Config {
    let mut config = Config::default();
    config.session_user = "testuser".to_owned();
    config
}

/// The real command set plus `probe`.
pub(crate) fn registry_with(probe: impl Command + 'static) -> Arc<Registry> {
    let mut registry = register_all();
    registry.register(Arc::new(probe));
    Arc::new(registry)
}

/// A test command that parks without observing cancellation, so only a forced
/// abort ends it.
pub(crate) struct Probe {
    name: &'static str,
    scope: Scope,
    hold: Option<Duration>,
    started: StdMutex<Option<oneshot::Sender<()>>>,
}

impl Probe {
    /// Never finishes.
    pub(crate) fn forever(name: &'static str) -> Self {
        Self {
            name,
            scope: Scope::Fanout,
            hold: None,
            started: StdMutex::new(None),
        }
    }

    /// Blocked mid host-op: announces on `started`, then sleeps far past any
    /// grace period without checking the cancel token.
    pub(crate) fn stubborn(name: &'static str, started: oneshot::Sender<()>) -> Self {
        Self {
            name,
            scope: Scope::Fanout,
            hold: Some(Duration::from_secs(600)),
            started: StdMutex::new(Some(started)),
        }
    }

    /// Session-level rather than fan-out, so the worker dispatches on a fork
    /// and does not hold the canonical session mutex.
    pub(crate) fn session_level(mut self) -> Self {
        self.scope = Scope::Single;
        self
    }
}

#[async_trait::async_trait]
impl Command for Probe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn scope(&self) -> Scope {
        self.scope
    }

    fn reads_resolved_report(&self) -> bool {
        !matches!(self.scope, Scope::Single)
    }

    async fn call(&self, _session: &mut Session, _args: &ArgMatches) -> CommandResult {
        if let Some(tx) = self.started.lock().expect("probe channel poisoned").take() {
            let _ = tx.send(());
        }
        match self.hold {
            Some(hold) => tokio::time::sleep(hold).await,
            None => std::future::pending::<()>().await,
        }
        Ok(())
    }
}

/// A queued audit record for a successful `run` call with sequence number `seq`.
pub(crate) fn queued_audit(seq: u64) -> QueuedAudit {
    QueuedAudit {
        seq,
        jsonl: "{}".to_owned(),
        tool: "run".to_owned(),
        outcome: "ok".to_owned(),
        event: "call".to_owned(),
        transport: "stdio".to_owned(),
        session_id: 1,
        response_bytes: None,
        trace: None,
        time_nanos: 0,
        extra_attrs: Vec::new(),
    }
}
