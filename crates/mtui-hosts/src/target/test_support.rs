//! Fixtures shared by the unit tests of the `target` modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mtui_config::Config;
use mtui_types::enums::TargetState;

use super::{Clock, Target};
use crate::connection::MockConnection;

/// A fake clock: fixed `now`, a manually-advanceable monotonic counter, and a
/// no-op sleep that advances the monotonic clock by the slept amount, so wait
/// budgets elapse instantly.
#[derive(Clone)]
pub(crate) struct FakeClock {
    now: u64,
    mono: Arc<AtomicU64>, // milliseconds
}

impl FakeClock {
    pub(crate) fn new(now: u64) -> Self {
        Self {
            now,
            mono: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl Clock for FakeClock {
    fn now_unix(&self) -> u64 {
        self.now
    }
    fn monotonic(&self) -> f64 {
        self.mono.load(Ordering::SeqCst) as f64 / 1000.0
    }
    async fn sleep(&self, secs: f64) {
        self.mono
            .fetch_add((secs * 1000.0) as u64, Ordering::SeqCst);
    }
}

/// The default config with a fixed session user.
pub(crate) fn cfg() -> Config {
    let mut c = Config::default();
    c.session_user = "testuser".into();
    c
}

/// An enabled target over `conn`.
pub(crate) fn enabled_with(conn: MockConnection) -> Target {
    Target::with_connection(
        "test-host.example.com",
        TargetState::Enabled,
        Box::new(conn),
    )
}
