//! Shared, explicitly-passed command state (`Session`).
//!
//! The Rust replacement for upstream's `CommandPrompt` god-object. Commands
//! receive `&mut Session` and read/mutate its state through methods — there are
//! no hidden globals. It owns the [`Config`], the [`TemplateRegistry`] (loaded
//! templates + active pointer), the [`CommandPromptDisplay`] output sink, and
//! the `interactive` flag that distinguishes the REPL (`true`) from headless
//! callers such as `mtui-mcp` (`false`).
//!
//! The scalar `metadata` / `targets` accessors upstream exposes as read-only
//! properties are provided here as [`metadata`](Session::metadata) /
//! [`targets`](Session::targets), delegating to the active report so command
//! bodies and tests keep working as the registry grows past one entry.

use mtui_config::Config;
use mtui_hosts::HostsGroup;
use mtui_testreport::TestReport;

use crate::display::CommandPromptDisplay;
use crate::template_registry::TemplateRegistry;

/// The explicitly-passed state every command operates on.
pub struct Session {
    /// The application configuration.
    pub config: Config,
    /// Loaded templates and the active pointer.
    pub templates: TemplateRegistry,
    /// Formatted-output sink.
    pub display: CommandPromptDisplay,
    /// `true` for the interactive REPL, `false` for headless callers (MCP).
    ///
    /// Drives the fan-out default: with several templates loaded and no
    /// interactive `switch` to pick an active one, an otherwise-unscoped command
    /// fans out across every template instead of silently picking one.
    pub interactive: bool,
    /// Set by the `quit` command to ask the interactive REPL loop to exit after
    /// the current dispatch returns.
    ///
    /// The Rust replacement for upstream `Quit` raising `SystemExit`/returning a
    /// truthy value from `onecmd`: rather than routing process-exit through the
    /// command error channel, `quit` flips this flag and returns `Ok(())`; the
    /// Phase-6 REPL checks [`should_exit`](Self::should_exit) after each line and
    /// breaks its loop. Headless callers (MCP) ignore it.
    should_exit: bool,
}

impl Session {
    /// Builds a session for `config`, defaulting the display to stdout.
    ///
    /// `interactive` mirrors upstream: `true` for the REPL, `false` for MCP.
    #[must_use]
    pub fn new(config: Config, interactive: bool) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        Self {
            config,
            templates,
            display: CommandPromptDisplay::stdout(),
            interactive,
            should_exit: false,
        }
    }

    /// Builds a session with an explicit display sink (test/embedding seam).
    #[must_use]
    pub fn with_display(config: Config, interactive: bool, display: CommandPromptDisplay) -> Self {
        let templates = TemplateRegistry::new(config.clone());
        Self {
            config,
            templates,
            display,
            interactive,
            should_exit: false,
        }
    }

    /// The active report (upstream `prompt.metadata`). Never `None` — the
    /// [`TemplateRegistry`] returns a null object when nothing is loaded.
    #[must_use]
    pub fn metadata(&self) -> &(dyn TestReport + Send + Sync) {
        self.templates.active()
    }

    /// The active report's connected targets (upstream `prompt.targets`).
    #[must_use]
    pub fn targets(&self) -> &HostsGroup {
        &self.templates.active().base().targets
    }

    /// Mutably borrows the active report's connected targets.
    ///
    /// The mutable counterpart of [`targets`](Self::targets); command bodies
    /// that fan a command out across hosts (`run`, `reboot`, `set_repo`) need
    /// `&mut HostsGroup`.
    pub fn targets_mut(&mut self) -> &mut HostsGroup {
        &mut self.templates.active_mut().base_mut().targets
    }

    /// Moves the active report's targets out, leaving an empty group in place.
    ///
    /// The counterpart to [`restore_targets`](Self::restore_targets). The
    /// report's `perform_*` methods take `&self` **and** `&mut HostsGroup`;
    /// because the targets live inside the active report, a single
    /// `&mut Box<dyn TestReport>` cannot hand out both borrows at once. Taking
    /// the group out by value breaks that tie: the caller then holds an owned
    /// `HostsGroup` (no borrow of `self`) and can freely re-borrow the report via
    /// [`metadata`](Self::metadata) to drive `perform_*`, restoring the group
    /// afterwards.
    ///
    /// Mirrors upstream, where a command reads `self.metadata` and `self.targets`
    /// as two views of the same active report.
    #[must_use]
    pub fn take_targets(&mut self) -> HostsGroup {
        let interactive = self.interactive;
        std::mem::replace(
            &mut self.templates.active_mut().base_mut().targets,
            HostsGroup::new(Vec::new(), interactive),
        )
    }

    /// Restores the active report's targets, undoing [`take_targets`](Self::take_targets).
    pub fn restore_targets(&mut self, targets: HostsGroup) {
        self.templates.active_mut().base_mut().targets = targets;
    }

    /// Requests that the interactive REPL loop exit after the current dispatch.
    ///
    /// Set by the `quit` command; read by the Phase-6 REPL via
    /// [`should_exit`](Self::should_exit).
    pub fn request_exit(&mut self) {
        self.should_exit = true;
    }

    /// Whether the `quit` command has asked the REPL loop to exit.
    #[must_use]
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn fresh_session_active_is_null_and_unloaded() {
        let s = Session::new(config(), true);
        assert!(!s.metadata().is_loaded());
        assert!(s.templates.is_empty());
        assert_eq!(s.metadata().id(), "");
    }

    #[test]
    fn interactive_flag_is_honored() {
        assert!(Session::new(config(), true).interactive);
        assert!(!Session::new(config(), false).interactive);
    }

    #[test]
    fn targets_of_unloaded_session_is_empty() {
        let s = Session::new(config(), true);
        assert!(s.targets().is_empty());
    }

    #[test]
    fn with_display_uses_supplied_sink() {
        use crate::display::{ColorMode, CommandPromptDisplay};
        let display = CommandPromptDisplay::with_sink(Box::new(Vec::new()), ColorMode::Always);
        let s = Session::with_display(config(), false, display);
        assert_eq!(s.display.color(), ColorMode::Always);
        assert!(!s.interactive);
    }
}
