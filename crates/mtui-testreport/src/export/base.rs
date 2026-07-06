//! Shared export state and template-mutation helpers.
//!
//! Ports upstream `mtui.update_workflow.export.base.BaseExport`. Rust has no
//! class inheritance, so the shared state and common methods live in
//! [`ExportContext`], which each concrete exporter (`auto`, `manual`, `kernel`)
//! embeds; the two `@abstractmethod`s (`get_logs`, `run`) become the
//! [`Exporter`] trait.
//!
//! ## Interactive overwrite prompt
//!
//! Upstream `_writer` calls `prompt_user` to ask whether to overwrite a
//! divergent existing file. Per the established crate-boundary pattern (see
//! `mtui-hosts::target::actions`), the interactive prompt is a display concern
//! owned by `mtui-cli` (Phase 6); here it is injected as the [`OverwritePrompt`]
//! trait so the exporter stays testable and free of a CLI dependency. The
//! default [`DenyOverwrite`] never overwrites (safe for non-interactive runs and
//! the MCP surface), mirroring `prompt_user(..., interactive=False)` returning
//! the default "no".

use std::path::{Path, PathBuf};

use mtui_config::options::Config;
use mtui_types::RequestReviewID;

use crate::support::fileops::{atomic_write_file, timestamp};
use crate::support::sysinfo::{EXPORT_PREFIX, detect_system, system_info};

/// The decision an exporter makes when an existing file differs from what it is
/// about to write.
///
/// The port of upstream `prompt_user(f"Should I overwrite {fn}", ...)`. `mtui`
/// (Phase 6) supplies an interactive implementation; library and test callers
/// use [`DenyOverwrite`].
pub trait OverwritePrompt {
    /// Returns `true` to overwrite `path` in place, `false` to write to a
    /// timestamp-suffixed sibling instead.
    fn should_overwrite(&self, path: &Path) -> bool;
}

/// The non-interactive default: never overwrite a divergent file.
///
/// Mirrors `prompt_user(..., interactive=False)`, which returns the default
/// negative answer, so the exporter falls back to a timestamped filename.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyOverwrite;

impl OverwritePrompt for DenyOverwrite {
    fn should_overwrite(&self, _path: &Path) -> bool {
        false
    }
}

/// Shared state and helpers for every exporter (upstream `BaseExport`).
///
/// Holds the config, the working template, the force flag, and the RRID. The
/// per-connector openQA results are held by the concrete exporters (their types
/// differ), so unlike upstream they are not a field here.
pub struct ExportContext {
    /// The application configuration.
    pub config: Config,
    /// The working template (a copy the exporter mutates).
    pub template: Vec<String>,
    /// Whether to overwrite existing files without prompting.
    pub force: bool,
    /// The RRID of the current update.
    pub rrid: RequestReviewID,
}

impl ExportContext {
    /// Builds an export context over a copy of `template` (upstream copies with
    /// `template[:]`).
    #[must_use]
    pub fn new(config: Config, template: &[String], force: bool, rrid: RequestReviewID) -> Self {
        Self {
            config,
            template: template.to_vec(),
            force,
            rrid,
        }
    }

    /// Writes `lines` (joined with `\n`) to `fn_path` (upstream `_writer`).
    ///
    /// If the file exists and `force` is unset: an identical file is left
    /// untouched; a divergent file is overwritten only when `prompt` agrees,
    /// otherwise the write is redirected to a `.{timestamp}` sibling.
    ///
    /// I/O failures are logged and swallowed (upstream logs and returns).
    pub fn writer(&self, fn_path: &Path, lines: &[String], prompt: &dyn OverwritePrompt) {
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

    /// Adds install-log links to the template (upstream `installlogs_lines`).
    ///
    /// Links are deduplicated against the template from just past a
    /// `HAS_UNTRACKED` marker (or from the whole template if the marker is
    /// absent).
    pub fn installlogs_lines(&mut self, filenames: &[String]) {
        // Index just past the HAS_UNTRACKED marker; links are deduped from there.
        let mut o = 0usize;
        for (i, line) in self.template.iter().enumerate() {
            if line.contains("HAS_UNTRACKED") {
                o = i + 1;
                break;
            }
        }

        let mut index = self.template.len();
        if self
            .template
            .last()
            .is_some_and(|l| l.contains("## export MTUI:"))
        {
            index -= 1;
        }
        self.template.insert(index, "\n".to_string());
        self.template
            .insert(index + 1, "Links for update logs:\n".to_string());
        self.template.insert(index + 2, "\n".to_string());
        index += 2;

        let reports_url = &self.config.reports_url;
        let install_logs = self.config.install_logs.display();
        let mut add_empty_line = false;
        for fn_name in filenames {
            let install_log = format!("{reports_url}/{}/{install_logs}/{fn_name}\n", self.rrid);
            if !self.template[o..].iter().any(|l| *l == install_log) {
                index += 1;
                self.template.insert(index, install_log);
                add_empty_line = true;
            }
        }

        if add_empty_line {
            self.template.insert(index + 1, "\n".to_string());
        }
    }

    /// Collapses consecutive duplicate non-blank lines (upstream `dedup_lines`).
    pub fn dedup_lines(&mut self) {
        let mut lines: Vec<String> = Vec::with_capacity(self.template.len());
        let mut prev: Option<&String> = None;
        for cur in &self.template {
            let is_dup = prev == Some(cur) && cur != "\n";
            if !is_dup {
                lines.push(cur.clone());
            }
            prev = Some(cur);
        }
        self.template = lines;
    }

    /// Appends the system-information footer, unless the last line already is it
    /// (upstream `add_sysinfo`).
    pub fn add_sysinfo(&mut self) {
        let (distro, verid, kernel) = detect_system();
        let info = system_info(
            &distro,
            &verid,
            &kernel,
            &self.config.session_user,
            EXPORT_PREFIX,
        );
        let last_trimmed = self
            .template
            .last()
            .map(|l| l.trim_end().to_string())
            .unwrap_or_default();
        if info.trim_end() != last_trimmed {
            self.template.push(info);
        }
    }

    /// Injects the openqa_overview block, if an overview payload is present
    /// (upstream `inject_overview`).
    ///
    /// Idempotent via begin/end markers — a prior block is replaced in place.
    /// Returns `true` when the template was modified.
    pub fn inject_overview(&mut self, overview: &mtui_datasources::OpenQAOverviewResult) -> bool {
        if !mtui_types::OverviewResult::has_overview(overview) {
            return false;
        }
        let modified = super::overview_inject::inject_overview(
            &mut self.template,
            &overview.single_incidents,
            &overview.aggregated_updates,
            &overview.build_checks,
            overview.skip_aggregated,
        );
        if modified {
            tracing::info!("Injected openqa_overview block into template");
        }
        modified
    }

    /// Inserts the pretty-printed openQA "auto" results block (upstream
    /// `inject_openqa`), removing a previous results block first.
    ///
    /// `pp` is the connector's pretty-print lines (`self.openqa.auto.pp`). A
    /// no-op when `pp` is empty. Requires the template to contain the
    /// `source code change review:` anchor; if absent this is a no-op (the
    /// upstream `.index(...)` would raise, but a missing anchor means the file
    /// is not mtui-shaped, matching how the injector guards on its header).
    pub fn inject_openqa(&mut self, pp: &[String]) {
        if pp.is_empty() {
            return;
        }

        // Remove a previous results block (first matching title wins).
        for title in [
            "Results from openQA jobs:\n",
            "Results from incidents openQA jobs:\n",
            "Results from openQA incidents jobs:\n",
        ] {
            let Some(r_start) = self.template.iter().position(|l| l == title) else {
                continue;
            };
            let r_end = if let Some(end) = self
                .template
                .iter()
                .position(|l| l == "End of openQA Incidents results\n")
            {
                end + 1
            } else {
                match self.anchor_index() {
                    Some(anchor) => anchor - 1,
                    None => return,
                }
            };
            self.template.drain(r_start..r_end);
            break;
        }

        let Some(anchor) = self.anchor_index() else {
            return;
        };
        let mut index = anchor - 1;
        for line in pp.iter().rev() {
            self.template.insert(index, line.clone());
        }

        let Some(anchor) = self.anchor_index() else {
            return;
        };
        index = anchor - 1;
        self.template.insert(index, "\n".to_string());
        self.template
            .insert(index + 1, "End of openQA Incidents results\n".to_string());
        self.template.insert(index + 2, "\n".to_string());
    }

    /// Inserts the "installation tests done in openQA" note under the
    /// `Test results by product-arch:` header (upstream `BaseExport.install_results`).
    ///
    /// A no-op when that header is absent (the upstream `.index(...)` would
    /// raise; a missing header means the file is not mtui-shaped).
    pub fn install_results(&mut self) {
        let Some(index) = self
            .template
            .iter()
            .position(|l| l == "Test results by product-arch:\n")
        else {
            return;
        };
        self.template.insert(
            index + 3,
            "All installation tests done in openQA please see installlogs section\n".to_string(),
        );
        self.template.insert(index + 4, "\n".to_string());
    }

    /// The index of the `source code change review:` anchor, if present.
    fn anchor_index(&self) -> Option<usize> {
        self.template
            .iter()
            .position(|l| l == "source code change review:\n")
    }

    /// Path of the per-RRID install-logs directory
    /// (`template_dir/<rrid>/install_logs`).
    #[must_use]
    pub fn install_logs_dir(&self) -> PathBuf {
        self.config
            .template_dir
            .join(self.rrid.to_string())
            .join(&self.config.install_logs)
    }
}

/// The abstract exporter surface (upstream `BaseExport`'s `@abstractmethod`s).
///
/// Each concrete exporter embeds an [`ExportContext`] and implements the
/// workflow-specific log collection and run sequence.
#[async_trait::async_trait]
pub trait Exporter {
    /// Borrows the shared export context.
    fn ctx(&self) -> &ExportContext;

    /// Mutably borrows the shared export context.
    fn ctx_mut(&mut self) -> &mut ExportContext;

    /// Collects log files and returns their filenames (upstream `get_logs`).
    async fn get_logs(&mut self) -> Vec<String>;

    /// Runs the exporter, returning the finished template
    /// (upstream `run` — returns the mutated `FileList`/line list).
    async fn run(&mut self, prompt: &dyn OverwritePrompt) -> Vec<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(template: &[&str]) -> ExportContext {
        let mut cfg = Config::default();
        cfg.reports_url = "https://reports".to_string();
        cfg.install_logs = PathBuf::from("install_logs");
        cfg.session_user = "alice".to_string();
        let rrid = "SUSE:Maintenance:1:2".parse().unwrap();
        let lines: Vec<String> = template.iter().map(|s| (*s).to_string()).collect();
        ExportContext::new(cfg, &lines, false, rrid)
    }

    #[test]
    fn dedup_collapses_consecutive_nonblank_dups_keeps_blanks() {
        let mut c = ctx_with(&["a\n", "a\n", "b\n", "\n", "\n", "b\n"]);
        c.dedup_lines();
        assert_eq!(
            c.template,
            vec!["a\n", "b\n", "\n", "\n", "b\n"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn installlogs_lines_dedups_and_uses_reports_url() {
        let mut c = ctx_with(&["HAS_UNTRACKED\n", "body\n"]);
        c.installlogs_lines(&["h1.log".to_string(), "h1.log".to_string()]);
        let body = c.template.concat();
        assert!(body.contains("Links for update logs:\n"));
        // Duplicate filename only appears once.
        assert_eq!(
            body.matches("https://reports/SUSE:Maintenance:1:2/install_logs/h1.log")
                .count(),
            1
        );
    }

    #[test]
    fn add_sysinfo_appends_once() {
        let mut c = ctx_with(&["body\n"]);
        c.add_sysinfo();
        let len_after_first = c.template.len();
        assert!(c.template.last().unwrap().starts_with("## export MTUI:"));
        // Idempotent: appending again when it's already the last line is a no-op.
        c.add_sysinfo();
        assert_eq!(c.template.len(), len_after_first);
    }

    #[test]
    fn writer_skips_identical_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        std::fs::write(&path, "a\nb").unwrap();
        let c = ctx_with(&[]);
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
        let c = ctx_with(&[]);
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

    #[test]
    fn inject_openqa_noop_on_empty_pp() {
        let mut c = ctx_with(&["source code change review:\n"]);
        let before = c.template.clone();
        c.inject_openqa(&[]);
        assert_eq!(c.template, before);
    }

    #[test]
    fn inject_openqa_inserts_before_anchor() {
        let mut c = ctx_with(&["intro\n", "\n", "source code change review:\n"]);
        c.inject_openqa(&["job1 => PASSED\n".to_string()]);
        let body = c.template.concat();
        assert!(body.contains("job1 => PASSED"));
        assert!(body.contains("End of openQA Incidents results\n"));
        let job = body.find("job1 => PASSED").unwrap();
        let anchor = body.find("source code change review:").unwrap();
        assert!(job < anchor);
    }
}
