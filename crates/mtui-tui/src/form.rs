//! The editor's model: the fields of a report document, their constraints and
//! the edits made so far. No terminal I/O.
//!
//! Edits go to a working copy of the document's JSON, each checked against its
//! schema node as it is made. [`Form::save`] then replays the changed sections
//! through [`ReportDocument::with_section`], which stays the authority on what
//! a valid document is.

use std::collections::BTreeSet;

use mtui_types::report_document::{ReportDocument, Section, SectionWriteError, null_pointers};
use serde_json::Value;
use thiserror::Error;

use crate::policy;
use crate::schema::{Kind, Node, Schema};

/// An edit or a save that was refused, with the RFC 6901 pointer of the field
/// at fault.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct FieldError {
    pub pointer: String,
    pub message: String,
}

fn refuse(pointer: &str, message: impl Into<String>) -> FieldError {
    FieldError {
        pointer: pointer.to_owned(),
        message: message.into(),
    }
}

/// A saved document and the section pointers it changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub document: ReportDocument,
    pub touched: Vec<String>,
}

/// A group of sections shown on one screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub name: &'static str,
    pub sections: Vec<Section>,
    pub writable: bool,
}

/// How a field is shown and edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Widget {
    /// An object, array or map; `len` counts its children.
    Group {
        expanded: bool,
        len: usize,
    },
    /// An optional subtree the document lacks.
    Absent {
        creatable: bool,
    },
    /// One of a fixed set of strings.
    Picker {
        nullable: bool,
    },
    /// `true` or `false`.
    Tristate {
        nullable: bool,
    },
    Text {
        multiline: bool,
        nullable: bool,
    },
    Integer {
        nullable: bool,
    },
    /// Shown, never edited: constants and keys the schema leaves open.
    Fixed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub pointer: String,
    pub depth: u16,
    pub label: String,
    pub help: Option<String>,
    pub widget: Widget,
    pub value: Value,
    pub editable: bool,
}

const TABS: [(&str, &[Section], bool); 7] = [
    ("summary", &[Section::Verdict, Section::Comment], true),
    ("people", &[Section::People], true),
    ("issues", &[Section::Issues], true),
    ("testing", &[Section::Testing], true),
    ("review", &[Section::Review], true),
    ("update", &[Section::Update], false),
    ("install", &[Section::Install], false),
];

/// Groups nested deeper than this start collapsed.
const EXPANDED_DEPTH: u16 = 3;

/// Where a node sits while the field list is built.
#[derive(Clone, Copy)]
struct Pos<'a> {
    pointer: &'a str,
    label: &'a str,
    depth: u16,
    top: bool,
}

/// The depth at which a group's children are listed.
struct Below {
    depth: u16,
}

impl Below {
    fn at<'a>(&self, pointer: &'a str, label: &'a str) -> Pos<'a> {
        Pos {
            pointer,
            label,
            depth: self.depth,
            top: false,
        }
    }
}

pub struct Form {
    root: Node,
    document: ReportDocument,
    original: Value,
    work: Value,
    tabs: Vec<Tab>,
    toggled: BTreeSet<String>,
}

impl Form {
    #[must_use]
    pub fn new(schema: &Schema, document: &ReportDocument) -> Self {
        let kind = serde_json::to_value(document.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let original = serde_json::to_value(document).expect("ReportDocument always serializes");
        let tabs = TABS
            .iter()
            .filter(|(_, sections, _)| sections.iter().any(|s| original.get(s.as_str()).is_some()))
            .map(|(name, sections, writable)| Tab {
                name,
                sections: sections.to_vec(),
                writable: *writable,
            })
            .collect();
        Self {
            root: schema.for_kind(&kind),
            document: document.clone(),
            work: original.clone(),
            original,
            tabs,
            toggled: BTreeSet::new(),
        }
    }

    /// The report's normalized review request id.
    #[must_use]
    pub fn document_id(&self) -> &str {
        &self.document.id
    }

    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.work != self.original
    }

    /// How many `null` leaves the document still has, edits included.
    #[must_use]
    pub fn unfilled(&self) -> usize {
        null_pointers(&self.work, "").len()
    }

    /// The fields of `tab`, flattened in display order.
    #[must_use]
    pub fn fields(&self, tab: usize) -> Vec<Field> {
        let mut out = Vec::new();
        let Some(tab) = self.tabs.get(tab) else {
            return out;
        };
        for section in &tab.sections {
            let key = section.as_str();
            let (Some(node), Some(value)) =
                (self.root.lookup(&format!("/{key}")), self.work.get(key))
            else {
                continue;
            };
            let at = Pos {
                pointer: &format!("/{key}"),
                label: key,
                depth: 0,
                top: true,
            };
            self.emit(node, value, at, &mut out);
        }
        out
    }

    /// Expand or collapse the group at `pointer`.
    pub fn toggle_group(&mut self, pointer: &str) {
        if !self.toggled.remove(pointer) {
            self.toggled.insert(pointer.to_owned());
        }
    }

    fn emit(&self, node: &Node, value: &Value, at: Pos<'_>, out: &mut Vec<Field>) {
        let (inner, nullable) = node.unwrapped();
        let help = node.description.clone().or_else(|| node.title.clone());
        let leaf = |widget: Widget| Field {
            pointer: at.pointer.to_owned(),
            depth: at.depth,
            label: at.label.to_owned(),
            help: help.clone(),
            editable: policy::is_editable(at.pointer) && widget != Widget::Fixed,
            widget,
            value: value.clone(),
        };
        match &inner.kind {
            Kind::Object { props, closed, .. } => {
                let Some(map) = value.as_object() else {
                    return out.push(leaf(Widget::Fixed));
                };
                let Some(below) = self.open_group(out, at, false, map.len(), &help, value) else {
                    return;
                };
                for (key, child) in props {
                    let pointer = format!("{}/{}", at.pointer, escape(key));
                    match map.get(key) {
                        Some(v) => self.emit(child, v, below.at(&pointer, key), out),
                        None if policy::is_creatable_root(&pointer) => out.push(Field {
                            pointer,
                            depth: below.depth,
                            label: key.clone(),
                            help: child.description.clone(),
                            widget: Widget::Absent {
                                creatable: synthesize(child).is_some(),
                            },
                            value: Value::Null,
                            editable: false,
                        }),
                        None => {}
                    }
                }
                if !closed {
                    for (key, v) in map
                        .iter()
                        .filter(|(k, _)| !props.iter().any(|(p, _)| p == *k))
                    {
                        out.push(Field {
                            pointer: format!("{}/{}", at.pointer, escape(key)),
                            depth: below.depth,
                            label: key.clone(),
                            help: None,
                            widget: Widget::Fixed,
                            value: v.clone(),
                            editable: false,
                        });
                    }
                }
            }
            Kind::Array { items, .. } => {
                let Some(list) = value.as_array() else {
                    return out.push(leaf(Widget::Fixed));
                };
                let Some(below) = self.open_group(out, at, false, list.len(), &help, value) else {
                    return;
                };
                for (i, v) in list.iter().enumerate() {
                    let pointer = format!("{}/{i}", at.pointer);
                    self.emit(items, v, below.at(&pointer, &format!("#{i}")), out);
                }
            }
            Kind::Map { value: item, .. } => {
                let Some(map) = value.as_object() else {
                    return out.push(leaf(Widget::Fixed));
                };
                let Some(below) = self.open_group(out, at, true, map.len(), &help, value) else {
                    return;
                };
                for (key, v) in map {
                    let pointer = format!("{}/{}", at.pointer, escape(key));
                    self.emit(item, v, below.at(&pointer, key), out);
                }
            }
            Kind::Str { .. } => out.push(leaf(Widget::Text {
                multiline: at.label == "comment",
                nullable,
            })),
            Kind::Int => out.push(leaf(Widget::Integer { nullable })),
            Kind::Bool => out.push(leaf(Widget::Tristate { nullable })),
            Kind::Enum(_) => out.push(leaf(Widget::Picker { nullable })),
            Kind::Const(_) | Kind::Nullable(_) => out.push(leaf(Widget::Fixed)),
        }
    }

    /// Push the group row (a section root has none) and return where its
    /// children go, or `None` when the group is collapsed.
    fn open_group(
        &self,
        out: &mut Vec<Field>,
        at: Pos<'_>,
        is_map: bool,
        len: usize,
        help: &Option<String>,
        value: &Value,
    ) -> Option<Below> {
        if at.top {
            return Some(Below { depth: at.depth });
        }
        let default_open = at.depth < EXPANDED_DEPTH && !(is_map && at.depth >= 1);
        let expanded = default_open != self.toggled.contains(at.pointer);
        let label = match value.get("title").and_then(Value::as_str) {
            Some(title) => format!("{}  {title}", at.label),
            None => at.label.to_owned(),
        };
        out.push(Field {
            pointer: at.pointer.to_owned(),
            depth: at.depth,
            label,
            help: help.clone(),
            widget: Widget::Group { expanded, len },
            value: Value::Null,
            editable: false,
        });
        expanded.then_some(Below {
            depth: at.depth + 1,
        })
    }

    /// The tab and row of the field at `pointer`, opening any collapsed group
    /// above it. When the pointer names nothing shown, the nearest enclosing
    /// field is returned instead.
    pub fn locate(&mut self, pointer: &str) -> Option<(usize, usize)> {
        let mut target = pointer.to_owned();
        loop {
            for _ in 0..MAX_NESTING {
                if let Some(hit) = self.find(&target) {
                    return Some(hit);
                }
                let Some(closed) = self.closed_ancestor(&target) else {
                    break;
                };
                self.toggle_group(&closed);
            }
            let (parent, _) = target.rsplit_once('/')?;
            if parent.is_empty() {
                return None;
            }
            target = parent.to_owned();
        }
    }

    fn find(&self, pointer: &str) -> Option<(usize, usize)> {
        (0..self.tabs.len()).find_map(|tab| {
            let row = self.fields(tab).iter().position(|f| f.pointer == pointer)?;
            Some((tab, row))
        })
    }

    fn closed_ancestor(&self, pointer: &str) -> Option<String> {
        (0..self.tabs.len()).find_map(|tab| {
            self.fields(tab).into_iter().find_map(|f| {
                let hidden = matches!(
                    f.widget,
                    Widget::Group {
                        expanded: false,
                        ..
                    }
                ) && pointer.starts_with(&format!("{}/", f.pointer));
                hidden.then_some(f.pointer)
            })
        })
    }

    /// Replace the leaf at `pointer`.
    ///
    /// # Errors
    ///
    /// Refused when the leaf is read-only, absent, or `value` breaks the
    /// field's `enum`, `pattern`, `minLength` or integer constraint.
    pub fn set(&mut self, pointer: &str, value: Value) -> Result<(), FieldError> {
        let node = self
            .root
            .lookup(pointer)
            .ok_or_else(|| refuse(pointer, "no such field"))?;
        if !policy::is_editable(pointer) {
            return Err(refuse(pointer, "read-only"));
        }
        check(node, &value).map_err(|message| refuse(pointer, message))?;
        let slot = self
            .work
            .pointer_mut(pointer)
            .ok_or_else(|| refuse(pointer, "not present in this report"))?;
        *slot = value;
        Ok(())
    }

    /// Replace the leaf at `pointer` with `text`, read as the field's type.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set), plus a refusal when an integer field gets text
    /// that is not an integer.
    pub fn set_text(&mut self, pointer: &str, text: &str) -> Result<(), FieldError> {
        let is_int = self
            .root
            .lookup(pointer)
            .is_some_and(|n| matches!(n.unwrapped().0.kind, Kind::Int));
        let value = if is_int {
            text.trim()
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| refuse(pointer, "must be an integer"))?
        } else {
            Value::String(text.to_owned())
        };
        self.set(pointer, value)
    }

    /// Set the leaf at `pointer` to `null`.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set); also refused when the field is not nullable.
    pub fn set_null(&mut self, pointer: &str) -> Result<(), FieldError> {
        self.set(pointer, Value::Null)
    }

    /// The values the field at `pointer` can take when it is a picker or a
    /// tristate, `null` first when allowed.
    #[must_use]
    pub fn choices(&self, pointer: &str) -> Option<Vec<Value>> {
        let (inner, nullable) = self.root.lookup(pointer)?.unwrapped();
        let mut out: Vec<Value> = nullable.then_some(Value::Null).into_iter().collect();
        match &inner.kind {
            Kind::Enum(options) => out.extend(options.iter().cloned().map(Value::String)),
            Kind::Bool => out.extend([Value::Bool(true), Value::Bool(false)]),
            _ => return None,
        }
        Some(out)
    }

    /// Step a picker or tristate to its next (or previous) choice.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set); also refused when the field has no fixed choices.
    pub fn cycle(&mut self, pointer: &str, forward: bool) -> Result<(), FieldError> {
        let choices = self
            .choices(pointer)
            .ok_or_else(|| refuse(pointer, "not a choice field"))?;
        let current = self.work.pointer(pointer).unwrap_or(&Value::Null);
        let at = choices.iter().position(|c| c == current).unwrap_or(0);
        let next = if forward {
            (at + 1) % choices.len()
        } else {
            (at + choices.len() - 1) % choices.len()
        };
        self.set(pointer, choices[next].clone())
    }

    /// Create the absent subtree at `pointer`.
    ///
    /// # Errors
    ///
    /// Refused when the subtree is not one a tester may create, or when no
    /// minimal value exists for it (a required field that cannot be `null`).
    pub fn create(&mut self, pointer: &str) -> Result<(), FieldError> {
        let node = self
            .root
            .lookup(pointer)
            .filter(|_| policy::is_creatable_root(pointer))
            .ok_or_else(|| refuse(pointer, "cannot be created here"))?;
        let value = synthesize(node)
            .ok_or_else(|| refuse(pointer, "cannot be created by hand; export builds it"))?;
        let (parent, key) = pointer
            .rsplit_once('/')
            .ok_or_else(|| refuse(pointer, "no parent"))?;
        let key = key.replace("~1", "/").replace("~0", "~");
        let map = self
            .work
            .pointer_mut(parent)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| refuse(pointer, "parent is not an object"))?;
        if map.contains_key(&key) {
            return Err(refuse(pointer, "already present"));
        }
        map.insert(key, value);
        Ok(())
    }

    /// Replay every changed section onto the document through
    /// [`ReportDocument::with_section`].
    ///
    /// # Errors
    ///
    /// The first section the document refuses, naming the offending pointer.
    pub fn save(&self) -> Result<Saved, FieldError> {
        let mut document = self.document.clone();
        let mut touched = Vec::new();
        for section in Section::ALL {
            let key = section.as_str();
            let Some(value) = self.work.get(key) else {
                continue;
            };
            if self.original.get(key) == Some(value) {
                continue;
            }
            document = document
                .with_section(section, value.clone())
                .map_err(|err| section_error(section, &err))?;
            touched.push(format!("/{key}"));
        }
        Ok(Saved { document, touched })
    }
}

const MAX_NESTING: usize = 32;

fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn section_error(section: Section, err: &SectionWriteError) -> FieldError {
    let whole = format!("/{section}");
    let pointer = match err {
        SectionWriteError::Invalid(e) if e.pointer != "/" => e.pointer.clone(),
        SectionWriteError::Dropped(pointers) => pointers.first().cloned().unwrap_or(whole),
        _ => whole,
    };
    let message = match err {
        SectionWriteError::Invalid(e) => e.to_string(),
        other => other.to_string(),
    };
    refuse(&pointer, message)
}

fn check(node: &Node, value: &Value) -> Result<(), String> {
    let (inner, nullable) = node.unwrapped();
    if value.is_null() {
        return if nullable {
            Ok(())
        } else {
            Err("cannot be null".to_owned())
        };
    }
    match &inner.kind {
        Kind::Enum(options) => match value.as_str() {
            Some(s) if options.iter().any(|o| o == s) => Ok(()),
            _ => Err(format!("must be one of: {}", options.join(", "))),
        },
        Kind::Str {
            min_len, pattern, ..
        } => {
            let text = value.as_str().ok_or("must be text")?;
            if text.chars().count() < *min_len {
                return Err(format!("must be at least {min_len} characters"));
            }
            match pattern {
                Some(p) if !p.is_match(text) => Err(format!("must match {}", p.as_str())),
                _ => Ok(()),
            }
        }
        Kind::Int if value.is_i64() || value.is_u64() => Ok(()),
        Kind::Int => Err("must be an integer".to_owned()),
        Kind::Bool if value.is_boolean() => Ok(()),
        Kind::Bool => Err("must be true or false".to_owned()),
        Kind::Const(expected) if value == expected => Ok(()),
        Kind::Const(expected) => Err(format!("must be {expected}")),
        Kind::Object { .. } | Kind::Array { .. } | Kind::Map { .. } | Kind::Nullable(_) => {
            Err("not a single value".to_owned())
        }
    }
}

/// The smallest value for an object whose every required field may be `null`.
fn synthesize(node: &Node) -> Option<Value> {
    let Kind::Object {
        props, required, ..
    } = &node.unwrapped().0.kind
    else {
        return None;
    };
    let mut map = serde_json::Map::new();
    for name in required {
        let (_, child) = props.iter().find(|(k, _)| k == name)?;
        if !child.unwrapped().1 {
            return None;
        }
        map.insert(name.clone(), Value::Null);
    }
    Some(Value::Object(map))
}
