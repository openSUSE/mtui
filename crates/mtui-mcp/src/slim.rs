//! Token-slimming helpers for the MCP wire surface.
//!
//! Two token-budget concerns live here, co-located exactly as upstream
//! `mtui/mcp/_slim.py` groups them:
//!
//! * [`cap_output`] — the per-tool-result byte bound.
//! * [`slim_tool_schema`] — the JSON-Schema slimming pass (P7.9) that drops
//!   redundant `title` keys, flattens `anyOf: [{type: X}, {type: null}]` unions,
//!   and terse-rewrites the long shared `help` strings before the tool list goes
//!   on the wire.
//!
//! Unlike upstream — which mutates a live pydantic-generated schema in the SDK's
//! tool table — the Rust schema is built directly from `clap` by
//! [`crate::schema`], so it never emits pydantic `title` keys and renders a
//! nullable scalar as the same `anyOf: [{type: X}, {type: null}]` shape via
//! [`crate::schema::command_input_schema`]'s `wrap_nullable`. The `title`-drop is
//! therefore mostly defensive here; the substantive wins are the nullable
//! flatten and the terse descriptions. The transforms run on the plain
//! [`ToolDescriptor`](crate::tools::ToolDescriptor) schema `Value`s in
//! [`crate::server::McpServer::new`] before conversion to `rmcp::model::Tool`.

use serde_json::{Map, Value};

/// Truncate `text` to at most `limit` bytes (UTF-8), appending a notice.
///
/// A single tool result — a `run` over many hosts, a multi-thousand-line install
/// log — can dwarf the rest of the client's context. When the UTF-8 length of
/// `text` exceeds `limit` the **tail** is dropped (the head usually carries the
/// command echo and the first, most diagnostic output) and a one-line
/// `…[truncated N bytes; …]` notice is appended pointing at the paged readers.
///
/// `limit == 0` disables the cap and returns `text` unchanged. Under-cap text is
/// returned byte-identical. The cut is made on a `char` boundary so the result
/// is always valid UTF-8 even when the byte cut would split a codepoint.
///
/// Mirrors upstream `mtui.mcp._slim.cap_output`: the reported dropped-byte count
/// is `total − limit` (the budget overrun), independent of the small extra bytes
/// a codepoint-boundary trim may shed.
#[must_use]
pub fn cap_output(text: String, limit: usize) -> String {
    if limit == 0 {
        return text;
    }
    let total = text.len();
    if total <= limit {
        return text;
    }
    // Largest char boundary at or below `limit` (upstream decodes `[:limit]`
    // with errors="ignore", which likewise drops a split trailing codepoint).
    let cut = (0..=limit)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let dropped = total - limit;
    let mut head = text;
    head.truncate(cut);
    head.push_str(&truncation_notice(dropped, limit));
    head
}

/// The one-line truncation notice appended when output is capped.
///
/// Shared by [`cap_output`] (post-hoc string truncation) and the write-time
/// [`SharedBuf`](crate::capture::SharedBuf) path in
/// [`McpSession::run_command`](crate::session::McpSession::run_command) so both
/// emit byte-identical text. `dropped` is the budget overrun (bytes discarded),
/// `limit` the `[mcp] max_output_bytes` budget.
#[must_use]
pub fn truncation_notice(dropped: usize, limit: usize) -> String {
    format!(
        "\n…[truncated {dropped} bytes; output exceeded the \
         [mcp] max_output_bytes={limit} budget — use a narrower command, or \
         the offset/limit paging on testreport reads]"
    )
}

/// Long `clap`/argparse `help` strings shared across many synthesised tools,
/// mapped to a terse equivalent. Rewriting only the MCP wire copy keeps the REPL
/// `--help` output (sourced from the same `clap` args) verbose and unchanged.
/// Keys are matched exactly against a field's `description`. Mirrors upstream
/// `_slim._TERSE_DESCRIPTIONS`.
const TERSE_DESCRIPTIONS: &[(&str, &str)] = &[
    (
        "RRID of a single loaded template to act on (default: all loaded templates)",
        "RRID of one loaded template (default: all)",
    ),
    (
        "Act on every loaded template (the default for this command)",
        "Act on all loaded templates (default)",
    ),
    (
        "Host to act on. Can be used multiple times. If is ommited all hosts are used",
        "Host to act on (repeatable; default: all hosts)",
    ),
];

/// Schema keywords whose value is a mapping of *names* to sub-schemas. Under
/// these, the map keys are parameter/definition names, not schema keywords, so
/// the slimming transforms are suspended for that one level (a parameter literally
/// named `title`/`description` must survive). Mirrors upstream `_slim._NAME_MAPS`.
const NAME_MAPS: &[&str] = &["properties", "patternProperties", "$defs", "definitions"];

/// Return `schema` recursively slimmed of redundant JSON-Schema weight.
///
/// Three transforms, applied depth-first so nested `properties` and `items` are
/// covered:
///
/// * drop every `title` schema keyword;
/// * collapse `anyOf: [{type: X}, {type: null}]` to a flat `{type: X}` (hoisting
///   the surviving arm's keys, e.g. `items` for arrays) via [`flatten_nullable`];
/// * replace a known-verbose `description` with its terse form from
///   [`TERSE_DESCRIPTIONS`].
///
/// The input is not mutated; a new [`Value`] is returned. The keys of a
/// `properties`/`$defs`-style map are *names*, not schema keywords, so the
/// transforms are suspended for that one level. Mirrors upstream
/// `_slim.slim_tool_schema`.
#[must_use]
pub fn slim_tool_schema(schema: &Value) -> Value {
    slim(schema, false)
}

/// Slim a `Map`-shaped tool input schema in place, returning the new map.
///
/// Convenience wrapper for [`crate::tools::ToolDescriptor::input_schema`], which
/// is a [`Map`] rather than a [`Value`]. Equivalent to
/// `slim_tool_schema(&Value::Object(map))` unwrapped back to the object.
#[must_use]
pub fn slim_input_schema(schema: &Map<String, Value>) -> Map<String, Value> {
    match slim_tool_schema(&Value::Object(schema.clone())) {
        Value::Object(map) => map,
        // slim() maps an object to an object; unreachable in practice.
        other => {
            let mut m = Map::new();
            m.insert("__slimmed".to_owned(), other);
            m
        }
    }
}

/// Collapse a two-member `anyOf: [{type: X}, {type: null}]` union in `node`.
///
/// The `null` arm is redundant for the model (a `default` already signals the
/// field is optional), so the non-null arm's `type` (and any sibling keys it
/// carries, e.g. `items`) is hoisted to node level and the `anyOf` is dropped.
/// Only the exact two-member `[T, null]` shape is touched; genuine multi-type
/// unions are left alone. Mirrors upstream `_slim._flatten_nullable`.
fn flatten_nullable(node: &mut Map<String, Value>) {
    let Some(Value::Array(any_of)) = node.get("anyOf") else {
        return;
    };
    if any_of.len() != 2 {
        return;
    }
    let is_null =
        |m: &Value| m.as_object().and_then(|o| o.get("type")) == Some(&Value::from("null"));
    let non_null: Vec<&Value> = any_of
        .iter()
        .filter(|m| m.is_object() && !is_null(m))
        .collect();
    let nulls: Vec<&Value> = any_of.iter().filter(|m| is_null(m)).collect();
    if non_null.len() != 1 || nulls.len() != 1 {
        return;
    }
    let Some(arm) = non_null[0].as_object() else {
        return;
    };
    if !arm.contains_key("type") {
        return;
    }
    let arm = arm.clone();
    node.remove("anyOf");
    // Hoist the surviving arm's keys without clobbering node-level metadata
    // (description/default/title live on the node, not the arm).
    for (key, value) in arm {
        node.entry(key).or_insert(value);
    }
}

/// Recursive slimming worker. `keys_are_names` suspends the keyword transforms
/// for one level (the values of a [`NAME_MAPS`] key). Mirrors upstream
/// `_slim._slim`.
fn slim(schema: &Value, keys_are_names: bool) -> Value {
    match schema {
        Value::Object(obj) => {
            let mut out = Map::new();
            for (key, value) in obj {
                if !keys_are_names {
                    if key == "title" {
                        continue;
                    }
                    if key == "description"
                        && let Value::String(desc) = value
                    {
                        let terse = TERSE_DESCRIPTIONS
                            .iter()
                            .find(|(from, _)| from == desc)
                            .map_or(desc.as_str(), |(_, to)| to);
                        out.insert(key.clone(), Value::String(terse.to_owned()));
                        continue;
                    }
                }
                let child_names = !keys_are_names && NAME_MAPS.contains(&key.as_str());
                out.insert(key.clone(), slim(value, child_names));
            }
            if !keys_are_names {
                flatten_nullable(&mut out);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| slim(item, false)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- slim_tool_schema ----------------------------------------------------

    #[test]
    fn slim_drops_all_title_keys() {
        let schema = json!({
            "title": "tool_runArguments",
            "type": "object",
            "properties": {
                "command": { "title": "Command", "type": "array", "items": { "type": "str" } },
            },
        });
        let out = slim_tool_schema(&schema);
        assert!(out.get("title").is_none());
        assert!(out["properties"]["command"].get("title").is_none());
        // non-title content preserved
        assert_eq!(out["properties"]["command"]["type"], json!("array"));
    }

    #[test]
    fn slim_flattens_nullable_union_and_keeps_default() {
        let node = json!({
            "anyOf": [{ "type": "string" }, { "type": "null" }],
            "default": null,
            "description": "some field",
        });
        let out = slim_tool_schema(&node);
        assert!(out.get("anyOf").is_none());
        assert_eq!(out["type"], json!("string"));
        assert_eq!(out["default"], Value::Null);
        assert_eq!(out["description"], json!("some field"));
    }

    #[test]
    fn slim_flattens_nullable_array_union_hoists_items() {
        let node = json!({
            "anyOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "null" }],
            "default": null,
        });
        let out = slim_tool_schema(&node);
        assert!(out.get("anyOf").is_none());
        assert_eq!(out["type"], json!("array"));
        assert_eq!(out["items"], json!({ "type": "string" }));
    }

    #[test]
    fn slim_leaves_genuine_multitype_union_untouched() {
        let node = json!({ "anyOf": [{ "type": "string" }, { "type": "integer" }] });
        let out = slim_tool_schema(&node);
        // No null arm → not the [T, null] shape → left as-is.
        assert_eq!(out["anyOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn slim_rewrites_known_verbose_description() {
        let node = json!({
            "type": "string",
            "default": null,
            "description":
                "RRID of a single loaded template to act on (default: all loaded templates)",
        });
        let out = slim_tool_schema(&node);
        assert_eq!(
            out["description"],
            json!("RRID of one loaded template (default: all)")
        );
    }

    #[test]
    fn slim_input_not_mutated() {
        let schema =
            json!({ "title": "X", "properties": { "a": { "title": "A", "type": "string" } } });
        let before = schema.clone();
        let _ = slim_tool_schema(&schema);
        assert_eq!(schema, before);
    }

    #[test]
    fn slim_keeps_property_named_title() {
        // A parameter literally named `title` is a name, not a keyword: the
        // schema keyword `title` is stripped but the property must survive so
        // `required` does not dangle.
        let schema = json!({
            "title": "tool_addBugArguments",
            "type": "object",
            "properties": {
                "title": { "title": "Title", "type": "string" },
                "description": { "title": "Description", "type": "string" },
            },
            "required": ["title", "description"],
        });
        let out = slim_tool_schema(&schema);
        assert!(out.get("title").is_none());
        assert_eq!(out["properties"]["title"], json!({ "type": "string" }));
        assert_eq!(
            out["properties"]["description"],
            json!({ "type": "string" })
        );
        assert_eq!(out["required"], json!(["title", "description"]));
    }

    #[test]
    fn slim_keeps_nested_defs_property_names() {
        let schema = json!({
            "type": "object",
            "properties": {
                "cfg": {
                    "type": "object",
                    "properties": { "title": { "type": "string", "title": "T" } },
                },
            },
            "$defs": { "title": { "type": "integer", "title": "X" } },
        });
        let out = slim_tool_schema(&schema);
        assert_eq!(
            out["properties"]["cfg"]["properties"]["title"],
            json!({ "type": "string" })
        );
        assert_eq!(out["$defs"]["title"], json!({ "type": "integer" }));
    }

    #[test]
    fn slim_input_schema_shrinks_and_strips_titles() {
        // The Map-shaped convenience wrapper used by the descriptor path.
        let mut schema = Map::new();
        schema.insert("title".to_owned(), json!("Arg"));
        schema.insert("type".to_owned(), json!("object"));
        let out = slim_input_schema(&schema);
        assert!(!out.contains_key("title"));
        assert_eq!(out["type"], json!("object"));
    }

    // -- cap_output ----------------------------------------------------------

    #[test]
    fn zero_limit_disables_the_cap() {
        let text = "x".repeat(1000);
        assert_eq!(cap_output(text.clone(), 0), text);
    }

    #[test]
    fn under_cap_is_byte_identical() {
        let text = "hello world".to_owned();
        assert_eq!(cap_output(text.clone(), 100), text);
    }

    #[test]
    fn at_cap_is_unchanged() {
        // len == limit is not "exceeded"; returned as-is.
        let text = "abcde".to_owned();
        assert_eq!(cap_output(text.clone(), 5), text);
    }

    #[test]
    fn over_cap_keeps_head_and_appends_notice() {
        let text = "abcdefghij".to_owned(); // 10 bytes
        let out = cap_output(text, 4);
        assert!(out.starts_with("abcd"), "head preserved: {out:?}");
        // Upstream reports the budget overrun: total(10) - limit(4) = 6.
        assert!(out.contains("truncated 6 bytes"), "notice count: {out:?}");
        assert!(out.contains("max_output_bytes=4"), "notice limit: {out:?}");
        // The dropped tail is gone.
        assert!(!out.contains("efghij"), "tail dropped: {out:?}");
    }

    #[test]
    fn cut_falls_on_a_char_boundary_for_multibyte_text() {
        // "€" is 3 bytes (E2 82 AC). A cut at limit=2 would split it; the head
        // must stop at the previous boundary (byte 0) rather than emit invalid
        // UTF-8. The whole String result must be valid UTF-8 (it is, by type).
        let text = "€€€".to_owned(); // 9 bytes
        let out = cap_output(text, 2);
        // Nothing before the first full codepoint fits, so the head is empty and
        // only the notice remains.
        assert!(out.starts_with('\n'), "head empty then notice: {out:?}");
        assert!(out.contains("truncated 7 bytes"), "9 - 2 = 7: {out:?}");
    }

    #[test]
    fn keeps_whole_codepoints_up_to_the_boundary() {
        // limit=4 with 3-byte codepoints: only the first "€" (bytes 0..3) fits.
        let text = "€€".to_owned(); // 6 bytes
        let out = cap_output(text, 4);
        assert!(out.starts_with('€'), "first codepoint kept: {out:?}");
        // Exactly one "€" then the notice — not one and a half.
        assert!(
            out.chars().filter(|&c| c == '€').count() == 1,
            "only whole codepoints kept: {out:?}"
        );
    }
}
