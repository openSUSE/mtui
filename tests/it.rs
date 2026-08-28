//! Consolidated integration-test entry point for the facade package.
//!
//! Each binary's smoke test is its own module, gated behind the feature that
//! builds it (`cli` for `mtui`, `mcp` for `mtui-mcp`), so a
//! `--no-default-features --features <one>` build still runs only the
//! matching test.

#[path = "cli_smoke.rs"]
mod cli_smoke;
#[path = "doc_targets.rs"]
mod doc_targets;
#[path = "mcp_version.rs"]
mod mcp_version;
