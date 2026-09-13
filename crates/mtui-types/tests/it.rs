//! Consolidated integration-test entry point.
//!
//! Every integration test in this crate is compiled into this single binary
//! (see `autotests = false` + `[[test]] name = "it"` in Cargo.toml) so the
//! crate + its dev-deps are linked once, not once per file. Add new
//! integration tests as a module here, not as a new top-level `tests/*.rs`.

#[path = "refhost.rs"]
mod refhost;
#[path = "rpmver.rs"]
mod rpmver;
#[path = "rrid.rs"]
mod rrid;
#[path = "updateid.rs"]
mod updateid;
