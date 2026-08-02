//! Action command tables: the per-`(release, transactional)` command templates
//! for install / uninstall / update / prepare / downgrade.
//!
//! ## Reference
//!
//! Each action's templates are keyed by `(release, transactional)` and
//! resolved through `substitute`, preserving `$$`
//! escaping and `${}` bracing (see [`template`] docs).
//!
//! The value type is [`ActionCommands`]: an owning bundle of the command
//! templates an action can carry. Not every action uses every field —
//! `installed_only` is prepare-only, `list_command` is downgrade-only, `reboot`
//! is transactional-only — so absent templates are `None`. Rendering a template
//! substitutes the variables an action interpolates (`$packages`, `$package`,
//! `$version`, `$repa`) and returns the ready-to-run command string.
//!
//! [`template`]: crate::update_workflow::template

pub(crate) mod downgrade;
pub(crate) mod install;
pub(crate) mod prepare;
pub(crate) mod uninstall;
pub(crate) mod update;

use std::collections::HashMap;

use crate::update_workflow::template::{TemplateError, safe_substitute, substitute};

/// Whether a template is rendered with strict `substitute` or lenient
/// `safe_substitute` semantics.
///
/// `install` / `uninstall` / `prepare` use [`Strict`](SubstMode::Strict);
/// `update` / `downgrade` use [`Safe`](SubstMode::Safe) because their templates
/// embed shell/awk `$`-tokens that must survive unresolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubstMode {
    /// Raise on a missing key / malformed placeholder.
    Strict,
    /// Leave a missing key / malformed placeholder verbatim.
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
/// `command` is always present; the remaining fields are optional and used
/// only by specific actions. `mode` records whether the
/// action's call site uses [`Strict`](SubstMode::Strict) or
/// [`Safe`](SubstMode::Safe) substitution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionCommands {
    /// The primary command template, always present.
    command: String,
    /// The transactional reboot template; present only for transactional
    /// (`slmicro`) entries.
    reboot: Option<String>,
    /// The "only if already installed" variant; present only for `prepare`
    /// actions.
    installed_only: Option<String>,
    /// The package-listing helper; present only for `downgrade` actions.
    list_command: Option<String>,
    /// The substitution mode for this action's templates.
    mode: SubstMode,
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
    pub(crate) fn render_command(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<String, TemplateError> {
        self.mode.render(&self.command, vars)
    }

    /// Renders [`reboot`](Self::reboot) if present. The reboot template takes no
    /// variables, so an empty map is passed (always strict — it has no
    /// placeholders).
    ///
    /// # Errors
    ///
    /// Propagates [`TemplateError`] (never expected — the reboot template has no
    /// placeholders).
    pub(crate) fn render_reboot(&self) -> Result<Option<String>, TemplateError> {
        match &self.reboot {
            Some(t) => Ok(Some(substitute(t, &HashMap::new())?)),
            None => Ok(None),
        }
    }

    /// The raw, unrendered [`command`](Self::command) template.
    ///
    /// Needed by the [`PlanProvider`](mtui_hosts::PlanProvider) adapter, which
    /// hands the template to a [`Doer`](mtui_hosts::Doer) that performs the
    /// `$packages` substitution itself (the package list is only known inside
    /// `mtui-hosts`, at
    /// [`Operation::collect`](mtui_hosts::Operation::collect) time). Prefer
    /// [`render_command`](Self::render_command) everywhere else.
    pub(crate) fn command_template(&self) -> &str {
        &self.command
    }

    /// The raw, unrendered [`reboot`](Self::reboot) template, if any. See
    /// [`command_template`](Self::command_template).
    pub(crate) fn reboot_template(&self) -> Option<&str> {
        self.reboot.as_deref()
    }

    /// Renders [`installed_only`](Self::installed_only) with `vars` if present,
    /// honouring the action's [`mode`](Self::mode).
    ///
    /// # Errors
    ///
    /// In strict mode, propagates [`TemplateError`] from the underlying
    /// substitution.
    pub(crate) fn render_installed_only(
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
    pub(crate) fn render_list_command(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> Result<Option<String>, TemplateError> {
        match &self.list_command {
            Some(t) => Ok(Some(self.mode.render(t, vars)?)),
            None => Ok(None),
        }
    }
}
