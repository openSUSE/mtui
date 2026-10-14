//! The report directory's files: the path guard and bounded line reader shared
//! by the `report_*` file tools.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::session::McpCommandError;

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
    /// Lines observed: the file's total, or — when `max_bytes` cut the read —
    /// the lines within the prefix that was read. Matches the `splitlines`
    /// convention.
    pub(crate) line_count: usize,
    /// The requested content: the whole (byte-capped) file for a non-windowed
    /// read, or just the `[offset, offset+limit)` window otherwise.
    pub(crate) content: String,
    /// Lines in the returned window (windowed reads only; `None` for whole-file).
    pub(crate) returned_lines: Option<usize>,
    #[cfg(test)]
    peak_line_bytes: usize,
}

fn source_cap_notice(cap: usize) -> String {
    format!(
        "\n…[source read stopped at {cap} bytes; total_lines counts the lines read, \
         the remaining size is unknown — raise [mcp] max_input_bytes to read further]"
    )
}

/// Stream `path` line-by-line, buffering only what the request needs.
///
/// Blocking; call inside [`spawn_blocking`](tokio::task::spawn_blocking). Reads
/// at most `max_bytes` source bytes (`0` = unbounded) — a line crossing the cap
/// is cut there — counting every line for `line_count` while buffering only the
/// requested content, so a huge file costs O(1) memory beyond the window.
/// `window` is `None` for a whole-file read or `Some((offset_1based, limit))`
/// for a windowed one. Decoding is UTF-8-lossy per line (a cap inside a
/// codepoint yields a replacement character); splitting on the `\n` byte cannot
/// split a codepoint. Also returns the canonical path for the dedup key (lexical
/// fallback, never an error), so the read and the key stat share one worker hop.
pub(crate) fn stream_read(
    path: &Path,
    max_bytes: usize,
    window: Option<(usize, Option<usize>)>,
) -> Result<(StreamRead, PathBuf), McpCommandError> {
    let file = std::fs::File::open(path)
        .map_err(|e| refuse(format!("failed to read {}: {e}", path.display())))?;
    let read = stream_read_from(BufReader::new(file), max_bytes, window)
        .map_err(|e| refuse(format!("failed to read {}: {e}", path.display())))?;
    Ok((
        read,
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
    ))
}

fn stream_read_from<R: BufRead>(
    mut reader: R,
    max_bytes: usize,
    window: Option<(usize, Option<usize>)>,
) -> std::io::Result<StreamRead> {
    // Window bounds as 0-based half-open [start, end) over line indices.
    let (start, end) = match window {
        Some((offset, limit)) => {
            let start = offset - 1;
            (start, limit.map(|n| start.saturating_add(n)))
        }
        None => (0, None),
    };
    let selected = |idx: usize| idx >= start && end.is_none_or(|e| idx < e);

    let mut remaining = (max_bytes != 0).then_some(max_bytes);
    let mut line_count = 0usize;
    let mut returned = 0usize;
    let mut content = String::new();
    // Bytes of the current line, kept only while it is inside the window.
    let mut line: Vec<u8> = Vec::new();
    let mut mid_line = false;
    let mut truncated = false;
    #[cfg(test)]
    let mut peak_line_bytes = 0usize;

    loop {
        let avail = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            break;
        }
        let take = remaining.map_or(avail.len(), |r| avail.len().min(r));
        if take == 0 {
            truncated = true;
            break;
        }
        let mut chunk = &avail[..take];
        while !chunk.is_empty() {
            let keep = selected(line_count);
            if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
                if keep {
                    line.extend_from_slice(&chunk[..=nl]);
                    #[cfg(test)]
                    {
                        peak_line_bytes = peak_line_bytes.max(line.len());
                    }
                    content.push_str(&String::from_utf8_lossy(&line));
                    returned += 1;
                    line.clear();
                }
                line_count += 1;
                mid_line = false;
                chunk = &chunk[nl + 1..];
            } else {
                if keep {
                    line.extend_from_slice(chunk);
                    #[cfg(test)]
                    {
                        peak_line_bytes = peak_line_bytes.max(line.len());
                    }
                }
                mid_line = true;
                chunk = &[];
            }
        }
        reader.consume(take);
        if let Some(r) = remaining.as_mut() {
            *r -= take;
        }
    }

    if mid_line {
        if selected(line_count) {
            content.push_str(&String::from_utf8_lossy(&line));
            returned += 1;
        }
        line_count += 1;
    }
    if truncated {
        content.push_str(&source_cap_notice(max_bytes));
    }

    Ok(StreamRead {
        line_count,
        content,
        returned_lines: window.map(|_| returned),
        #[cfg(test)]
        peak_line_bytes,
    })
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

    const NOTICE_MARK: &str = "\n…[";

    fn read(bytes: &[u8], cap: usize, window: Option<(usize, Option<usize>)>) -> StreamRead {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.log");
        std::fs::write(&path, bytes).unwrap();
        stream_read(&path, cap, window).unwrap().0
    }

    fn split_notice(content: &str) -> (&str, bool) {
        match content.find(NOTICE_MARK) {
            Some(i) => (&content[..i], true),
            None => (content, false),
        }
    }

    /// Hands out at most `chunk` bytes per `fill_buf`, so scanner behaviour at
    /// buffer boundaries is observable, and counts what was actually consumed.
    struct Chunked {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl Chunked {
        fn new(data: &[u8], chunk: usize) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
                chunk,
            }
        }
    }

    impl std::io::Read for Chunked {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = {
                let buf = self.fill_buf()?;
                let n = buf.len().min(out.len());
                out[..n].copy_from_slice(&buf[..n]);
                n
            };
            self.consume(n);
            Ok(n)
        }
    }

    impl BufRead for Chunked {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = self.data.len().min(self.pos + self.chunk);
            Ok(&self.data[self.pos..end])
        }

        fn consume(&mut self, amt: usize) {
            self.pos += amt;
        }
    }

    #[test]
    fn a_giant_line_is_cut_at_the_source_cap() {
        let body = vec![b'x'; 1 << 20];
        let r = read(&body, 16, None);
        let (source, noticed) = split_notice(&r.content);
        assert_eq!(source, "x".repeat(16));
        assert!(noticed);
        assert_eq!(r.line_count, 1);
        assert_eq!(r.returned_lines, None);
        assert_eq!(
            r.content,
            format!("{}{}", "x".repeat(16), source_cap_notice(16))
        );
    }

    #[test]
    fn the_source_cap_notice_is_pinned() {
        assert_eq!(
            source_cap_notice(10),
            "\n…[source read stopped at 10 bytes; total_lines counts the lines read, \
             the remaining size is unknown — raise [mcp] max_input_bytes to read further]"
        );
    }

    #[test]
    fn a_capped_read_consumes_no_more_than_the_cap() {
        let body = vec![b'x'; 1 << 20];
        let mut reader = Chunked::new(&body, 7);
        let r = stream_read_from(&mut reader, 16, None).unwrap();
        assert_eq!(reader.pos, 16);
        assert!(r.peak_line_bytes <= 16, "{}", r.peak_line_bytes);
    }

    #[test]
    fn a_skipped_line_is_never_buffered_even_without_a_cap() {
        let mut body = vec![b'x'; 1 << 20];
        body.extend_from_slice(b"\ny\n");
        let r = stream_read_from(Chunked::new(&body, 64), 0, Some((2, Some(1)))).unwrap();
        assert_eq!(r.content, "y\n");
        assert_eq!(r.line_count, 2);
        assert_eq!(r.peak_line_bytes, 2);
    }

    #[test]
    fn a_multibyte_char_split_across_buffers_decodes_whole() {
        for chunk in 1..=5 {
            let r = stream_read_from(Chunked::new("A€B\n".as_bytes(), chunk), 0, None).unwrap();
            assert_eq!(r.content, "A€B\n", "chunk {chunk}");
            assert_eq!(r.line_count, 1);
        }
    }

    #[test]
    fn a_skipped_giant_line_is_not_returned() {
        let mut body = vec![b'x'; 1 << 20];
        body.extend_from_slice(b"\ny\n");
        let r = read(&body, 16, Some((2, Some(1))));
        let (source, noticed) = split_notice(&r.content);
        assert_eq!(source, "");
        assert!(noticed);
        assert_eq!(r.line_count, 1);
        assert_eq!(r.returned_lines, Some(0));
    }

    #[test]
    fn the_cap_boundary_counts_lines_and_marks_truncation() {
        type Row = (&'static [u8], usize, &'static str, usize, bool);
        let rows: [Row; 5] = [
            (b"", 16, "", 0, false),
            (b"ab\ncd", 3, "ab\n", 1, true),
            (b"ab\ncd", 4, "ab\nc", 2, true),
            (b"ab\n", 3, "ab\n", 1, false),
            (b"ab", 16, "ab", 1, false),
        ];
        for (bytes, cap, source, lines, truncated) in rows {
            for chunk in [1, 2, 3, 64] {
                let r = stream_read_from(Chunked::new(bytes, chunk), cap, None).unwrap();
                let (got, noticed) = split_notice(&r.content);
                assert_eq!(got, source, "{bytes:?} cap {cap} chunk {chunk}");
                assert_eq!(r.line_count, lines, "{bytes:?} cap {cap} chunk {chunk}");
                assert_eq!(noticed, truncated, "{bytes:?} cap {cap} chunk {chunk}");
            }
        }
    }

    #[test]
    fn a_zero_cap_reads_everything_and_still_counts_past_the_window() {
        let r = read(b"a\nb\nc\n", 0, Some((2, Some(1))));
        assert_eq!(r.content, "b\n");
        assert_eq!(r.line_count, 3);
        assert_eq!(r.returned_lines, Some(1));
    }

    #[test]
    fn a_zero_limit_returns_nothing_but_counts_the_prefix() {
        let r = read(b"a\nb\n", 3, Some((2, Some(0))));
        let (source, noticed) = split_notice(&r.content);
        assert_eq!(source, "");
        assert!(noticed);
        assert_eq!(r.line_count, 2);
        assert_eq!(r.returned_lines, Some(0));
    }

    #[test]
    fn a_cap_inside_a_multibyte_char_decodes_the_prefix_lossily() {
        let r = read("A€B\n".as_bytes(), 2, None);
        let (source, noticed) = split_notice(&r.content);
        assert_eq!(source, "A\u{FFFD}");
        assert!(noticed);
        assert_eq!(r.line_count, 1);
    }
}
