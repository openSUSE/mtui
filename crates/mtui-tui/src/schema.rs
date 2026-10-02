//! A walker over the report document's JSON Schema.
//!
//! It understands only the keywords the shipped schema uses and refuses the
//! rest, so a server-side schema change that adds a construct the editor cannot
//! interpret fails the load instead of being silently skipped. Declared
//! property order is kept so a form lists fields as the schema does.

use std::collections::BTreeSet;
use std::fmt;

use regex::Regex;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Number, Value};
use thiserror::Error;

const NODE_KEYWORDS: &[&str] = &[
    "$comment",
    "$ref",
    "title",
    "description",
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "items",
    "additionalProperties",
    "propertyNames",
    "pattern",
    "minLength",
    "minItems",
    "minProperties",
    "format",
];
const ROOT_ONLY_KEYWORDS: &[&str] = &["$schema", "$id", "$defs", "allOf"];
const MAX_REF_DEPTH: usize = 32;

/// Why a schema could not be turned into a [`Schema`].
#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("schema is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{pointer}: unsupported keyword {keyword:?}")]
    UnsupportedKeyword { pointer: String, keyword: String },
    #[error("{pointer}: {reason}")]
    Invalid { pointer: String, reason: String },
    #[error("{pointer}: unresolved reference {target:?}")]
    UnresolvedRef { pointer: String, target: String },
    #[error("{pointer}: bad pattern: {source}")]
    BadPattern {
        pointer: String,
        #[source]
        source: regex::Error,
    },
}

fn invalid(pointer: &str, reason: impl Into<String>) -> SchemaError {
    SchemaError::Invalid {
        pointer: pointer.to_owned(),
        reason: reason.into(),
    }
}

/// A compiled `pattern` keyword.
#[derive(Debug, Clone)]
pub struct Pattern {
    source: String,
    regex: Regex,
}

impl Pattern {
    /// The pattern as written in the schema.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn is_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }
}

/// One schema node: its documentation plus the shape it describes.
#[derive(Debug, Clone)]
pub struct Node {
    pub title: Option<String>,
    pub description: Option<String>,
    pub kind: Kind,
}

#[derive(Debug, Clone)]
pub enum Kind {
    /// Named properties, in declared order. `closed` is `additionalProperties: false`.
    Object {
        props: Vec<(String, Node)>,
        required: BTreeSet<String>,
        closed: bool,
    },
    Array {
        items: Box<Node>,
        min_items: usize,
    },
    /// Free keys over one value schema (`additionalProperties: {…}`).
    Map {
        value: Box<Node>,
        key_pattern: Option<Pattern>,
        min_props: usize,
    },
    Str {
        min_len: usize,
        pattern: Option<Pattern>,
        format: Option<String>,
    },
    Int,
    Bool,
    Enum(Vec<String>),
    Const(Value),
    /// The value may also be `null`.
    Nullable(Box<Node>),
}

impl Node {
    /// This node without a `Nullable` wrapper, and whether there was one.
    #[must_use]
    pub fn unwrapped(&self) -> (&Node, bool) {
        match &self.kind {
            Kind::Nullable(inner) => (inner, true),
            _ => (self, false),
        }
    }

    /// The node at an RFC 6901 `pointer` below this one. Intermediate
    /// `Nullable` wrappers are looked through; the node at the pointer itself
    /// is returned as declared.
    #[must_use]
    pub fn lookup(&self, pointer: &str) -> Option<&Node> {
        if pointer.is_empty() {
            return Some(self);
        }
        let rest = pointer.strip_prefix('/')?;
        let (token, tail) = rest.split_once('/').map_or((rest, ""), |(t, r)| (t, r));
        let token = token.replace("~1", "/").replace("~0", "~");
        let tail = if tail.is_empty() {
            String::new()
        } else {
            format!("/{tail}")
        };
        let child = match &self.unwrapped().0.kind {
            Kind::Object { props, .. } => props.iter().find(|(k, _)| *k == token).map(|(_, n)| n),
            Kind::Array { items, .. } => token.parse::<usize>().ok().map(|_| &**items),
            Kind::Map { value, .. } => Some(&**value),
            _ => None,
        }?;
        child.lookup(&tail)
    }
}

/// A `then` branch of a root conditional: extra `required` keys, at the root
/// and one level down.
#[derive(Debug, Clone, Default)]
struct Overlay {
    required: Vec<String>,
    nested: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone)]
struct Condition {
    kinds: Vec<String>,
    then: Overlay,
}

/// The parsed report schema.
#[derive(Debug, Clone)]
pub struct Schema {
    root: Node,
    conditions: Vec<Condition>,
}

impl Schema {
    /// The schema shipped with `mtui-types`.
    ///
    /// # Errors
    ///
    /// Fails when the shipped schema uses a construct the walker does not
    /// understand.
    pub fn load() -> Result<Self, SchemaError> {
        Self::parse(mtui_types::report_document::SCHEMA_JSON)
    }

    /// Parse a schema document.
    ///
    /// # Errors
    ///
    /// Fails on malformed JSON, an unsupported keyword, an unresolved `$ref`
    /// or an invalid `pattern`.
    pub fn parse(raw: &str) -> Result<Self, SchemaError> {
        let json: Json = serde_json::from_str(raw)?;
        let Json::Object(entries) = &json else {
            return Err(invalid("", "schema root must be an object"));
        };
        let root_json = &json;
        let defs = match get(entries, "$defs") {
            Some(Json::Object(defs)) => defs.as_slice(),
            Some(_) => return Err(invalid("/$defs", "must be an object")),
            None => &[],
        };
        let ctx = Ctx { defs };
        let root = ctx.node(root_json, "", true, 0)?;
        let conditions = match get(entries, "allOf") {
            Some(Json::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| condition(item, &format!("/allOf/{i}")))
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(invalid("/allOf", "must be an array")),
            None => Vec::new(),
        };
        Ok(Self { root, conditions })
    }

    /// The root node with the conditionals that select on `kind` applied
    /// (`review` is required for `maintenance`/`slfo`, `update.patches` for `pi`).
    #[must_use]
    pub fn for_kind(&self, kind: &str) -> Node {
        let mut root = self.root.clone();
        for condition in self
            .conditions
            .iter()
            .filter(|c| c.kinds.iter().any(|k| k == kind))
        {
            if let Kind::Object { required, .. } = &mut root.kind {
                required.extend(condition.then.required.iter().cloned());
            }
            for (prop, names) in &condition.then.nested {
                if let Kind::Object { props, .. } = &mut root.kind
                    && let Some((_, node)) = props.iter_mut().find(|(k, _)| k == prop)
                    && let Kind::Object { required, .. } = &mut node.kind
                {
                    required.extend(names.iter().cloned());
                }
            }
        }
        root
    }
}

struct Ctx<'a> {
    defs: &'a [(String, Json)],
}

impl Ctx<'_> {
    fn node(
        &self,
        json: &Json,
        pointer: &str,
        root: bool,
        depth: usize,
    ) -> Result<Node, SchemaError> {
        let Json::Object(entries) = json else {
            return Err(invalid(pointer, "a schema must be an object"));
        };
        for (key, _) in entries {
            let known = NODE_KEYWORDS.contains(&key.as_str())
                || (root && ROOT_ONLY_KEYWORDS.contains(&key.as_str()));
            if !known {
                return Err(SchemaError::UnsupportedKeyword {
                    pointer: pointer.to_owned(),
                    keyword: key.clone(),
                });
            }
        }
        let title = string(entries, "title", pointer)?;
        let description = string(entries, "description", pointer)?;

        if let Some(target) = string(entries, "$ref", pointer)? {
            if let Some((key, _)) = entries
                .iter()
                .find(|(k, _)| !matches!(k.as_str(), "$ref" | "title" | "description" | "$comment"))
            {
                return Err(invalid(
                    pointer,
                    format!("{key:?} beside $ref is not supported"),
                ));
            }
            if depth >= MAX_REF_DEPTH {
                return Err(invalid(pointer, "$ref chain too deep"));
            }
            let unresolved = || SchemaError::UnresolvedRef {
                pointer: pointer.to_owned(),
                target: target.clone(),
            };
            let name = target.strip_prefix("#/$defs/").ok_or_else(unresolved)?;
            let (_, def) = self
                .defs
                .iter()
                .find(|(k, _)| k == name)
                .ok_or_else(unresolved)?;
            let mut node = self.node(def, &format!("/$defs/{name}"), false, depth + 1)?;
            node.title = title.or(node.title);
            node.description = description.or(node.description);
            return Ok(node);
        }

        let (kind, nullable) = self.kind(entries, pointer, depth)?;
        let kind = if nullable {
            Kind::Nullable(Box::new(Node {
                title: None,
                description: None,
                kind,
            }))
        } else {
            kind
        };
        Ok(Node {
            title,
            description,
            kind,
        })
    }

    fn kind(
        &self,
        entries: &[(String, Json)],
        pointer: &str,
        depth: usize,
    ) -> Result<(Kind, bool), SchemaError> {
        if let Some(values) = get(entries, "enum") {
            let Json::Array(values) = values else {
                return Err(invalid(pointer, "enum must be an array"));
            };
            let mut options = Vec::new();
            let mut nullable = false;
            for value in values {
                match value {
                    Json::Scalar(Value::Null) => nullable = true,
                    Json::Scalar(Value::String(s)) => options.push(s.clone()),
                    _ => return Err(invalid(pointer, "enum members must be strings or null")),
                }
            }
            return Ok((Kind::Enum(options), nullable));
        }
        if let Some(Json::Scalar(value)) = get(entries, "const") {
            return Ok((Kind::Const(value.clone()), false));
        }
        let (base, nullable) = match get(entries, "type") {
            Some(Json::Scalar(Value::String(t))) => (t.clone(), false),
            Some(Json::Array(types)) => {
                let names: Vec<&str> = types
                    .iter()
                    .map(|t| match t {
                        Json::Scalar(Value::String(s)) => Ok(s.as_str()),
                        _ => Err(invalid(pointer, "type members must be strings")),
                    })
                    .collect::<Result<_, _>>()?;
                let mut rest = names.iter().filter(|n| **n != "null");
                match (rest.next(), rest.next()) {
                    (Some(base), None) => ((*base).to_owned(), names.contains(&"null")),
                    _ => return Err(invalid(pointer, "type list must be one type plus null")),
                }
            }
            _ => return Err(invalid(pointer, "a schema needs type, enum or const")),
        };
        let kind = match base.as_str() {
            "string" => Kind::Str {
                min_len: count(entries, "minLength", pointer)?,
                pattern: pattern(entries, "pattern", pointer)?,
                format: string(entries, "format", pointer)?,
            },
            "integer" => Kind::Int,
            "boolean" => Kind::Bool,
            "array" => {
                let items =
                    get(entries, "items").ok_or_else(|| invalid(pointer, "array needs items"))?;
                Kind::Array {
                    items: Box::new(self.node(items, &format!("{pointer}/items"), false, depth)?),
                    min_items: count(entries, "minItems", pointer)?,
                }
            }
            "object" => self.object(entries, pointer, depth)?,
            other => return Err(invalid(pointer, format!("unsupported type {other:?}"))),
        };
        Ok((kind, nullable))
    }

    fn object(
        &self,
        entries: &[(String, Json)],
        pointer: &str,
        depth: usize,
    ) -> Result<Kind, SchemaError> {
        let props = match get(entries, "properties") {
            Some(Json::Object(props)) => props
                .iter()
                .map(|(key, node)| {
                    let at = format!("{pointer}/properties/{key}");
                    Ok((key.clone(), self.node(node, &at, false, depth)?))
                })
                .collect::<Result<Vec<_>, SchemaError>>()?,
            Some(_) => return Err(invalid(pointer, "properties must be an object")),
            None => Vec::new(),
        };
        let required = names(get(entries, "required"), pointer)?;
        match get(entries, "additionalProperties") {
            Some(extra @ Json::Object(_)) => {
                if !props.is_empty() {
                    return Err(invalid(
                        pointer,
                        "properties beside additionalProperties schema",
                    ));
                }
                let key_pattern = match get(entries, "propertyNames") {
                    Some(Json::Object(names)) => {
                        pattern(names, "pattern", &format!("{pointer}/propertyNames"))?
                    }
                    Some(_) => return Err(invalid(pointer, "propertyNames must be an object")),
                    None => None,
                };
                Ok(Kind::Map {
                    value: Box::new(self.node(
                        extra,
                        &format!("{pointer}/additionalProperties"),
                        false,
                        depth,
                    )?),
                    key_pattern,
                    min_props: count(entries, "minProperties", pointer)?,
                })
            }
            other => Ok(Kind::Object {
                props,
                required: required.into_iter().collect(),
                closed: matches!(other, Some(Json::Scalar(Value::Bool(false)))),
            }),
        }
    }
}

fn condition(item: &Json, pointer: &str) -> Result<Condition, SchemaError> {
    let Json::Object(entries) = item else {
        return Err(invalid(pointer, "allOf members must be objects"));
    };
    if let Some((key, _)) = entries
        .iter()
        .find(|(k, _)| !matches!(k.as_str(), "$comment" | "if" | "then"))
    {
        return Err(SchemaError::UnsupportedKeyword {
            pointer: pointer.to_owned(),
            keyword: key.clone(),
        });
    }
    let (Some(Json::Object(cond)), Some(Json::Object(then))) =
        (get(entries, "if"), get(entries, "then"))
    else {
        return Err(invalid(pointer, "allOf members must carry if and then"));
    };
    Ok(Condition {
        kinds: kinds_selected(cond, &format!("{pointer}/if"))?,
        then: overlay(then, &format!("{pointer}/then"))?,
    })
}

fn only_keys(
    entries: &[(String, Json)],
    allowed: &[&str],
    pointer: &str,
) -> Result<(), SchemaError> {
    match entries.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((key, _)) => Err(SchemaError::UnsupportedKeyword {
            pointer: pointer.to_owned(),
            keyword: key.clone(),
        }),
        None => Ok(()),
    }
}

/// `{"required": [...], "properties": {"kind": {"enum": [...]} | {"const": …}}}`.
fn kinds_selected(cond: &[(String, Json)], pointer: &str) -> Result<Vec<String>, SchemaError> {
    only_keys(cond, &["required", "properties"], pointer)?;
    let Some(Json::Object(props)) = get(cond, "properties") else {
        return Err(invalid(pointer, "if must test properties.kind"));
    };
    let [(key, Json::Object(test))] = props.as_slice() else {
        return Err(invalid(pointer, "if must test exactly one property"));
    };
    if key != "kind" {
        return Err(invalid(pointer, "if must test properties.kind"));
    }
    only_keys(test, &["enum", "const"], pointer)?;
    match (get(test, "enum"), get(test, "const")) {
        (Some(Json::Array(values)), None) => values
            .iter()
            .map(|v| match v {
                Json::Scalar(Value::String(s)) => Ok(s.clone()),
                _ => Err(invalid(pointer, "kind values must be strings")),
            })
            .collect(),
        (None, Some(Json::Scalar(Value::String(s)))) => Ok(vec![s.clone()]),
        _ => Err(invalid(pointer, "if must test kind by enum or const")),
    }
}

fn overlay(then: &[(String, Json)], pointer: &str) -> Result<Overlay, SchemaError> {
    only_keys(then, &["required", "properties"], pointer)?;
    let mut out = Overlay {
        required: names(get(then, "required"), pointer)?,
        nested: Vec::new(),
    };
    match get(then, "properties") {
        Some(Json::Object(props)) => {
            for (key, node) in props {
                let at = format!("{pointer}/properties/{key}");
                let Json::Object(inner) = node else {
                    return Err(invalid(&at, "must be an object"));
                };
                only_keys(inner, &["required"], &at)?;
                out.nested
                    .push((key.clone(), names(get(inner, "required"), &at)?));
            }
        }
        Some(_) => return Err(invalid(pointer, "properties must be an object")),
        None => {}
    }
    Ok(out)
}

fn get<'a>(entries: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn string(
    entries: &[(String, Json)],
    key: &str,
    pointer: &str,
) -> Result<Option<String>, SchemaError> {
    match get(entries, key) {
        Some(Json::Scalar(Value::String(s))) => Ok(Some(s.clone())),
        Some(_) => Err(invalid(pointer, format!("{key} must be a string"))),
        None => Ok(None),
    }
}

fn count(entries: &[(String, Json)], key: &str, pointer: &str) -> Result<usize, SchemaError> {
    match get(entries, key) {
        Some(Json::Scalar(Value::Number(n))) => n
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| invalid(pointer, format!("{key} must be a non-negative integer"))),
        Some(_) => Err(invalid(
            pointer,
            format!("{key} must be a non-negative integer"),
        )),
        None => Ok(0),
    }
}

fn pattern(
    entries: &[(String, Json)],
    key: &str,
    pointer: &str,
) -> Result<Option<Pattern>, SchemaError> {
    let Some(source) = string(entries, key, pointer)? else {
        return Ok(None);
    };
    let regex = Regex::new(&source).map_err(|source| SchemaError::BadPattern {
        pointer: pointer.to_owned(),
        source,
    })?;
    Ok(Some(Pattern { source, regex }))
}

fn names(json: Option<&Json>, pointer: &str) -> Result<Vec<String>, SchemaError> {
    match json {
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| match item {
                Json::Scalar(Value::String(s)) => Ok(s.clone()),
                _ => Err(invalid(pointer, "required must list strings")),
            })
            .collect(),
        Some(_) => Err(invalid(pointer, "required must be an array")),
        None => Ok(Vec::new()),
    }
}

/// JSON with object keys in document order, which `serde_json::Value` does not
/// keep.
#[derive(Debug)]
enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    Scalar(Value),
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Json;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Scalar(Value::Bool(v)))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Scalar(Value::from(v)))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Scalar(Value::from(v)))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
                Number::from_f64(v)
                    .map(|n| Json::Scalar(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }

            fn visit_str<E>(self, v: &str) -> Result<Json, E> {
                Ok(Json::Scalar(Value::String(v.to_owned())))
            }

            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Scalar(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Json::Object(entries))
            }
        }

        deserializer.deserialize_any(V)
    }
}
