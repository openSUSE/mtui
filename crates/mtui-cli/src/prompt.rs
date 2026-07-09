//! The interactive prompt string.
//!
//! [`MtuiPrompt`] renders two segments from the live [`Session`]:
//!
//! * **Left prompt** — the workflow-aware `mtui[-mode]> ` (upstream
//!   `repl.py::set_prompt`): plain `mtui> ` for [`Workflow::Manual`], and
//!   `mtui-<mode]> ` (e.g. `mtui-kernel> `, `mtui-auto> `) otherwise.
//! * **Right prompt** — the `mode / hosts / templates / active:RRID` status line
//!   (upstream `repl.py::_bottom_toolbar`). reedline 0.49 has no bottom-toolbar
//!   segment, so the status lives in the right prompt — the closest persistent
//!   analogue.
//!
//! Both read the shared `Arc<Mutex<Session>>` the [`Repl`](crate::repl::Repl)
//! loop and the [`MtuiCompleter`](crate::completer::MtuiCompleter) already share.
//! reedline calls `render_*` synchronously *during* `read_line`, the same window
//! the completer locks in; per-line dispatch locks the session only *after*
//! `read_line` returns, so these short read-locks never overlap the dispatch
//! lock (same soundness argument as the completer — see its module docs).

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use mtui_core::Session;
use mtui_types::enums::Workflow;
use reedline::{Prompt, PromptEditMode, PromptHistorySearch};

/// The mtui REPL prompt, reading live [`Session`] state on each render.
///
/// Holds a clone of the same `Arc<Mutex<Session>>` the loop drives so the prompt
/// reflects the active workflow, loaded RRID, and host/template counts as they
/// change mid-session (upstream's live `CommandPrompt` reference).
#[derive(Clone)]
pub struct MtuiPrompt {
    session: Arc<Mutex<Session>>,
}

impl MtuiPrompt {
    /// Builds a prompt sharing `session` with the REPL loop and completer.
    #[must_use]
    pub fn new(session: Arc<Mutex<Session>>) -> Self {
        Self { session }
    }
}

impl Prompt for MtuiPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Upstream `set_prompt`: bare `mtui` for MANUAL, `mtui-<mode>` otherwise.
        let workflow = session.metadata().workflow();
        let prompt = match workflow {
            Workflow::Manual => "mtui> ".to_owned(),
            other => format!("mtui-{other}> "),
        };
        Cow::Owned(prompt)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Upstream `_bottom_toolbar`: mode / hosts / templates / active(RRID).
        let mode = session.metadata().workflow();
        let n_hosts = session.targets().len();
        let n_templates = session.templates.len();
        let rrid = session.metadata().id();
        let active = if rrid.is_empty() { "-" } else { &rrid };
        Cow::Owned(format!(
            " mode: {mode}  hosts: {n_hosts}  templates: {n_templates}  active: {active} "
        ))
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_config::Config;
    use mtui_testreport::{ObsReport, TestReport};
    use mtui_types::RequestReviewID;
    use mtui_types::enums::Workflow;
    use reedline::{PromptHistorySearchStatus, PromptViMode};

    fn session() -> Arc<Mutex<Session>> {
        Arc::new(Mutex::new(Session::new(Config::default(), true)))
    }

    /// Seeds a loaded `ObsReport` (RRID + optional workflow) as the active
    /// template, mirroring `mtui-core`'s own `seed_active_report` test helper.
    fn seed(session: &Arc<Mutex<Session>>, rrid: &str, workflow: Workflow) {
        let mut s = session.lock().unwrap();
        let mut report = ObsReport::new(s.config.clone());
        report.base_mut().rrid = Some(RequestReviewID::parse(rrid).unwrap());
        report.base_mut().workflow = workflow;
        s.templates.add(Box::new(report));
        s.templates.set_active(rrid);
    }

    #[test]
    fn left_prompt_is_bare_mtui_for_manual() {
        // A fresh session's active is the null report (workflow MANUAL default).
        let p = MtuiPrompt::new(session());
        assert_eq!(p.render_prompt_left(), "mtui> ");
    }

    #[test]
    fn left_prompt_appends_non_manual_workflow() {
        let s = session();
        seed(&s, "SUSE:Maintenance:1:1", Workflow::Kernel);
        assert_eq!(
            MtuiPrompt::new(Arc::clone(&s)).render_prompt_left(),
            "mtui-kernel> "
        );
        seed(&s, "SUSE:Maintenance:2:2", Workflow::Auto);
        // The most recently seeded RRID is active.
        assert_eq!(MtuiPrompt::new(s).render_prompt_left(), "mtui-auto> ");
    }

    #[test]
    fn right_prompt_shows_dash_active_when_unloaded() {
        let p = MtuiPrompt::new(session());
        let right = p.render_prompt_right();
        assert!(right.contains("active: -"), "got: {right:?}");
        assert!(right.contains("hosts: 0"), "got: {right:?}");
        assert!(right.contains("templates: 0"), "got: {right:?}");
        assert!(right.contains("mode: manual"), "got: {right:?}");
    }

    #[test]
    fn right_prompt_shows_rrid_and_counts_when_loaded() {
        let s = session();
        seed(&s, "SUSE:Maintenance:1:1", Workflow::Auto);
        let p = MtuiPrompt::new(s);
        let right = p.render_prompt_right();
        assert!(
            right.contains("active: SUSE:Maintenance:1:1"),
            "got: {right:?}"
        );
        assert!(right.contains("templates: 1"), "got: {right:?}");
        assert!(right.contains("mode: auto"), "got: {right:?}");
    }

    #[test]
    fn other_segments_are_empty() {
        let p = MtuiPrompt::new(session());
        assert_eq!(p.render_prompt_indicator(PromptEditMode::Default), "");
        assert_eq!(
            p.render_prompt_indicator(PromptEditMode::Vi(PromptViMode::Insert)),
            ""
        );
        assert_eq!(p.render_prompt_multiline_indicator(), "");
        assert_eq!(
            p.render_prompt_history_search_indicator(PromptHistorySearch::new(
                PromptHistorySearchStatus::Passing,
                "x".to_owned(),
            )),
            ""
        );
    }
}
