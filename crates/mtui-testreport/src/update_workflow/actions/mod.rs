//! Action command tables: the per-`(release, transactional)` command templates
//! for install / uninstall / update / prepare / downgrade.
//!
//! ## Reference
//!
//! Ports upstream `mtui/update_workflow/actions/`. Each upstream module builds a
//! `DictWithInjections` keyed by `(release, transactional)` whose values are
//! dicts of `string.Template` commands. This port keeps the **template strings
//! verbatim** and resolves them through [`crate::update_workflow::substitute`],
//! preserving `$$` escaping and `${}` bracing (see [`template`] docs).
//!
//! The value type is [`ActionCommands`]: an owning bundle of the command
//! templates an action can carry. Not every action uses every field —
//! `installed_only` is prepare-only, `list_command` is downgrade-only, `reboot`
//! is transactional-only — so absent templates are `None`. Rendering a template
//! substitutes the variables an action interpolates (`$packages`, `$package`,
//! `$version`, `$repa`) and returns the ready-to-run command string.
//!
//! [`template`]: crate::update_workflow::template

pub mod downgrade;
pub mod install;
pub mod prepare;
pub mod uninstall;
pub mod update;

use std::collections::HashMap;

use crate::update_workflow::template::{TemplateError, safe_substitute, substitute};

/// Whether a template is rendered with strict [`substitute`] or lenient
/// [`safe_substitute`] semantics.
///
/// Mirrors upstream's choice of `.substitute` vs `.safe_substitute` per action:
/// `install` / `uninstall` / `prepare` are [`Strict`](SubstMode::Strict);
/// `update` / `downgrade` are [`Safe`](SubstMode::Safe) because their templates
/// embed shell/awk `$`-tokens that must survive unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstMode {
    /// Raise on a missing key / malformed placeholder (upstream `.substitute`).
    Strict,
    /// Leave a missing key / malformed placeholder verbatim (upstream
    /// `.safe_substitute`).
    Safe,
}

impl SubstMode {
    /// Renders `template` with `vars` under this mode.
    fn render(self, template: &str, vars: &HashMap<&str, &str>) -> Result<String, TemplateError> {
        match self {
            SubstMode::Strict => substitute(template, vars),
            SubstMode::Safe => Ok(safe_substitute(template, vars)),
        }
    }
}

/// The command templates for one resolved action.
///
/// The Rust analogue of upstream's per-key `{"command": Template, ...}` dict.
/// `command` is always present; the remaining fields mirror the optional keys
/// upstream stores for specific actions. [`mode`](Self::mode) records whether
/// the action's call site used `.substitute` or `.safe_substitute`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionCommands {
    /// The primary command template (upstream `"command"`), always present.
    pub command: String,
    /// The transactional reboot template (upstream `"reboot"`); present only for
    /// transactional (`slmicro`) entries.
    pub reboot: Option<String>,
    /// The "only if already installed" variant (upstream `"installed_only"`);
    /// present only for `prepare` actions.
    pub installed_only: Option<String>,
    /// The package-listing helper (upstream `"list_command"`); present only for
    /// `downgrade` actions.
    pub list_command: Option<String>,
    /// The substitution mode for this action's templates.
    pub mode: SubstMode,
}

impl ActionCommands {
    /// A strict command-only action set (no reboot / installed_only /
    /// list_command).
    #[must_use]
    fn command_only(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            reboot: None,
            installed_only: None,
            list_command: None,
            mode: SubstMode::Strict,
        }
    }

    /// A strict command + reboot action set (the transactional shape).
    #[must_use]
    fn with_reboot(command: impl Into<String>, reboot: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            reboot: Some(reboot.into()),
            installed_only: None,
            list_command: None,
            mode: SubstMode::Strict,
        }
    }

    /// Overrides the substitution [`mode`](Self::mode) (builder-style).
    #[must_use]
    fn with_mode(mut self, mode: SubstMode) -> Self {
        self.mode = mode;
        self
    }

    /// Renders [`command`](Self::command) with `vars` substituted, honouring the
    /// action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In [`Strict`](SubstMode::Strict) mode, propagates [`TemplateError`] from
    /// the underlying substitution (missing key or malformed placeholder).
    pub fn render_command(&self, vars: &HashMap<&str, &str>) -> Result<String, TemplateError> {
        self.mode.render(&self.command, vars)
    }

    /// Renders [`reboot`](Self::reboot) if present. The reboot template takes no
    /// variables upstream, so an empty map is passed (always strict — it has no
    /// placeholders).
    ///
    /// # Errors
    ///
    /// Propagates [`TemplateError`] (never expected — the reboot template has no
    /// placeholders).
    pub fn render_reboot(&self) -> Result<Option<String>, TemplateError> {
        match &self.reboot {
            Some(t) => Ok(Some(substitute(t, &HashMap::new())?)),
            None => Ok(None),
        }
    }

    /// Renders [`installed_only`](Self::installed_only) with `vars` if present,
    /// honouring the action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In strict mode, propagates [`TemplateError`] from the underlying
    /// substitution.
    pub fn render_installed_only(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<Option<String>, TemplateError> {
        match &self.installed_only {
            Some(t) => Ok(Some(self.mode.render(t, vars)?)),
            None => Ok(None),
        }
    }

    /// Renders [`list_command`](Self::list_command) with `vars` if present,
    /// honouring the action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In strict mode, propagates [`TemplateError`] from the underlying
    /// substitution.
    pub fn render_list_command(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<Option<String>, TemplateError> {
        match &self.list_command {
            Some(t) => Ok(Some(self.mode.render(t, vars)?)),
            None => Ok(None),
        }
    }
}
