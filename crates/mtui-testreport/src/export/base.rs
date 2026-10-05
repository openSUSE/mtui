//! Shared export state and the install-log writer.
//!
//! Rust has no class inheritance, so the shared state and common methods live
//! in [`ExportContext`], which each concrete exporter (`auto`, `manual`,
//! `kernel`) embeds.
//!
//! Writing a divergent existing file asks whether to overwrite it. As in
//! `mtui-hosts::target::actions`, that prompt is a display concern owned by
//! `mtui-cli` and is injected here as the [`OverwritePrompt`] trait, so the
//! exporter stays testable and free of a CLI dependency; the default
//! [`DenyOverwrite`] never overwrites (safe for non-interactive runs and MCP).

use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_types::RequestReviewID;

use crate::support::fileops::{atomic_write_file, timestamp};

/// The decision an exporter makes when an existing file differs from what it is
/// about to write.
///
/// `mtui` supplies an interactive implementation; library and test callers use
/// [`DenyOverwrite`]. `Send + Sync` because
/// [`AutoExport::run`](crate::AutoExport) holds the boxed prompt across an
/// `.await`.
pub trait OverwritePrompt: Send + Sync {
    /// Returns `true` to overwrite `path` in place, `false` to write to a
    /// timestamp-suffixed sibling instead.
    fn should_overwrite(&self, path: &Path) -> bool;
}

/// The non-interactive default: never overwrite a divergent file, so the
/// exporter falls back to a timestamped filename.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyOverwrite;

impl OverwritePrompt for DenyOverwrite {
    fn should_overwrite(&self, _path: &Path) -> bool {
        false
    }
}

/// Shared state and helpers for every exporter.
///
/// The per-connector openQA results are *not* a field here: their types differ
/// per exporter, so each concrete one holds its own.
pub struct ExportContext {
    /// The application configuration.
    pub(crate) config: Config,
    /// Whether to overwrite existing files without prompting.
    pub(crate) force: bool,
    /// The RRID of the current update.
    pub(crate) rrid: RequestReviewID,
}

impl ExportContext {
    /// Builds an export context.
    #[must_use]
    pub fn new(config: Config, force: bool, rrid: RequestReviewID) -> Self {
        Self {
            config,
            force,
            rrid,
        }
    }

    /// Writes `lines` (joined with `\n`) to `fn_path`.
    ///
    /// If the file exists and `force` is unset: an identical file is left
    /// untouched; a divergent file is overwritten only when `prompt` agrees,
    /// otherwise the write is redirected to a `.{timestamp}` sibling.
    ///
    /// I/O failures are logged and swallowed.
    pub(crate) fn writer(&self, fn_path: &Path, lines: &[String], prompt: &dyn OverwritePrompt) {
        let to_write = lines.join("\n");
        let mut target = fn_path.to_path_buf();

        if target.exists() && !self.force {
            match std::fs::read_to_string(&target) {
                Ok(existing) if existing == to_write => {
                    tracing::info!("Log {} exists and is same as export", target.display());
                    return;
                }
                _ => {
                    tracing::warn!("file {} exists.", target.display());
                    if !prompt.should_overwrite(&target) {
                        target = target.with_extension(timestamp());
                    }
                }
            }
        }

        tracing::info!("exporting log to {}", target.display());
        if let Err(e) = atomic_write_file(to_write.as_bytes(), &target) {
            tracing::error!("Failed to write {}: {e}", target.display());
        }
    }

    /// Path of the per-RRID install-logs directory
    /// (`template_dir/<rrid>/install_logs`).
    #[must_use]
    pub(crate) fn install_logs_dir(&self) -> PathBuf {
        self.config
            .template_dir
            .join(self.rrid.to_string())
            .join(&self.config.install_logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with() -> ExportContext {
        let mut cfg = Config::default();
        cfg.install_logs = PathBuf::from("install_logs");
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        ExportContext::new(cfg, false, rrid)
    }

    #[test]
    fn writer_skips_identical_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        std::fs::write(&path, "a\nb").unwrap();
        let c = ctx_with();
        c.writer(&path, &["a".into(), "b".into()], &DenyOverwrite);
        // Unchanged, and no timestamped sibling created.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb");
        let siblings: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(siblings.len(), 1);
    }

    #[test]
    fn writer_divergent_without_overwrite_writes_timestamped_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        std::fs::write(&path, "old").unwrap();
        let c = ctx_with();
        c.writer(&path, &["new".into()], &DenyOverwrite);
        // Original untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        // A sibling with a numeric (timestamp) extension holds the new content.
        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(entries.iter().any(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.chars().all(|c| c.is_ascii_digit()))
                && std::fs::read_to_string(p).unwrap() == "new"
        }));
    }
}
