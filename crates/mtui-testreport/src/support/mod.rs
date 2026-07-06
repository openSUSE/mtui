//! Shared support helpers for the testreport export subsystem.
//!
//! Ports the slices of upstream `mtui.support.*` and `mtui.types.filelist` that
//! the exporters depend on, kept local to `mtui-testreport` to avoid widening a
//! cross-crate public surface (see Phase 4 crate-boundary decision).

pub mod filelist;
pub mod fileops;
pub mod sysinfo;

pub use filelist::FileList;
pub use fileops::{atomic_write_file, timestamp};
pub use sysinfo::{EXPORT_PREFIX, detect_system, system_info};
