//! Translate a command's `clap` arg spec into a JSON-Schema `object`.
//!
//! Port of upstream `mtui/mcp/_schema.py`. Where upstream introspects
//! **argparse actions** to build a `pydantic`/`inspect.Signature` the FastMCP
//! SDK then derives a schema from, this module introspects a **built
//! [`clap::Command`]** directly and emits the JSON Schema `rmcp` wants — a
//! `serde_json::Map<String, Value>` shaped like
//! `{"type":"object","properties":{…},"required":[…]}`.
//!
//! It is intentionally pure: the single entry point [`command_input_schema`]
//! takes a `&clap::Command` and returns plain data. Tool registration, argv
//! reconstruction, and schema slimming live in the sibling P7.6/P7.5/P7.9
//! modules.
//!
//! # Deliberate deviations from upstream
//!
//! * **No shared-`dest` collapse.** Upstream's `_scan_shared_dest_groups`
//!   (~200 lines) exists because several argparse actions in a mutually
//!   exclusive group can share one `dest` (`load_template -a/-k`, `set_repo
//!   -A/-R`). In `clap` every group member carries a *distinct* arg id
//!   (`auto`/`kernel`, `add`/`remove`), so each maps to its own schema property
//!   naturally and the "exactly one required" constraint is enforced by
//!   `clap::ArgGroup` at parse time — nothing for the schema to reconstruct.
//! * **Ranged integers stay `{"type":"integer"}` without bounds.** Upstream
//!   renders argparse `choices=range(1,31)` (`--days`) as a 30-element `enum`.
//!   The Rust `--days` uses a `value_parser!(u32).range(1..=30)` parser; `clap`
//!   erases the parser behind [`clap::builder::ValueParser`] and does not expose
//!   the numeric bounds, so we emit a plain `integer` (the parser still enforces
//!   the range at call time). The Rust arg spec — not the Python one — is the
//!   source of truth here.

use std::any::TypeId;

use clap::builder::ValueRange;
use clap::{Arg, ArgAction};
use serde_json::{Map, Value, json};

/// Build the JSON-Schema `object` describing `cmd`'s callable inputs.
///
/// Walks `cmd.get_arguments()`, skipping the auto `--help`/`--version` args, and
/// emits one property per remaining arg plus a top-level `required` list. The
/// result is the `inputSchema` for the tool `rmcp` synthesises from this command
/// (P7.6). `required` is omitted entirely when empty.
#[must_use]
pub(crate) fn command_input_schema(cmd: &clap::Command) -> Map<String, Value> {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();

    for arg in cmd.get_arguments() {
        // clap injects `help`/`version` args (unless disabled). They carry no
        // callable value — the tool envelope advertises descriptions instead —
        // so drop them exactly as upstream drops `_HelpAction`/`_VersionAction`.
        let id = arg.get_id().as_str();
        if id == "help" || id == "version" {
            continue;
        }

        let (schema, is_required) = arg_to_property(arg);
        properties.insert(id.to_owned(), schema);
        if is_required {
            required.push(Value::String(id.to_owned()));
        }
    }

    let mut out = Map::new();
    out.insert("type".to_owned(), Value::String("object".to_owned()));
    out.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        out.insert("required".to_owned(), Value::Array(required));
    }
    // Reject unknown/misspelled fields: a strict schema lets schema-aware clients
    // fail fast on a typo instead of silently dropping it (the runtime dispatch
    // enforces the same, for clients that do not validate client-side).
    out.insert("additionalProperties".to_owned(), Value::Bool(false));
    out
}

/// Translate one [`Arg`] into `(property schema, is_required)`.
fn arg_to_property(arg: &Arg) -> (Value, bool) {
    // ------------------------------------------------------------- boolean
    // store_true → default false; store_false → default true. A flag never
    // takes a value and is never "required".
    match arg.get_action() {
        ArgAction::SetTrue => return (bool_schema(arg, false), false),
        ArgAction::SetFalse => return (bool_schema(arg, true), false),
        _ => {}
    }

    let is_list = is_list_arg(arg);
    let base = base_type(arg);

    // A property object; `enum`/`minItems`/`default`/`description` layer on.
    let mut inner = Map::new();
    match &base {
        BaseType::Enum(values) => {
            // Choices → JSON `enum`. clap normalises the value type to String,
            // so the enum members are strings.
            inner.insert("type".to_owned(), Value::String("string".to_owned()));
            inner.insert(
                "enum".to_owned(),
                Value::Array(values.iter().cloned().map(Value::String).collect()),
            );
        }
        BaseType::Scalar(kind) => {
            inner.insert(
                "type".to_owned(),
                Value::String(kind.json_type().to_owned()),
            );
        }
    }

    if let Some(desc) = description(arg) {
        inner.insert("description".to_owned(), Value::String(desc));
    }

    // ------------------------------------------------------------- list
    if is_list {
        let mut array = Map::new();
        array.insert("type".to_owned(), Value::String("array".to_owned()));
        array.insert("items".to_owned(), Value::Object(inner));
        apply_list_bounds(&mut array, arg.get_num_args());
        // A list arg is required only when clap marks it required *and* it has no
        // default; otherwise it is optional (upstream defaults optional lists to
        // `[]`/their real default, keeping the schema a plain array).
        let has_default = !arg.get_default_values().is_empty();
        let required = arg.is_required_set() && !has_default;
        if let Some(def) = default_array(arg) {
            array.insert("default".to_owned(), def);
        }
        return (Value::Object(array), required);
    }

    // ------------------------------------------------------------- scalar
    let has_default = !arg.get_default_values().is_empty();
    // clap is authoritative on requiredness for both positionals and options:
    // a positional is optional unless `.required(true)` (unlike argparse, where
    // a bare positional is required). A default always makes it optional. When a
    // required positional also carries a `num_args` lower bound of zero (`?`/`*`)
    // it is effectively optional, so honour that too.
    let required = if has_default {
        false
    } else if arg.is_required_set() {
        arg.get_num_args()
            .is_none_or(|r| positional_is_required(Some(r)))
    } else {
        false
    };

    if let Some(def) = default_scalar(arg, &base) {
        inner.insert("default".to_owned(), def);
        return (Value::Object(inner), false);
    }

    if required {
        return (Value::Object(inner), true);
    }

    // Optional scalar with no default: widen to `X | null` so the schema
    // reflects nullability rather than failing client-side validation (upstream
    // `Annotated[T | None, …]`). enum members stay inside the string alternative.
    let nullable = wrap_nullable(inner);
    (nullable, false)
}

/// The scalar (non-list) shape an arg's tokens decode to.
enum BaseType {
    /// A closed set of string choices → JSON `enum`.
    Enum(Vec<String>),
    /// An open scalar of a JSON primitive type.
    Scalar(ScalarKind),
}

/// The JSON primitive an arg's value parser yields.
#[derive(Clone, Copy)]
enum ScalarKind {
    Integer,
    String,
}

impl ScalarKind {
    fn json_type(self) -> &'static str {
        match self {
            ScalarKind::Integer => "integer",
            ScalarKind::String => "string",
        }
    }
}

/// Decide an arg's base type from its possible values and value parser.
///
/// Possible values (`PossibleValuesParser`) win → `enum`. Otherwise the value
/// parser's output [`TypeId`] selects integer vs string; an unrecognised parser
/// degrades to string with a WARNING, mirroring upstream's "unknown argparse
/// type … falling back to str".
fn base_type(arg: &Arg) -> BaseType {
    let choices = arg.get_possible_values();
    if !choices.is_empty() {
        return BaseType::Enum(choices.iter().map(|pv| pv.get_name().to_owned()).collect());
    }

    // `AnyValueId: PartialEq<TypeId>`, so compare the parser's output type
    // against the standard integer types clap can produce.
    if is_integer_type(arg) {
        return BaseType::Scalar(ScalarKind::Integer);
    }
    // String / PathBuf / no explicit parser all render as JSON string. Only an
    // *unexpected* type warns; the common string/path case is silent.
    if !is_known_string_type(arg) {
        tracing::warn!(
            arg = arg.get_id().as_str(),
            "unknown clap value parser output type; falling back to string"
        );
    }
    BaseType::Scalar(ScalarKind::String)
}

/// True when `arg`'s parser output is one of the standard integer types clap emits.
fn is_integer_type(arg: &Arg) -> bool {
    let id = arg.get_value_parser().type_id();
    id == TypeId::of::<i8>()
        || id == TypeId::of::<i16>()
        || id == TypeId::of::<i32>()
        || id == TypeId::of::<i64>()
        || id == TypeId::of::<isize>()
        || id == TypeId::of::<u8>()
        || id == TypeId::of::<u16>()
        || id == TypeId::of::<u32>()
        || id == TypeId::of::<u64>()
        || id == TypeId::of::<usize>()
}

/// True when `arg`'s parser output is a recognised string-like type (`String`,
/// `PathBuf`, or `OsString`). Used only to decide whether to warn about an
/// unexpected type.
fn is_known_string_type(arg: &Arg) -> bool {
    let id = arg.get_value_parser().type_id();
    id == TypeId::of::<String>()
        || id == TypeId::of::<std::path::PathBuf>()
        || id == TypeId::of::<std::ffi::OsString>()
}

/// A store_true/store_false flag schema: `{"type":"boolean","default":…}`.
fn bool_schema(arg: &Arg, default: bool) -> Value {
    let mut m = Map::new();
    m.insert("type".to_owned(), Value::String("boolean".to_owned()));
    if let Some(desc) = description(arg) {
        m.insert("description".to_owned(), Value::String(desc));
    }
    m.insert("default".to_owned(), Value::Bool(default));
    Value::Object(m)
}

/// Whether an arg accepts multiple values (→ JSON array).
///
/// `ArgAction::Append` is always a list. A value-taking arg whose `num_args`
/// upper bound exceeds one is also a list. `num_args(0..=1)` (`?`) and the
/// default single-value shape stay scalar.
fn is_list_arg(arg: &Arg) -> bool {
    if matches!(arg.get_action(), ArgAction::Append) {
        return true;
    }
    match arg.get_num_args() {
        Some(range) => range.max_values() > 1,
        None => false,
    }
}

/// Apply `minItems`/`maxItems` to an array schema from an arg's `num_args`.
///
/// `1..` (`+`) → `minItems:1`. A fixed `N` (N>1) → `minItems:N,maxItems:N`.
/// `Append` with no explicit num_args and open `*`-style ranges add no bounds.
fn apply_list_bounds(array: &mut Map<String, Value>, num_args: Option<ValueRange>) {
    let Some(range) = num_args else { return };
    let min = range.min_values();
    let max = range.max_values();
    if min == max && min > 1 {
        array.insert("minItems".to_owned(), json!(min));
        array.insert("maxItems".to_owned(), json!(min));
    } else if min >= 1 {
        array.insert("minItems".to_owned(), json!(min));
    }
}

/// A positional is required unless its `num_args` allows zero values
/// (`?` → `0..=1`, `*` → `0..`).
fn positional_is_required(num_args: Option<ValueRange>) -> bool {
    match num_args {
        Some(range) => range.min_values() >= 1,
        None => true,
    }
}

/// Read an arg's help text, trimmed, or `None`.
fn description(arg: &Arg) -> Option<String> {
    arg.get_help()
        .map(|s| s.to_string().trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Render a scalar arg's default value into a JSON value matching `base`, or
/// `None` when the arg has no default.
fn default_scalar(arg: &Arg, base: &BaseType) -> Option<Value> {
    let raw = arg.get_default_values();
    let first = raw.first()?;
    let s = first.to_string_lossy().into_owned();
    match base {
        BaseType::Scalar(ScalarKind::Integer) => s
            .parse::<i64>()
            .ok()
            .map(|n| json!(n))
            .or(Some(Value::String(s))),
        _ => Some(Value::String(s)),
    }
}

/// Render a list arg's default as a JSON array, or `None` when it has none.
fn default_array(arg: &Arg) -> Option<Value> {
    let raw = arg.get_default_values();
    if raw.is_empty() {
        return None;
    }
    Some(Value::Array(
        raw.iter()
            .map(|v| Value::String(v.to_string_lossy().into_owned()))
            .collect(),
    ))
}

/// Wrap a scalar property object as a nullable `{"anyOf":[<inner>,{"type":"null"}]}`.
fn wrap_nullable(inner: Map<String, Value>) -> Value {
    // Description belongs on the outer schema so clients see it regardless of
    // which alternative validates; pull it out of the inner object.
    let mut inner = inner;
    let description = inner.remove("description");
    let mut out = Map::new();
    out.insert(
        "anyOf".to_owned(),
        Value::Array(vec![Value::Object(inner), json!({"type": "null"})]),
    );
    if let Some(desc) = description {
        out.insert("description".to_owned(), desc);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtui_core::{Registry, register_all};

    /// Build the per-command clap parser the way each command's own unit tests
    /// do: a bare `no_binary_name` base with the command's `configure` applied.
    /// The base template flags (`-T`/`--all-templates`) are the caller's concern
    /// (P7.6) and out of scope for the per-arg schema this task produces.
    fn schema_for(command: &str) -> Map<String, Value> {
        let registry: Registry = register_all();
        let cmd = registry
            .get(command)
            .unwrap_or_else(|| panic!("command not registered: {command}"));
        let parser = cmd.configure(clap::Command::new(cmd.name()).no_binary_name(true));
        command_input_schema(&parser)
    }

    fn props(schema: &Map<String, Value>) -> &Map<String, Value> {
        schema["properties"].as_object().unwrap()
    }

    fn required(schema: &Map<String, Value>) -> Vec<&str> {
        schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    // --------------------------------------------------------------- scalars

    #[test]
    fn int_typed_option_maps_to_integer_schema_and_keeps_default() {
        // `openqa_overview --days` (value_parser!(u32).range(1..=30), default 5)
        // stays an integer with its real default — never a string.
        let schema = schema_for("openqa_overview");
        let days = &props(&schema)["days"];
        assert_eq!(days["type"], "integer");
        assert_eq!(days["default"], json!(5));
        // Not required (has a default) and not nullable.
        assert!(!required(&schema).contains(&"days"));
        assert!(days.get("anyOf").is_none());
    }

    #[test]
    fn ranged_integer_emits_no_bounds_documented_deviation() {
        // Intentional divergence from upstream's `enum: 1..30`: clap erases the
        // range parser, so we emit a plain integer with no minimum/maximum/enum.
        let days = props(&schema_for("openqa_overview"))["days"].clone();
        assert!(days.get("minimum").is_none());
        assert!(days.get("maximum").is_none());
        assert!(days.get("enum").is_none());
    }

    #[test]
    fn required_int_positional_is_required_scalar() {
        // `set_timeout timeout` (value_parser!(u64), required) → required integer.
        let schema = schema_for("set_timeout");
        let timeout = &props(&schema)["timeout"];
        assert_eq!(timeout["type"], "integer");
        assert!(required(&schema).contains(&"timeout"));
        assert!(timeout.get("default").is_none());
    }

    // --------------------------------------------------------------- booleans

    #[test]
    fn store_true_defaults_false_with_description() {
        // `openqa_overview --export` (SetTrue) → boolean default false, keeps help.
        let schema = schema_for("openqa_overview");
        let export = &props(&schema)["export"];
        assert_eq!(export["type"], "boolean");
        assert_eq!(export["default"], Value::Bool(false));
        assert_eq!(
            export["description"],
            "Also inject the overview into the loaded testreport's log"
        );
        assert!(!required(&schema).contains(&"export"));
    }

    #[test]
    fn store_true_generic_branch_defaults_false() {
        // Pin the generic SetTrue/SetFalse mapping directly against a bespoke arg
        // (no mtui command uses SetFalse today).
        let parser = clap::Command::new("probe").no_binary_name(true).arg(
            clap::Arg::new("color")
                .long("no-color")
                .action(ArgAction::SetFalse)
                .help("disable color output"),
        );
        let schema = command_input_schema(&parser);
        let color = &props(&schema)["color"];
        assert_eq!(color["type"], "boolean");
        assert_eq!(color["default"], Value::Bool(true));
    }

    // ----------------------------------------------------------------- enums

    #[test]
    fn possible_values_become_enum() {
        // `set_log_level level` (PossibleValuesParser, required) → string enum.
        let schema = schema_for("set_log_level");
        let level = &props(&schema)["level"];
        assert_eq!(level["type"], "string");
        let members: Vec<&str> = level["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(members.contains(&"info") && members.contains(&"debug"));
        assert!(required(&schema).contains(&"level"));
    }

    // ------------------------------------------------------------------ lists

    #[test]
    fn append_list_with_choices_carries_min_items() {
        // `openqa_overview --aggregated-groups` (Append + PossibleValues) → array
        // of string-enum items.
        let schema = schema_for("openqa_overview");
        let groups = &props(&schema)["aggregated_groups"];
        assert_eq!(groups["type"], "array");
        let items = groups["items"].as_object().unwrap();
        assert_eq!(items["type"], "string");
        assert!(items.get("enum").is_some());
    }

    #[test]
    fn fixed_nargs_list_carries_exact_bounds() {
        let parser = clap::Command::new("probe").no_binary_name(true).arg(
            clap::Arg::new("pair")
                .long("pair")
                .num_args(2)
                .help("two values"),
        );
        let schema = command_input_schema(&parser);
        let pair = &props(&schema)["pair"];
        assert_eq!(pair["type"], "array");
        assert_eq!(pair["minItems"], json!(2));
        assert_eq!(pair["maxItems"], json!(2));
    }

    #[test]
    fn plus_nargs_list_carries_min_items_one() {
        let parser = clap::Command::new("probe").no_binary_name(true).arg(
            clap::Arg::new("many")
                .long("many")
                .num_args(1..)
                .help("one or more"),
        );
        let schema = command_input_schema(&parser);
        let many = &props(&schema)["many"];
        assert_eq!(many["type"], "array");
        assert_eq!(many["minItems"], json!(1));
        assert!(many.get("maxItems").is_none());
    }

    // ------------------------------------------------------------- optionality

    #[test]
    fn optional_positional_is_nullable_and_not_required() {
        // `export filename` (num_args 0..=1) → optional, nullable string.
        let schema = schema_for("export");
        let filename = &props(&schema)["filename"];
        assert!(!required(&schema).contains(&"filename"));
        let alts = filename["anyOf"].as_array().unwrap();
        assert!(alts.contains(&json!({"type": "string"})));
        assert!(alts.contains(&json!({"type": "null"})));
    }

    #[test]
    fn optional_scalar_option_without_default_is_nullable() {
        let parser = clap::Command::new("probe")
            .no_binary_name(true)
            .arg(clap::Arg::new("note").long("note").help("free-form note"));
        let schema = command_input_schema(&parser);
        let note = &props(&schema)["note"];
        // Description hoisted to the outer nullable wrapper.
        assert_eq!(note["description"], "free-form note");
        let alts = note["anyOf"].as_array().unwrap();
        assert!(alts.contains(&json!({"type": "null"})));
    }

    #[test]
    fn scalar_option_with_default_is_optional_string() {
        // `updates --status` (default "testing") → optional string, no anyOf.
        let schema = schema_for("updates");
        let status = &props(&schema)["status"];
        assert_eq!(status["type"], "string");
        assert_eq!(status["default"], "testing");
        assert!(status.get("anyOf").is_none());
        assert!(!required(&schema).contains(&"status"));
    }

    // ---------------------------------------------------------- mutex groups

    #[test]
    fn clap_group_members_become_independent_nullable_props() {
        // `load_template` -a/-k share a required ArgGroup but carry distinct ids,
        // so each is its own optional nullable property (the group's "exactly
        // one" is enforced by clap at parse time, not by the schema).
        let schema = schema_for("load_template");
        let p = props(&schema);
        assert!(p.contains_key("auto"));
        assert!(p.contains_key("kernel"));
        assert!(!required(&schema).contains(&"auto"));
        assert!(!required(&schema).contains(&"kernel"));
        assert!(p["auto"].get("anyOf").is_some());
    }

    // -------------------------------------------------------------- structure

    #[test]
    fn output_is_an_object_with_properties() {
        let schema = schema_for("updates");
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
    }

    #[test]
    fn output_is_strict_rejecting_unknown_properties() {
        // A strict schema advertises additionalProperties:false so a schema-aware
        // client rejects a misspelled field rather than silently dropping it.
        let schema = schema_for("updates");
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        // Also holds for an empty command.
        let empty = command_input_schema(
            &clap::Command::new("probe")
                .no_binary_name(true)
                .disable_help_flag(true),
        );
        assert_eq!(empty["additionalProperties"], Value::Bool(false));
    }

    #[test]
    fn help_and_version_args_are_skipped() {
        // clap injects a `help` arg; it must never surface as a tool property.
        let parser = clap::Command::new("probe").arg(clap::Arg::new("real").long("real"));
        let schema = command_input_schema(&parser);
        assert!(!props(&schema).contains_key("help"));
        assert!(props(&schema).contains_key("real"));
    }

    #[test]
    fn empty_command_has_no_required_key() {
        let parser = clap::Command::new("probe")
            .no_binary_name(true)
            .disable_help_flag(true);
        let schema = command_input_schema(&parser);
        assert!(schema.get("required").is_none());
        assert!(props(&schema).is_empty());
    }

    #[test]
    fn unknown_value_parser_falls_back_to_string() {
        // A bespoke parser whose output is neither integer nor a known string
        // type degrades to string (warns; assertion covers the fallback path).
        #[derive(Clone)]
        struct Weird(#[allow(dead_code)] String);
        let parser = clap::Command::new("probe").no_binary_name(true).arg(
            clap::Arg::new("w")
                .long("weird")
                .value_parser(clap::builder::ValueParser::new(
                    |s: &str| -> Result<Weird, std::convert::Infallible> {
                        Ok(Weird(s.to_owned()))
                    },
                )),
        );
        let schema = command_input_schema(&parser);
        // Optional scalar with no default → nullable string alternative.
        let w = &props(&schema)["w"];
        let alts = w["anyOf"].as_array().unwrap();
        assert!(alts.contains(&json!({"type": "string"})));
    }
}
