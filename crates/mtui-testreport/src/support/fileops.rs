//! File and time helpers shared by the exporters.
//!
//! * `timestamp` — a whole-second Unix timestamp string, used as a
//!   filename suffix when the user declines to overwrite an existing export.
//! * [`atomic_write_file`] — a thin wrapper over [`mtui_config::atomic::write`],
//!   the single secure temp-file + rename implementation shared across the
//!   workspace.

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as a whole-second Unix timestamp string.
///
/// A whole-second Unix timestamp: the fractional part is dropped. Used to
/// build a unique filename suffix for exports the user chose not to
/// overwrite.
#[must_use]
pub(crate) fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// Atomically writes `data` to `path`.
///
/// Delegates to [`mtui_config::atomic::write`], the single secure temp-file +
/// rename implementation: it creates the destination directory first (a cache
/// location may be absent on a fresh checkout), writes to a unique
/// same-directory temp opened `create_new` + `0o600`, fsyncs, then renames — so
/// no reader sees a half-written file and no precreated symlink is followed.
///
/// # Errors
///
/// Returns any I/O error from creating the directory, writing the temp file, or
/// renaming it into place.
pub fn atomic_write_file(data: &[u8], path: &Path) -> io::Result<()> {
    mtui_config::atomic::write(data, path)
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
