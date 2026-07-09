//! The interactive prompt string.
//!
//! P6.2 ships a **minimal, static** [`Prompt`] implementation rendering a bare
//! `mtui> ` left prompt. Upstream `repl.py::set_prompt` refreshes the prompt
//! from the active workflow (`mtui-<mode]> `) and P6.5 will grow the dynamic
//! prompt + RRID bottom toolbar here; keeping it in its own module means that
//! task only touches this file, not the [`Repl`](crate::repl::Repl) loop.

use std::borrow::Cow;

use reedline::{Prompt, PromptEditMode, PromptHistorySearch};

/// The mtui REPL prompt.
///
/// Renders the static left prompt `mtui> `; every other segment (right prompt,
/// indicator, multiline, history-search) is empty. P6.5 replaces this with the
/// workflow-aware prompt and toolbar.
#[derive(Debug, Default, Clone, Copy)]
pub struct MtuiPrompt;

impl Prompt for MtuiPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("mtui> ")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
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
    use reedline::{PromptHistorySearchStatus, PromptViMode};

    #[test]
    fn left_prompt_is_static_mtui() {
        assert_eq!(MtuiPrompt.render_prompt_left(), "mtui> ");
    }

    #[test]
    fn other_segments_are_empty() {
        let p = MtuiPrompt;
        assert_eq!(p.render_prompt_right(), "");
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
