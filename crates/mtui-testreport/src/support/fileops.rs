//! File and time helpers shared by the exporters.
//!
//! Ports the pieces of upstream `mtui.support.fileops` that the export
//! subsystem depends on:
//!
//! * [`timestamp`] — a whole-second Unix timestamp string (upstream
//!   `str(int(time.time()))`), used as a filename suffix when the user declines
//!   to overwrite an existing export.
//! * [`atomic_write_file`] — a temp-file + rename write that first ensures the
//!   destination directory exists (upstream `atomic_write_file`).
//!
//! The refhost resolver already carries a private `AtomicFileWriter` with the
//! same semantics, but it is coupled to `RefhostError`; per the crate-boundary
//! decision for Phase 4 the export subsystem keeps its own small copy rather
//! than promoting that type to a cross-crate public API.

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as a whole-second Unix timestamp string.
///
/// Mirrors upstream `timestamp()` (`str(int(time.time()))`): the fractional
/// part is dropped. Used to build a unique filename suffix for exports the user
/// chose not to overwrite.
#[must_use]
pub fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// Atomically writes `data` to `path` via a sibling temp file + rename.
///
/// The destination directory is created first (upstream comment: cache
/// locations such as `~/.cache/mtui` may be absent on a fresh checkout).
/// Writing to a temp file and renaming into place means a reader never observes
/// a half-written file.
///
/// # Errors
///
/// Returns any I/O error from creating the directory, writing the temp file, or
/// renaming it into place.
pub fn atomic_write_file(data: &[u8], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_all_digits_and_nonempty() {
        let ts = timestamp();
        assert!(!ts.is_empty());
        assert!(ts.chars().all(|c| c.is_ascii_digit()), "got {ts:?}");
    }

    #[test]
    fn atomic_write_creates_parent_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("out.txt");
        atomic_write_file(b"hello", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        // No leftover temp file beside the destination.
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        atomic_write_file(b"first", &path).unwrap();
        atomic_write_file(b"second", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }
}
