//! Persistent REPL command history.
//!
//! A [`FileBackedHistory`] keeps the up-arrow stack across sessions; Ctrl-R
//! reverse-search comes free from reedline's default emacs edit mode, and the
//! greyed inline hint ([`DefaultHinter`](reedline::DefaultHinter)) is wired
//! alongside [`file_backed_history`] in [`crate::repl::Repl::new`].
//!
//! mtui is XDG-first: the history file lives at `$XDG_DATA_HOME/mtui/history`
//! ([`mtui_config::data_dir`]), keeping durable per-user state out of the config
//! and cache trees.
//!
//! History is best-effort, matching mtui's lenient config philosophy — an
//! unresolvable data directory or an unopenable file degrades to an **in-memory**
//! history with a WARN rather than failing to start.

use std::path::PathBuf;

use reedline::{FileBackedHistory, HISTORY_SIZE, History};

/// Basename of the history file inside the mtui data directory.
const HISTORY_FILE: &str = "history";

/// Builds the shared REPL history backend.
///
/// Persists to `$XDG_DATA_HOME/mtui/history` when the data directory resolves,
/// otherwise (or on any I/O error) degrades to an in-memory history. Returns a
/// boxed trait object so [`crate::repl`] stays decoupled from the concrete
/// backend.
#[must_use]
pub(crate) fn file_backed_history() -> Box<dyn History> {
    history_from_path(mtui_config::data_dir().map(|d| d.join(HISTORY_FILE)))
}

/// Pure core of [`file_backed_history`], with the path injected so both the
/// happy and the degradation path are unit-testable without touching the
/// process environment. `None` or a `with_file` failure both yield the
/// in-memory [`FileBackedHistory::default`], the latter with a WARN.
#[must_use]
fn history_from_path(path: Option<PathBuf>) -> Box<dyn History> {
    let Some(path) = path else {
        tracing::warn!("no data directory resolved; REPL history will not persist");
        return Box::new(FileBackedHistory::default());
    };

    match FileBackedHistory::with_file(HISTORY_SIZE, path.clone()) {
        Ok(history) => Box::new(history),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                %err,
                "failed to open REPL history file; falling back to in-memory history"
            );
            Box::new(FileBackedHistory::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{HistoryItem, SearchDirection, SearchQuery};

    /// The production entry point returns a usable, searchable backend whether
    /// or not a data dir resolves in the test environment.
    #[test]
    fn file_backed_history_is_constructible() {
        let history = file_backed_history();
        let all = history
            .search(SearchQuery::everything(SearchDirection::Forward, None))
            .expect("history search must succeed");
        assert!(all.len() <= HISTORY_SIZE);
    }

    /// No data dir → in-memory fallback, still a working history.
    #[test]
    fn missing_data_dir_falls_back_to_in_memory() {
        let mut history = history_from_path(None);
        let item = history
            .save(HistoryItem::from_command_line("list"))
            .expect("in-memory save must succeed");
        assert_eq!(item.command_line, "list");
    }

    /// An unwritable path takes the error path to the in-memory fallback, never
    /// a panic.
    #[test]
    fn unwritable_path_degrades_to_in_memory() {
        // A parent that cannot be created: a regular file used as a directory.
        let mut file = std::env::temp_dir();
        file.push(format!("mtui-history-blocker-{}", std::process::id()));
        std::fs::write(&file, b"x").expect("seed a regular file");
        let bad = file.join("nested").join("history");

        let mut history = history_from_path(Some(bad));
        let saved = history
            .save(HistoryItem::from_command_line("add host"))
            .expect("fallback save must succeed");
        assert_eq!(saved.command_line, "add host");

        let _ = std::fs::remove_file(&file);
    }

    /// The persistence contract: what one session writes over a file, the next
    /// backend over that file recalls.
    #[test]
    fn history_persists_across_sessions() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mtui-history-roundtrip-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        {
            let mut h1 = history_from_path(Some(path.clone()));
            h1.save(HistoryItem::from_command_line("testreport export"))
                .expect("save in session 1");
            h1.sync().expect("sync session 1");
        }

        let h2 = history_from_path(Some(path.clone()));
        let found = h2
            .search(SearchQuery::everything(SearchDirection::Forward, None))
            .expect("search in session 2");
        assert!(
            found.iter().any(|i| i.command_line == "testreport export"),
            "entry from session 1 should be recalled in session 2, got: {found:?}"
        );

        let _ = std::fs::remove_file(&path);
    }
}
