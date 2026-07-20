//! Per-client session isolation for the http transport (P7.10, mtui-rs-76e.10).
//!
//! Under `--transport http` one process serves many clients, and each must see
//! **only its own** loaded template + SSH `targets`; sharing one session would
//! let one client's `load_template` clobber another's. rmcp's streamable-HTTP
//! transport enforces this by calling [`SessionRegistry::make_server`] once per
//! new MCP session — so the correctness+security property to prove offline is:
//! **the factory mints independent sessions whose state does not bleed.**
//!
//! These are offline unit tests of the factory boundary (no live HTTP round-trip
//! in this bead — that is deferred).

#![cfg(feature = "mcp")]

use std::sync::Arc;

use mtui_config::Config;
use mtui_core::register_all;
use mtui_mcp::SessionRegistry;

fn registry(user: &str) -> SessionRegistry {
    let mut config = Config::default();
    config.session_user = user.to_owned();
    SessionRegistry::new(Arc::new(register_all()), config)
}

/// Two sessions minted from the same registry are **distinct instances** — the
/// baseline of per-client isolation (no shared `Arc<McpSession>`).
#[tokio::test]
async fn factory_mints_distinct_sessions() {
    let reg = registry("alice");

    let a = reg.make_session();
    let b = reg.make_session();

    assert!(
        !Arc::ptr_eq(&a, &b),
        "each MCP session must get its own McpSession instance"
    );
}

/// Each session's capture sink is independent: output produced on one session is
/// never observable through another's sink.
#[tokio::test]
async fn sessions_capture_output_independently() {
    let reg = registry("carol");
    let cmds = register_all();

    let a = reg.make_session();
    let b = reg.make_session();

    let out_a = a
        .run_command(&cmds, "whoami", &[])
        .await
        .expect("whoami on a");
    assert!(
        out_a.contains("User: carol"),
        "a produced output: {out_a:?}"
    );

    // b never ran a command, so its sink is empty — a's output did not leak.
    assert_eq!(
        b.output().take(),
        "",
        "b's sink must be untouched by a's run"
    );
}

/// Sessions minted from different registries carry their **own** config clone:
/// each `whoami` reflects that registry's `session_user`, proving `make_session`
/// isolates per-session config rather than sharing one instance.
#[tokio::test]
async fn each_registry_isolates_its_config() {
    let cmds = register_all();

    let dave = registry("dave").make_session();
    let erin = registry("erin").make_session();

    let out_dave = dave
        .run_command(&cmds, "whoami", &[])
        .await
        .expect("whoami on dave");
    let out_erin = erin
        .run_command(&cmds, "whoami", &[])
        .await
        .expect("whoami on erin");

    assert!(out_dave.contains("User: dave"), "got: {out_dave:?}");
    assert!(out_erin.contains("User: erin"), "got: {out_erin:?}");
}
