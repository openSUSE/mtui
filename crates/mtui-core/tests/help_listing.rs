//! Snapshot of the `help` command's no-arg listing — the REPL command-surface
//! contract.
//!
//! This golden output changes whenever a command is added/removed or gains an
//! [`about`](mtui_core::Command::about) (moving between the documented and
//! undocumented buckets). Such a change is a deliberate command-surface change
//! and must be reviewed via `cargo insta review` — it is not a regression.

use std::sync::{Arc, Mutex};

use mtui_config::Config;
use mtui_core::{ColorMode, CommandPromptDisplay, Session, dispatch_line, register_all};

/// A `Write` sink backed by a shared buffer so the test can read the output.
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn help_listing_is_the_command_surface_contract() {
    let registry = register_all();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let display =
        CommandPromptDisplay::with_sink(Box::new(SharedBuf(Arc::clone(&buf))), ColorMode::Never);
    let mut session = Session::with_display(Config::default(), true, display);

    dispatch_line(&registry, &mut session, "help")
        .await
        .expect("help should render");

    let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    insta::assert_snapshot!("help_listing", out);
}

/// Every registered command must document itself via
/// [`about`](mtui_core::Command::about). This keeps the `help` listing's
/// "Undocumented commands" bucket empty (matching upstream, whose commands all
/// carry a docstring) and guarantees each command contributes a description to
/// the MCP tool synthesiser in Phase 7.
#[test]
fn every_registered_command_is_documented() {
    let registry = register_all();
    let undocumented: Vec<&str> = registry
        .names()
        .filter(|name| registry.get(name).and_then(|c| c.about()).is_none())
        .collect();
    assert!(
        undocumented.is_empty(),
        "these commands are missing an about() one-liner: {undocumented:?}"
    );
}
