//! Reconstruct a `clap` argv token list from a tool-call kwargs dict.
//!
//! The **inverse of [`crate::schema`]**. An MCP tool call arrives as a
//! `{param_name: value}` object (kwargs). To dispatch it through the *same*
//! engine the REPL uses ([`mtui_core::dispatch_argv`]), we must turn that object
//! back into the `argv` token list `clap` re-parses. This module does that
//! reconstruction purely, introspecting the command's built [`clap::Command`] —
//! the identical `get_arguments()` surface [`crate::schema`] reads.
//!
//! Port of upstream `mtui/mcp/_argv.py::kwargs_to_argv`.
//!
//! # Deliberate deviations from upstream
//!
//! * **No synthetic-dest / const-mutex routing.** Upstream carries a
//!   `_mtui_synthetic_dests` pass and a StoreConst-mutex pass to un-collapse
//!   argparse mutually-exclusive groups that share one `dest` (`load_template
//!   -a/-k`, `set_repo -A/-R`). In `clap` each group member carries a *distinct*
//!   arg id (`auto`/`kernel`, `add`/`remove`), so its kwarg maps straight to its
//!   own long flag through the normal loop — no special-casing.
//! * **No "exactly one required" pre-check.** `clap::ArgGroup` enforces that when
//!   the reconstructed argv is re-parsed by the engine, which already surfaces a
//!   clean [`mtui_core::EngineError::Parse`]. Upstream pre-validated only to
//!   dodge argparse's ugly usage dump.
//!
//! # Ordering
//!
//! Plain optional flags are emitted first (in `get_arguments()` order), then the
//! positional tail. Append / multi-value flags are routed into the tail as well:
//! such a flag consumes every token after it, so a later flag (notably the base
//! parser's `-T/--template`, declared after per-command args) must not sit behind
//! it — mirroring upstream's REMAINDER handling.

use clap::{Arg, ArgAction};
use serde_json::{Map, Value};

/// Re-encode a tool-call kwargs dict as `clap`-compatible argv.
///
/// `argv_prefix` is prepended verbatim — used by the P7.6 subparser fan-out to
/// inject a subcommand name (`["show"]` for the `config_show` tool). Args absent
/// from `kwargs` or whose value is JSON `null` are skipped. Flags come first,
/// then the positional tail (see the module docs for why).
#[must_use]
pub(crate) fn kwargs_to_argv(
    cmd: &clap::Command,
    kwargs: &Map<String, Value>,
    argv_prefix: &[String],
) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();

    for arg in cmd.get_arguments() {
        let id = arg.get_id().as_str();
        if id == "help" || id == "version" {
            continue;
        }
        let Some(value) = kwargs.get(id) else {
            continue;
        };
        if value.is_null() {
            continue;
        }

        // ---- boolean-shaped flags ----------------------------------------
        // Emit the long flag only when the value is the flag's "on" side.
        match arg.get_action() {
            ArgAction::SetTrue => {
                if value.as_bool() == Some(true) {
                    flags.push(long_flag(arg));
                }
                continue;
            }
            ArgAction::SetFalse => {
                if value.as_bool() == Some(false) {
                    flags.push(long_flag(arg));
                }
                continue;
            }
            _ => {}
        }

        let items = as_items(value);
        if items.is_empty() {
            continue;
        }

        // ---- positional ---------------------------------------------------
        if is_positional(arg) {
            tail.extend(items);
            continue;
        }

        // ---- append / multi-value flag -----------------------------------
        // Emit the flag once followed by every token, into the tail so a later
        // flag cannot be swallowed as one of its values.
        if is_multi(arg) {
            tail.push(long_flag(arg));
            tail.extend(items);
            continue;
        }

        // ---- optional scalar flag ----------------------------------------
        flags.push(long_flag(arg));
        flags.extend(items);
    }

    let mut out = Vec::with_capacity(argv_prefix.len() + flags.len() + tail.len());
    out.extend_from_slice(argv_prefix);
    out.append(&mut flags);
    out.append(&mut tail);
    out
}

/// The long `--flag` form, falling back to the first option string.
///
/// Every mtui optional with a short form also has a long form; the fallback is
/// defensive.
fn long_flag(arg: &Arg) -> String {
    arg.get_long()
        .map(|l| format!("--{l}"))
        .or_else(|| arg.get_short().map(|c| format!("-{c}")))
        .unwrap_or_else(|| arg.get_id().to_string())
}

/// Whether an arg is a positional (takes no `--flag`).
fn is_positional(arg: &Arg) -> bool {
    arg.get_long().is_none() && arg.get_short().is_none()
}

/// Whether a value-taking arg accepts multiple tokens after one flag.
///
/// `ArgAction::Append` or a `num_args` upper bound above one. Kept in sync with
/// [`crate::schema`]'s `is_list_arg`.
fn is_multi(arg: &Arg) -> bool {
    if matches!(arg.get_action(), ArgAction::Append) {
        return true;
    }
    arg.get_num_args().is_some_and(|r| r.max_values() > 1)
}

/// Flatten a JSON value into the argv tokens it contributes.
///
/// Arrays fan out per element; scalars render to a single token. `null` is
/// filtered by the caller, but nested `null`s inside an array are dropped here.
fn as_items(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(render_scalar).collect(),
        other => render_scalar(other).into_iter().collect(),
    }
}

/// Render a single non-array JSON value into an argv token, or `None` for `null`.
fn render_scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        // Objects/arrays are not valid CLI scalars; stringify defensively so the
        // downstream parse error names the offending value rather than panicking.
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_core::{Registry, command_parser, register_all};
    use serde_json::json;

    /// Build a command's full parser (base template flags + its own `configure`)
    /// through the same `mtui-core` builder `dispatch_argv` re-parses, so this
    /// test exercises exactly the parser real dispatch sees.
    fn parser_for(command: &str) -> clap::Command {
        let registry: Registry = register_all();
        let cmd = registry
            .get(command)
            .unwrap_or_else(|| panic!("command not registered: {command}"));
        command_parser(cmd.as_ref())
    }

    fn argv(command: &str, kwargs: Value) -> Vec<String> {
        let parser = parser_for(command);
        kwargs_to_argv(&parser, kwargs.as_object().unwrap(), &[])
    }

    /// The reconstructed argv must re-parse cleanly through the same parser.
    fn assert_reparses(command: &str, tokens: &[String]) {
        let parser = parser_for(command);
        parser
            .try_get_matches_from(tokens)
            .unwrap_or_else(|e| panic!("argv {tokens:?} failed to re-parse: {e}"));
    }

    // ------------------------------------------------------------- booleans

    #[test]
    fn store_true_on_emits_flag_off_omits() {
        // `openqa_overview --export` (SetTrue).
        let on = argv("openqa_overview", json!({ "export": true }));
        assert_eq!(on, vec!["--export"]);
        let off = argv("openqa_overview", json!({ "export": false }));
        assert!(off.is_empty(), "false SetTrue emits nothing: {off:?}");
        assert_reparses("openqa_overview", &on);
    }

    #[test]
    fn store_false_off_side_emits_flag() {
        // No mtui command uses SetFalse; pin the branch on a bespoke parser.
        let parser = clap::Command::new("probe").no_binary_name(true).arg(
            clap::Arg::new("color")
                .long("no-color")
                .action(ArgAction::SetFalse),
        );
        let off = kwargs_to_argv(&parser, json!({ "color": false }).as_object().unwrap(), &[]);
        assert_eq!(off, vec!["--no-color"]);
        let on = kwargs_to_argv(&parser, json!({ "color": true }).as_object().unwrap(), &[]);
        assert!(on.is_empty(), "true SetFalse emits nothing: {on:?}");
    }

    // -------------------------------------------------------------- scalars

    #[test]
    fn int_scalar_flag_renders_number_token() {
        // `openqa_overview --days` (u32 option).
        let out = argv("openqa_overview", json!({ "days": 5 }));
        assert_eq!(out, vec!["--days", "5"]);
        assert_reparses("openqa_overview", &out);
    }

    #[test]
    fn required_int_positional_lands_in_tail() {
        // `set_timeout timeout` (required u64 positional).
        let out = argv("set_timeout", json!({ "timeout": 30 }));
        assert_eq!(out, vec!["30"]);
        assert_reparses("set_timeout", &out);
    }

    #[test]
    fn enum_positional_emits_value() {
        // `set_log_level level` (PossibleValues positional).
        let out = argv("set_log_level", json!({ "level": "info" }));
        assert_eq!(out, vec!["info"]);
        assert_reparses("set_log_level", &out);
    }

    #[test]
    fn regenerate_rrid_maps_to_positional_not_template_selector() {
        // The load/regenerate catch-22 fix: `{rrid: <RRID>}` must land on the
        // `regenerate` positional (a bare token), NOT the base `-T/--template`
        // loaded-template selector — otherwise the engine rejects the unloaded
        // RRID with `Template not loaded`.
        let out = argv("regenerate", json!({ "rrid": "SUSE:SLFO:1.2:6311" }));
        assert_eq!(out, vec!["SUSE:SLFO:1.2:6311"]);
        assert!(
            !out.iter().any(|t| t == "--template" || t == "-T"),
            "RRID must not route through the -T selector: {out:?}"
        );
        assert_reparses("regenerate", &out);
    }

    #[test]
    fn regenerate_kernel_hint_and_rrid_round_trip() {
        let out = argv(
            "regenerate",
            json!({ "kernel": true, "rrid": "SUSE:Maintenance:1:1" }),
        );
        // Boolean flag first, positional in the tail.
        assert_eq!(out, vec!["--kernel", "SUSE:Maintenance:1:1"]);
        assert_reparses("regenerate", &out);
    }

    // ---------------------------------------------------------------- lists

    #[test]
    fn append_multi_flag_emits_once_into_tail() {
        // `commit -m/--msg` (Append, num_args 1..): one flag then every token,
        // in the positional tail so a trailing base flag stays safe.
        let out = argv(
            "commit",
            json!({ "msg": ["hello", "world"], "template": "a:b:1:1" }),
        );
        // `-T` (flag) precedes the tail-routed `--msg`.
        assert_eq!(
            out,
            vec!["--template", "a:b:1:1", "--msg", "hello", "world"]
        );
        assert_reparses("commit", &out);
    }

    #[test]
    fn append_list_flag_with_choices_emits_once() {
        // `openqa_overview --aggregated-groups` (Append + PossibleValues).
        let parser = parser_for("openqa_overview");
        // Discover a valid enum member from the parser so the re-parse succeeds.
        let arg = parser
            .get_arguments()
            .find(|a| a.get_id() == "aggregated_groups")
            .expect("aggregated_groups arg");
        let member = arg
            .get_possible_values()
            .first()
            .map(|pv| pv.get_name().to_owned())
            .expect("aggregated_groups has choices");
        let out = argv(
            "openqa_overview",
            json!({ "aggregated_groups": [member.clone()] }),
        );
        assert_eq!(out, vec!["--aggregated-groups".to_owned(), member]);
        assert_reparses("openqa_overview", &out);
    }

    // ------------------------------------------------------------ omission

    #[test]
    fn absent_optional_positional_emits_nothing() {
        // `export filename` (optional positional) omitted → empty argv.
        let out = argv("export", json!({}));
        assert!(out.is_empty(), "absent arg emits nothing: {out:?}");
    }

    #[test]
    fn null_value_is_skipped() {
        let out = argv("openqa_overview", json!({ "days": null, "export": null }));
        assert!(out.is_empty(), "null values skipped: {out:?}");
    }

    // -------------------------------------------------------- base flags

    #[test]
    fn base_template_flag_round_trips() {
        let out = argv("whoami", json!({ "template": "SUSE:Maintenance:1:1" }));
        assert_eq!(out, vec!["--template", "SUSE:Maintenance:1:1"]);
        assert_reparses("whoami", &out);
    }

    #[test]
    fn all_templates_bool_round_trips() {
        let out = argv("whoami", json!({ "all_templates": true }));
        assert_eq!(out, vec!["--all-templates"]);
        assert_reparses("whoami", &out);
    }

    #[test]
    fn per_command_flags_precede_tail_routed_multi() {
        // A plain scalar flag comes before an append flag routed to the tail.
        let out = argv("commit", json!({ "msg": ["m"], "all_templates": true }));
        assert_eq!(out, vec!["--all-templates", "--msg", "m"]);
        assert_reparses("commit", &out);
    }

    // ------------------------------------------------------- mutex group

    #[test]
    fn mutex_group_member_emits_its_own_flag() {
        // `load_template` -a/-k are distinct clap ids; `auto` maps straight to
        // its long flag with no synthetic routing.
        let out = argv("load_template", json!({ "auto": "SUSE:Maintenance:1:1" }));
        assert_eq!(out, vec!["--auto-review-id", "SUSE:Maintenance:1:1"]);
        assert_reparses("load_template", &out);
    }

    // ---------------------------------------------------------- prefix

    #[test]
    fn argv_prefix_is_prepended() {
        // P7.6 fans `config` out per-subcommand and passes the *subparser* here
        // (its args live on `set`, not the parent). Mirror that: introspect the
        // `set` subcommand and prepend its name as the prefix.
        let parent = parser_for("config");
        let set = parent
            .get_subcommands()
            .find(|c| c.get_name() == "set")
            .expect("config has a `set` subcommand");
        let out = kwargs_to_argv(
            set,
            json!({ "attribute": "template_dir", "value": "/tmp" })
                .as_object()
                .unwrap(),
            &["set".to_owned()],
        );
        assert_eq!(out, vec!["set", "template_dir", "/tmp"]);
        // The full `config set …` argv re-parses through the parent parser.
        assert_reparses("config", &out);
    }

    // --------------------------------------------------------- helpers

    #[test]
    fn help_and_version_ids_are_ignored() {
        let out = argv("whoami", json!({ "help": true, "version": true }));
        assert!(out.is_empty(), "help/version never round-trip: {out:?}");
    }
}
