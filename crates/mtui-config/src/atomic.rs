//! Secure atomic local file writes.
//!
//! The single implementation of mtui-rs's temp-file + rename write. It is the
//! shared home for a pattern that previously existed in three drifted copies
//! (the known-hosts persist path in `mtui-hosts`, the testreport exporter in
//! `mtui-testreport`, and the refhosts cache writer in `mtui-datasources`).
//! Consolidating it here — the lowest crate that already performs filesystem
//! I/O and is depended on by all three writers — means the security guarantees
//! below can never drift between call sites again.
//!
//! ## Guarantees
//!
//! * **No symlink follow / TOCTOU.** The temp file is created in the
//!   destination's own directory with [`create_new`](std::fs::OpenOptions::create_new)
//!   and a **unique** name, so an attacker cannot pre-create it as a symlink to
//!   redirect the write outside the intended directory.
//! * **No cross-writer collision.** The temp name embeds the PID and a
//!   nanosecond timestamp, so two concurrent writers (even a Rust and a Python
//!   mtui sharing a directory) never race on the same temp path.
//! * **Restrictive permissions.** On unix the temp is opened `0o600`.
//! * **Durability.** The temp is `fsync`ed before the rename, so a crash never
//!   leaves a torn or empty destination.
//! * **Atomic replacement.** The temp lives in the same directory as the
//!   destination, so the final [`rename`](std::fs::rename) stays intra-filesystem
//!   and is atomic; a reader never observes a half-written file.
//! * **Clean failure.** Any write/fsync/rename error removes the temp file, so
//!   no partial temp leaks beside the destination.

use std::io;
use std::path::{Path, PathBuf};

/// Atomically write `data` to `path`.
///
/// Creates the destination's parent directory if absent (cache locations such
/// as `~/.cache/mtui` may not exist on a fresh checkout), writes `data` to a
/// unique same-directory temp file opened with `create_new` (and `0o600` on
/// unix), fsyncs it, then renames it over `path`. See the [module
/// docs](self) for the full set of guarantees.
///
/// Callers assemble the final bytes themselves (e.g. read-existing + append)
/// and hand the complete buffer here.
///
/// # Errors
///
/// Returns any I/O error from creating the parent directory, opening/writing
/// the temp file, fsyncing it, or renaming it into place. On any failure after
/// the temp is created, the temp file is removed before returning.
pub fn write(data: &[u8], path: &Path) -> io::Result<()> {
    use io::Write as _;

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }

    let dir = parent.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let tmp = unique_temp_path(&dir, path);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;

    if let Err(e) = file.write_all(data).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    drop(file);

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// A unique temp path in `dir` derived from `target`'s file name plus the PID
/// (cross-process uniqueness), a nanosecond timestamp, and a
/// process-lifetime-monotonic counter (intra-process uniqueness across threads
/// racing within the same nanosecond), so concurrent writers never collide on
/// the `create_new` open.
fn unique_temp_path(dir: &Path, target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let base = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("mtui-atomic");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{base}.{}.{nanos}.{seq}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_files_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".tmp"))
            })
            .collect()
    }

    #[test]
    fn creates_missing_parent_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("out.txt");
        assert!(!path.parent().unwrap().exists());
        write(b"hello", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert!(
            tmp_files_in(path.parent().unwrap()).is_empty(),
            "no leftover temp"
        );
    }

    #[test]
    fn replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        write(b"first", &path).unwrap();
        write(b"second", &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert!(tmp_files_in(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write(b"data", &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_and_no_partial_on_rename_failure() {
        // A destination whose parent is read-only makes create_new succeed
        // (the temp is created before the dir is locked)… so instead force the
        // failure by making the *destination itself* a directory: rename of a
        // file over a non-empty dir fails, exercising the cleanup path.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("child"), b"x").unwrap();

        let err = write(b"data", &dest).unwrap_err();
        assert!(err.kind() != io::ErrorKind::NotFound, "unexpected: {err}");
        // Destination is untouched (still the directory) and no temp leaked.
        assert!(dest.is_dir());
        assert!(
            tmp_files_in(dir.path()).is_empty(),
            "temp must be cleaned up"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_at_destination_is_replaced_not_followed() {
        // A pre-existing symlink at the destination must be replaced by the
        // rename, not followed to clobber its link target.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"original").unwrap();

        let dest = dir.path().join("link");
        std::os::unix::fs::symlink(&outside, &dest).unwrap();

        write(b"new", &dest).unwrap();

        // The symlink target is untouched; the destination is now a regular file.
        assert_eq!(std::fs::read(&outside).unwrap(), b"original");
        assert!(
            !std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    }

    #[test]
    fn concurrent_writers_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::sync::Arc::new(dir.path().join("shared"));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = std::sync::Arc::clone(&path);
                std::thread::spawn(move || write(format!("writer-{i}").as_bytes(), &path))
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        // Exactly one payload wins; every write landed atomically.
        let final_content = std::fs::read_to_string(&*path).unwrap();
        assert!(
            final_content.starts_with("writer-"),
            "got {final_content:?}"
        );
        assert!(tmp_files_in(dir.path()).is_empty(), "no temp files leaked");
    }
}
