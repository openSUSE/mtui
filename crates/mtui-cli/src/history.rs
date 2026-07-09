//! Persistent REPL command history.
//!
//! Ports upstream `mtui.cli._history` + the `PromptSession(history=…,
//! enable_history_search=True, auto_suggest=AutoSuggestFromHistory())` wiring in
//! `mtui.cli.repl` onto reedline. Three behaviours come together here:
//!
//! * **Persistence** — a [`FileBackedHistory`] keeps the up-arrow stack across
//!   sessions, the equivalent of upstream's `FileHistory`.
//! * **Reverse-search** — Ctrl-R, provided by reedline's default emacs edit mode
//!   (upstream `enable_history_search=True`); it needs no wiring here, only a
//!   populated history to search.
//! * **Inline suggestion** — the greyed hint shown by [`DefaultHinter`] is
//!   reedline's analogue of upstream `AutoSuggestFromHistory`; it is wired in
//!   [`crate::repl::Repl::new`] alongside [`file_backed_history`].
//!
//! ## Location (deliberate deviation from upstream)
//!
//! Upstream persists to `~/.mtui_history`. mtui-rs is XDG-first, so the file
//! lives at `$XDG_DATA_HOME/mtui/history` ([`mtui_config::data_dir`]). This keeps
//! durable per-user state out of the config and cache trees.
//!
//! ## Degradation contract
//!
//! History is best-effort, matching mtui-rs's lenient config philosophy: if the
//! data directory cannot be resolved, or the file cannot be created/opened, the
//! REPL falls back to an **in-memory** history (a WARN is logged via `tracing`)
//! rather than failing to start. The line editor is always usable.

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
pub fn file_backed_history() -> Box<dyn History> {
    history_from_path(mtui_config::data_dir().map(|d| d.join(HISTORY_FILE)))
}

/// Pure core of [`file_backed_history`], with the target path injected so the
/// happy path and the degradation path are both unit-testable without touching
/// the process environment (mirrors the `resolve_search_paths` pattern in
/// `mtui-config`).
///
/// `None` (no data dir) or a `with_file` failure (unwritable path, mkdir error)
/// both yield the in-memory [`FileBackedHistory::default`], with a WARN logged
/// on the error path.
#[must_use]
pub fn history_from_path(path: Option<PathBuf>) -> Box<dyn History> {
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

    /// The production entry point returns a usable backend without panicking,
    /// whether or not a data dir resolves in the test environment.
    #[test]
    fn file_backed_history_is_constructible() {
        let history = file_backed_history();
        // A fresh/opened history must be searchable (no panic, empty or not).
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

    /// An unwritable path → error path → in-memory fallback (never a panic).
    #[test]
    fn unwritable_path_degrades_to_in_memory() {
        // A path whose parent cannot be created: a file used as a directory.
        let mut file = std::env::temp_dir();
        file.push(format!("mtui-history-blocker-{}", std::process::id()));
        std::fs::write(&file, b"x").expect("seed a regular file");
        let bad = file.join("nested").join("history");

        let mut history = history_from_path(Some(bad));
        // Fallback is in-memory but fully functional.
        let saved = history
            .save(HistoryItem::from_command_line("add host"))
            .expect("fallback save must succeed");
        assert_eq!(saved.command_line, "add host");

        let _ = std::fs::remove_file(&file);
    }

    /// Round-trip across two backends over the same file: what one session
    /// writes, the next session recalls — the persistence contract.
    #[test]
    fn history_persists_across_sessions() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mtui-history-roundtrip-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        // Session 1: write an entry and drop the backend (sync on drop).
        {
            let mut h1 = history_from_path(Some(path.clone()));
            h1.save(HistoryItem::from_command_line("testreport export"))
                .expect("save in session 1");
            h1.sync().expect("sync session 1");
        }

        // Session 2: a new backend over the same file recalls it.
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
