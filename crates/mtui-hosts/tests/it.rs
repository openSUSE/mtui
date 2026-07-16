//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its heavy deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.

#[path = "fanout_spinner_production_runtime.rs"]
mod fanout_spinner_production_runtime;
#[path = "fanout_spinner_visible.rs"]
mod fanout_spinner_visible;
#[path = "lock_format.rs"]
mod lock_format;
#[path = "operation_group.rs"]
mod operation_group;
#[path = "sftp_traversal.rs"]
mod sftp_traversal;
#[path = "spinner_concurrent_logging.rs"]
mod spinner_concurrent_logging;
#[path = "ssh_integration.rs"]
mod ssh_integration;
#[path = "target_parsers.rs"]
mod target_parsers;
