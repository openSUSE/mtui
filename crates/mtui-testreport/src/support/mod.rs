//! Shared support helpers for the testreport export subsystem.
//!
//! Kept local to `mtui-testreport` to avoid widening a cross-crate public
//! surface (see Phase 4 crate-boundary decision).

pub mod filelist;
pub mod fileops;
pub mod sysinfo;

pub use filelist::FileList;
pub use fileops::atomic_write_file;
pub use sysinfo::{detect_system, system_info};
