//! The report directory's files: the path guard and bounded line reader shared
//! by the `report_*` file tools.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::session::McpCommandError;
use crate::slim::truncation_notice;

fn refuse(msg: impl Into<String>) -> McpCommandError {
    McpCommandError {
        stdout: String::new(),
        stderr: msg.into(),
        exit_code: 1,
    }
}

/// Resolve `relpath` under `base`, refusing anything that escapes it.
///
/// The target must be `base` itself or a descendant, guarding `..` traversal and
/// absolute paths. Containment is checked twice: a cheap lexical `.`/`..`
/// collapse first, so a not-yet-existing file still resolves where
/// `canonicalize` would fail, then a symlink-aware re-check over the target's
/// longest *existing* ancestor — which catches an in-tree symlink pointing
/// outside the tree, as the lexical pass alone would follow it.
pub(crate) fn safe_template_file(base: &Path, relpath: &str) -> Result<PathBuf, McpCommandError> {
    let base_resolved = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let joined = base_resolved.join(relpath);
    // Normalise `.`/`..` lexically so a not-yet-existing file still resolves.
    let target = normalize(&joined);
    let escape = || refuse(format!("path {relpath:?} escapes the testreport directory"));
    if target != base_resolved && !target.starts_with(&base_resolved) {
        return Err(escape());
    }
    // Compare against the *canonicalized* base ancestor, so a base that does not
    // itself exist on disk is not spuriously rejected against a canonicalized
    // target prefix (e.g. `/tmp`→`/private/tmp`).
    let resolved = resolve_existing_ancestor(&target);
    let base_canon = resolve_existing_ancestor(&base_resolved);
    if resolved != base_canon && !resolved.starts_with(&base_canon) {
        return Err(escape());
    }
    Ok(target)
}

/// Canonicalize `path`'s longest existing ancestor, re-appending the trailing
/// not-yet-existing components lexically. Symlinks in the existing prefix are
/// followed; the missing tail is left as-is (nothing to resolve). Falls back to
/// the lexical `path` when even the root cannot be canonicalized.
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(canon) = ancestor.canonicalize() {
            let mut out = canon;
            for comp in tail.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match (ancestor.file_name(), ancestor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                ancestor = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Lexically normalise a path, collapsing `.` and `..` without touching disk.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Outcome of a streamed, bounded read of a checkout file.
pub(crate) struct StreamRead {
    /// Lines read (the whole file's total unless `truncated`, in which case the
    /// lines observed up to `max_bytes`). Matches the `splitlines` convention.
    pub(crate) line_count: usize,
    /// The requested content: the whole (byte-capped) file for a non-windowed
    /// read, or just the `[offset, offset+limit)` window otherwise.
    pub(crate) content: String,
    /// Lines in the returned window (windowed reads only; `None` for whole-file).
    pub(crate) returned_lines: Option<usize>,
}

/// Stream `path` line-by-line, buffering only what the request needs.
///
/// Blocking; call inside [`spawn_blocking`](tokio::task::spawn_blocking). Reads
/// at most `max_bytes` source bytes (`0` = unbounded), counting every line for
/// `line_count` while buffering only the requested content, so a huge file costs
/// O(1) memory beyond the window. `window` is `None` for a whole-file read or
/// `Some((offset_1based, limit))` for a windowed one. Decoding is UTF-8-lossy per
/// line; splitting on the `\n` byte cannot split a codepoint. Also returns the
/// canonical path for the dedup key (lexical fallback, never an error), so the
/// read and the key stat share one worker hop.
pub(crate) fn stream_read(
    path: &Path,
    max_bytes: usize,
    window: Option<(usize, Option<usize>)>,
) -> Result<(StreamRead, PathBuf), McpCommandError> {
    let file = std::fs::File::open(path)
        .map_err(|e| refuse(format!("failed to read {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);

    let mut line_count = 0usize;
    let mut read_bytes = 0usize;
    let mut truncated = false;
    let mut content = String::new();
    let mut returned = 0usize;
    // Window bounds as 0-based half-open [start, end) over line indices.
    let (start, end) = match window {
        Some((offset, limit)) => {
            let start = offset - 1;
            let end = limit.map(|n| start.saturating_add(n));
            (start, end)
        }
        None => (0, None),
    };

    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| refuse(format!("failed to read {}: {e}", path.display())))?;
        if n == 0 {
            break; // EOF
        }
        let idx = line_count;
        line_count += 1;
        read_bytes += n;

        let keep = match window {
            None => true,
            Some(_) => idx >= start && end.is_none_or(|e| idx < e),
        };
        if keep {
            content.push_str(&String::from_utf8_lossy(&buf));
            returned += 1;
        }

        if max_bytes != 0 && read_bytes >= max_bytes {
            // Byte cap reached: stop before EOF. `line_count` now reflects lines
            // observed up to the cap, not the (unknown) file total.
            truncated = true;
            break;
        }
    }

    if truncated {
        let dropped = read_bytes.saturating_sub(max_bytes);
        content.push_str(&truncation_notice(dropped, max_bytes));
    }

    Ok((
        StreamRead {
            line_count,
            content,
            returned_lines: window.map(|_| returned),
        },
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    ))
}

/// The loose file readable and listed beside the directory roots.
pub(crate) const CHECKERS_LOG: &str = "checkers.log";
const BUILD_CHECKS: &str = "build_checks";
const RESULTS: &str = "results";

/// The report-dir subdirectories the file tools cover, in listing order.
fn dir_roots(install_logs: &Path) -> [String; 3] {
    [
        BUILD_CHECKS.to_owned(),
        install_logs.to_string_lossy().into_owned(),
        RESULTS.to_owned(),
    ]
}

/// A regular file in the report directory, as the file tools list it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LocalFile {
    /// Report-dir-relative, `/`-separated (`install_logs/h1.log`).
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) size: u64,
}

/// Lists the report directory's regular files under the directory roots, plus
/// `checkers.log`, sorted by path. Flat and symlink-free, so a file shows up
/// here exactly when the server's flat listing could hold it. Blocking.
pub(crate) fn list_local(dir: &Path, install_logs: &Path) -> Vec<LocalFile> {
    let mut files = Vec::new();
    for root in dir_roots(install_logs) {
        let Ok(entries) = std::fs::read_dir(dir.join(&root)) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if meta.is_file() {
                files.push(LocalFile {
                    path: format!("{root}/{name}"),
                    name,
                    size: meta.len(),
                });
            }
        }
    }
    if let Ok(meta) = std::fs::symlink_metadata(dir.join(CHECKERS_LOG))
        && meta.is_file()
    {
        files.push(LocalFile {
            path: CHECKERS_LOG.to_owned(),
            name: CHECKERS_LOG.to_owned(),
            size: meta.len(),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// What [`resolve_local`] found at an allowed path.
#[derive(Debug)]
pub(crate) enum LocalLookup {
    Found(PathBuf),
    Missing,
}

/// Resolves `rel` to a file the tools may read, returning it with its
/// normalised report-dir-relative spelling.
///
/// Refuses a path that escapes `base` ([`safe_template_file`]) and one that is
/// not `checkers.log` or a direct child of a directory root. Anything but a
/// regular file at the target reads as [`LocalLookup::Missing`], matching what
/// [`list_local`] shows. Blocking.
pub(crate) fn resolve_local(
    base: &Path,
    install_logs: &Path,
    rel: &str,
) -> Result<(LocalLookup, String), McpCommandError> {
    let target = safe_template_file(base, rel)?;
    let base_canon = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let relative = target
        .strip_prefix(&base_canon)
        .map_err(|_| refuse(format!("path {rel:?} escapes the testreport directory")))?;
    let parts: Vec<&str> = relative
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or_default())
        .collect();
    let allowed = match parts.as_slice() {
        [file] => *file == CHECKERS_LOG,
        [root, _] => dir_roots(install_logs).iter().any(|r| r == root),
        _ => false,
    };
    if !allowed {
        return Err(refuse(format!(
            "{rel:?} is not a report file; readable: {}, {CHECKERS_LOG}",
            dir_roots(install_logs).map(|r| format!("{r}/")).join(", ")
        )));
    }
    let normalised = parts.join("/");
    let found = std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_file());
    let lookup = if found {
        LocalLookup::Found(target)
    } else {
        LocalLookup::Missing
    };
    Ok((lookup, normalised))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_template_file_allows_nested_and_blocks_escape() {
        let base = Path::new("/tmp/checkout");
        assert!(safe_template_file(base, "build_checks/x.log").is_ok());
        assert!(safe_template_file(base, "../escape").is_err());
        assert!(safe_template_file(base, "/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn safe_template_file_blocks_in_tree_symlink_escape() {
        use std::os::unix::fs::symlink;

        // Refused even though the relpath is lexically inside `base`.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("checkout");
        std::fs::create_dir_all(&base).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "top secret\n").unwrap();

        // symlink: <base>/escape -> <tmp>/outside
        symlink(&outside, base.join("escape")).unwrap();

        let err = safe_template_file(&base, "escape/secret")
            .expect_err("in-tree symlink escaping the checkout must refuse");
        assert!(err.stderr.contains("escapes"), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn safe_template_file_allows_in_tree_symlink() {
        use std::os::unix::fs::symlink;

        // A symlink pointing to a file *inside* the checkout is allowed.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("checkout");
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::fs::write(base.join("real/file"), "ok\n").unwrap();

        // symlink: <base>/link -> <base>/real
        symlink(base.join("real"), base.join("link")).unwrap();

        let resolved = safe_template_file(&base, "link/file").expect("in-tree symlink is allowed");
        assert!(resolved.ends_with("file"), "{resolved:?}");
    }
}
