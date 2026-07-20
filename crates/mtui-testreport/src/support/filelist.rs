//! A line-buffer that can be loaded from and saved back to a file.
//!
//! Ports upstream `mtui.types.filelist.FileList`, a `UserList[str]` whose
//! elements are the file's lines **with their trailing newline preserved**
//! (Python `readlines()` semantics). The exporters mutate this buffer in place
//! — inserting result blocks, links, and system info — then persist it.
//!
//! Upstream tracks a hash of the content captured at load time and only writes
//! on context-manager exit when the content changed. Rust has no `with`
//! statement, so that behaviour is exposed explicitly:
//!
//! * [`FileList::is_dirty`] reports whether the buffer differs from what was
//!   loaded, and
//! * [`FileList::write_if_dirty`] performs the conditional atomic write,
//!   mirroring `FileList.__exit__`.
//!
//! [`FileList::write`] always writes (mirroring the eager `FileList.write`).

use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use super::fileops::atomic_write_file;

/// A `Vec<String>` of file lines (each keeping its trailing newline) bound to a
/// source path, with load-time change tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileList {
    lines: Vec<String>,
    file: PathBuf,
    /// The joined content observed at load time, used to detect mutation.
    loaded: String,
}

impl FileList {
    /// Builds a [`FileList`] from in-memory lines bound to `path`.
    ///
    /// Used by tests and callers that synthesize a template rather than reading
    /// one from disk. The load snapshot is taken from `lines` as given, so a
    /// freshly built list is not considered dirty until mutated.
    #[must_use]
    pub fn from_lines(path: impl Into<PathBuf>, lines: Vec<String>) -> Self {
        let loaded = lines.concat();
        Self {
            lines,
            file: path.into(),
            loaded,
        }
    }

    /// Loads a [`FileList`] from `path`, splitting into lines that each retain a
    /// trailing `\n` (matching Python `readlines()`).
    ///
    /// Invalid UTF-8 is replaced (upstream opens with `errors="replace"`).
    ///
    /// # Errors
    ///
    /// Returns any I/O error from reading the file.
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let bytes = std::fs::read(&path)?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let lines = split_keepends(&text);
        let loaded = lines.concat();
        Ok(Self {
            lines,
            file: path,
            loaded,
        })
    }

    /// The path this list is bound to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// The lines as a slice.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The current content as a single string (upstream `"".join(self.data)`).
    #[must_use]
    pub fn content(&self) -> String {
        self.lines.concat()
    }

    /// Whether the buffer differs from the content observed at load time.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.content() != self.loaded
    }

    /// Atomically writes the current content to the bound path (always).
    ///
    /// Mirrors the eager upstream `FileList.write`. After a successful write the
    /// load snapshot is refreshed so a subsequent [`is_dirty`](Self::is_dirty)
    /// reflects changes made *after* this write.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from the atomic write.
    pub fn write(&mut self) -> io::Result<()> {
        let content = self.content();
        atomic_write_file(content.as_bytes(), &self.file)?;
        self.loaded = content;
        Ok(())
    }

    /// Writes only when the buffer changed since load (upstream `__exit__`).
    ///
    /// Returns `true` when a write happened.
    ///
    /// # Errors
    ///
    /// Returns any I/O error from the atomic write.
    pub fn write_if_dirty(&mut self) -> io::Result<bool> {
        if self.is_dirty() {
            self.write()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl Deref for FileList {
    type Target = Vec<String>;

    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

impl DerefMut for FileList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lines
    }
}

/// Splits `text` into lines that each keep their trailing `\n`, matching
/// Python's `str.splitlines(keepends=True)` for the `\n` case (mtui templates
/// are Unix-newline text). A trailing chunk without a newline is kept as its
/// own final element; an empty string yields no lines.
fn split_keepends(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(text[start..=i].to_string());
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(text[start..].to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_trailing_newlines() {
        assert_eq!(split_keepends("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_keepends("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_keepends(""), Vec::<String>::new());
        assert_eq!(split_keepends("\n"), vec!["\n"]);
    }

    #[test]
    fn load_round_trips_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tpl.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let fl = FileList::load(&path).unwrap();
        assert_eq!(fl.lines(), &["one\n".to_string(), "two\n".to_string()]);
        assert!(!fl.is_dirty());
    }

    #[test]
    fn mutation_marks_dirty_and_conditional_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tpl.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut fl = FileList::load(&path).unwrap();

        // Unchanged: no write.
        assert!(!fl.write_if_dirty().unwrap());

        fl.push("two\n".to_string());
        assert!(fl.is_dirty());
        assert!(fl.write_if_dirty().unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");

        // Snapshot refreshed after write.
        assert!(!fl.is_dirty());
        assert!(!fl.write_if_dirty().unwrap());
    }

    #[test]
    fn deref_allows_vec_ops() {
        let mut fl = FileList::from_lines("x", vec!["a\n".into()]);
        fl.insert(0, "b\n".into());
        assert_eq!(fl.lines(), &["b\n".to_string(), "a\n".to_string()]);
        assert_eq!(fl.len(), 2);
    }
}
