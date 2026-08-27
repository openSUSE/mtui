//! Reconstruct a `clap` argv token list from a tool-call kwargs dict.
//!
//! The **inverse of [`crate::schema`]**: a tool call arrives as a
//! `{param_name: value}` object, and dispatching it through the *same* engine
//! the REPL uses ([`mtui_core::dispatch_argv`]) means turning that object back
//! into the argv `clap` re-parses. Done purely, introspecting the command's
//! built [`clap::Command`] — the identical `get_arguments()` surface
//! [`crate::schema`] reads.
//!
//! Deliberately absent: synthetic-dest routing for mutually-exclusive groups
//! (clap already gives each member a distinct arg id, so its kwarg maps to its
//! own long flag through the normal loop), and an "exactly one required"
//! pre-check (`clap::ArgGroup` enforces that on re-parse, as a clean
//! [`mtui_core::EngineError::Parse`]).
//!
//! # Ordering
//!
//! Plain optional flags first (in `get_arguments()` order), then the positional
//! tail. Append / multi-value flags route into the tail too: a greedy
//! (`num_args(1..)`) one consumes every token after it, so a later flag
//! (notably the base parser's `-T/--template`, declared after per-command args)
//! must not sit behind it. An `Append` flag is emitted once per element (the only
//! form clap parses when each occurrence takes one value); a non-`Append`
//! multi-value option once with all its values (see the loop below).

use clap::{Arg, ArgAction};
use serde_json::{Map, Value};

/// Re-encode a tool-call kwargs dict as `clap`-compatible argv.
///
/// `argv_prefix` is prepended verbatim — used by the MCP tool synthesiser's
/// subparser fan-out to inject a subcommand name (`["show"]` for the
/// `config_show` tool). Args absent from `kwargs` or whose value is JSON `null`
/// are skipped. Flags come first, then the positional tail (module docs for why).
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

        // Boolean-shaped: emit the long flag only on the flag's "on" side.
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

        if is_positional(arg) {
            tail.extend(items);
            continue;
        }

        // Both shapes route into the tail so a later flag cannot be swallowed as
        // one of their values, but emit differently: `Append` repeats the flag
        // before *every* element (the only form a default `num_args = 1` Append
        // arg parses), while a non-`Append` multi-value option consumes many
        // values from one occurrence — repeating it would make the last
        // occurrence win and silently drop the earlier values.
        if is_multi(arg) {
            if matches!(arg.get_action(), ArgAction::Append) {
                for item in items {
                    tail.push(long_flag(arg));
                    tail.push(item);
                }
            } else {
                tail.push(long_flag(arg));
                tail.extend(items);
            }
            continue;
        }

        flags.push(long_flag(arg));
        flags.extend(items);
    }

    let mut out = Vec::with_capacity(argv_prefix.len() + flags.len() + tail.len());
    out.extend_from_slice(argv_prefix);
    out.append(&mut flags);
    out.append(&mut tail);
    out
}

/// The long `--flag` form. Every mtui optional with a short form also has a
/// long form, so the short/id fallbacks are purely defensive.
fn long_flag(arg: &Arg) -> String {
    arg.get_long()
        .map(|l| format!("--{l}"))
        .or_else(|| arg.get_short().map(|c| format!("-{c}")))
        .unwrap_or_else(|| arg.get_id().to_string())
}

/// Whether an arg is a positional.
fn is_positional(arg: &Arg) -> bool {
    arg.get_long().is_none() && arg.get_short().is_none()
}

/// Whether a value-taking arg accepts multiple tokens after one flag
/// (`ArgAction::Append`, or a `num_args` upper bound above one). Kept in sync
/// with [`crate::schema`]'s `is_list_arg`.
fn is_multi(arg: &Arg) -> bool {
    if matches!(arg.get_action(), ArgAction::Append) {
        return true;
    }
    arg.get_num_args().is_some_and(|r| r.max_values() > 1)
}

/// Flatten a JSON value into the argv tokens it contributes: arrays fan out per
/// element, scalars render to one token. A top-level `null` is filtered by the
/// caller; one nested inside an array is dropped here.
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
        // Not a valid CLI scalar; stringify so the downstream parse error names
        // the offending value rather than panicking.
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_core::{Registry, command_parser, register_all};
    use serde_json::json;

    /// Build a command's full parser (base template flags + its own `configure`)
    /// through the same `mtui-core` builder `dispatch_argv` re-parses, so these
    /// tests exercise exactly the parser real dispatch sees.
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

    /// The reconstructed argv must re-parse through the same parser.
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
        // `{rrid: <RRID>}` must land on the `regenerate` positional, NOT the base
        // `-T/--template` loaded-template selector — otherwise the engine rejects
        // the unloaded RRID with `Template not loaded`.
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
    fn append_single_value_flag_repeats_per_element() {
        // `approve -g/--group` (Append, default num_args = 1): clap rejects the
        // flag-once form `--group a b`, so the flag must repeat per element.
        // Same shape as `updates --review-group` / `-F`.
        let out = argv("approve", json!({ "group": ["qam-sle", "qam-teradata"] }));
        assert_eq!(out, vec!["--group", "qam-sle", "--group", "qam-teradata"]);
        assert_reparses("approve", &out);
    }

    #[test]
    fn run_hosts_and_command_round_trip_without_leaking_across_boundary() {
        // The silent-corruption case: `run`'s `-t/--target` (Append) and its
        // trailing `COMMAND` positional share the tail, so a flag-once
        // `--target h1 h2 uname -a` parses as hosts=["h1"], command=["h2",
        // "uname","-a"] — running `h2 uname -a` on h1. Repeating `--target` per
        // host keeps the boundary intact; no other test covers this interaction.
        let out = argv(
            "run",
            json!({ "hosts": ["h1", "h2"], "command": ["uname", "-a"] }),
        );
        assert_eq!(out, vec!["--target", "h1", "--target", "h2", "uname", "-a"]);
        assert_reparses("run", &out);
        let parsed = parser_for("run").try_get_matches_from(&out).unwrap();
        let hosts: Vec<&String> = parsed.get_many::<String>("hosts").unwrap().collect();
        assert_eq!(hosts, ["h1", "h2"]);
        let command: Vec<&String> = parsed.get_many::<String>("command").unwrap().collect();
        assert_eq!(command, ["uname", "-a"]);
    }

    #[test]
    fn updates_multi_group_and_fields_round_trip() {
        // The #415 surface: two review groups and two field names (one with an
        // embedded space) round-trip to the value lists the client sent.
        let out = argv(
            "updates",
            json!({
                "review_group": ["qam-sle", "qam-teradata"],
                "field": ["Rating", "Assigned Roles"],
            }),
        );
        assert_reparses("updates", &out);
        let parsed = parser_for("updates").try_get_matches_from(&out).unwrap();
        let groups: Vec<&String> = parsed.get_many::<String>("review_group").unwrap().collect();
        assert_eq!(groups, ["qam-sle", "qam-teradata"]);
        let fields: Vec<&String> = parsed.get_many::<String>("field").unwrap().collect();
        assert_eq!(fields, ["Rating", "Assigned Roles"]);
    }

    #[test]
    fn append_multi_flag_repeats_per_element_and_reparses_identically() {
        // `commit -m/--msg` (Append, num_args 1..): tail-routed so a trailing
        // base flag stays safe, and for `num_args(1..)` the repeated form parses
        // to the same value list as the flag-once one — pinned by re-parsing.
        let out = argv(
            "commit",
            json!({ "msg": ["hello", "world"], "template": "a:b:1:1" }),
        );
        // `-T` (flag) precedes the tail-routed `--msg`.
        assert_eq!(
            out,
            vec!["--template", "a:b:1:1", "--msg", "hello", "--msg", "world"]
        );
        assert_reparses("commit", &out);
        let parsed = parser_for("commit").try_get_matches_from(&out).unwrap();
        let msgs: Vec<&String> = parsed.get_many::<String>("msg").unwrap().collect();
        assert_eq!(msgs, ["hello", "world"]);
    }

    #[test]
    fn append_list_flag_with_choices_repeats_per_element() {
        // `openqa_overview --aggregated-groups` (Append + PossibleValues,
        // default num_args = 1): every element gets its own flag, else a
        // two-element call cannot re-parse.
        let parser = parser_for("openqa_overview");
        // Discover valid enum members from the parser so the re-parse succeeds.
        let arg = parser
            .get_arguments()
            .find(|a| a.get_id() == "aggregated_groups")
            .expect("aggregated_groups arg");
        let members: Vec<String> = arg
            .get_possible_values()
            .iter()
            .take(2)
            .map(|pv| pv.get_name().to_owned())
            .collect();
        assert!(!members.is_empty(), "aggregated_groups has choices");
        let out = argv(
            "openqa_overview",
            json!({ "aggregated_groups": members.clone() }),
        );
        // One `--aggregated-groups` immediately before each member, in order.
        let expected: Vec<String> = members
            .iter()
            .flat_map(|m| ["--aggregated-groups".to_owned(), m.clone()])
            .collect();
        assert_eq!(out, expected);
        assert_reparses("openqa_overview", &out);
    }

    #[test]
    fn set_multi_value_option_emits_flag_once() {
        // No mtui registry arg is a non-Append multi-value option, so pin the
        // branch on a bespoke parser (as `store_false_off_side_emits_flag`
        // does): `num_args(2..)` consumes many values from ONE occurrence, so
        // the flag must be emitted once followed by all items.
        let parser = clap::Command::new("probe").no_binary_name(true).arg(
            clap::Arg::new("point")
                .long("point")
                .action(ArgAction::Set)
                .num_args(2..),
        );
        let out = kwargs_to_argv(
            &parser,
            json!({ "point": ["1", "2", "3"] }).as_object().unwrap(),
            &[],
        );
        assert_eq!(out, vec!["--point", "1", "2", "3"]);
        let parsed = parser.try_get_matches_from(&out).unwrap();
        let values: Vec<&String> = parsed.get_many::<String>("point").unwrap().collect();
        assert_eq!(values, ["1", "2", "3"]);
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
        // The MCP tool synthesiser fans `config` out per-subcommand and passes
        // the *subparser* here (its args live on `set`, not the parent).
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
