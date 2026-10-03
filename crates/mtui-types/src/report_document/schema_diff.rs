//! Comparing a live report schema against the one this build ships.
//!
//! By **value**, never by bytes: the server serves Mojo::JSON's compact,
//! key-sorted encoding while the committed copy is pretty-printed, so a byte
//! comparison would report drift permanently.

use std::collections::BTreeSet;

use serde_json::Value;

use super::SCHEMA_JSON;

/// Human-readable `pointer: detail` lines for every value that differs between
/// `committed` and `live`; empty when they are equal.
#[must_use]
pub fn schema_diffs(committed: &Value, live: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_diffs(committed, live, &mut String::new(), &mut out);
    out
}

/// The first difference between `live` and the schema this build ships, or
/// `None` when they are equal.
#[must_use]
pub fn schema_drift(live: &Value) -> Option<String> {
    let committed: Value =
        serde_json::from_str(SCHEMA_JSON).expect("the committed schema is valid JSON");
    schema_diffs(&committed, live).into_iter().next()
}

fn collect_diffs(committed: &Value, live: &Value, path: &mut String, out: &mut Vec<String>) {
    if committed == live {
        return;
    }
    match (committed, live) {
        (Value::Object(a), Value::Object(b)) => {
            let mut keys: BTreeSet<&String> = a.keys().collect();
            keys.extend(b.keys());
            for key in keys {
                let mark = path.len();
                path.push('/');
                path.push_str(key);
                match (a.get(key), b.get(key)) {
                    (Some(av), Some(bv)) => collect_diffs(av, bv, path, out),
                    (Some(_), None) => out.push(format!("{path}: removed from live")),
                    (None, Some(_)) => out.push(format!("{path}: added to live")),
                    (None, None) => unreachable!("key came from one of the two maps"),
                }
                path.truncate(mark);
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (av, bv)) in a.iter().zip(b.iter()).enumerate() {
                let mark = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                collect_diffs(av, bv, path, out);
                path.truncate(mark);
            }
        }
        _ => {
            let pointer = if path.is_empty() { "/" } else { path.as_str() };
            out.push(format!("{pointer}: {committed} != {live}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed() -> Value {
        serde_json::from_str(SCHEMA_JSON).unwrap()
    }

    #[test]
    fn identical_values_produce_no_diffs() {
        let v = serde_json::json!({"a": 1, "b": [1, 2, {"c": "x"}]});
        assert!(schema_diffs(&v, &v).is_empty());
    }

    #[test]
    fn a_changed_leaf_reports_its_pointer() {
        let committed = serde_json::json!({"properties": {"kind": {"const": "1.0"}}});
        let live = serde_json::json!({"properties": {"kind": {"const": "1.1"}}});
        assert_eq!(
            schema_diffs(&committed, &live),
            vec!["/properties/kind/const: \"1.0\" != \"1.1\""]
        );
    }

    #[test]
    fn an_added_key_is_reported() {
        let committed = serde_json::json!({"a": 1});
        let live = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(schema_diffs(&committed, &live), vec!["/b: added to live"]);
    }

    #[test]
    fn a_removed_key_is_reported() {
        let committed = serde_json::json!({"a": 1, "b": 2});
        let live = serde_json::json!({"a": 1});
        assert_eq!(
            schema_diffs(&committed, &live),
            vec!["/b: removed from live"]
        );
    }

    #[test]
    fn the_shipped_schema_does_not_drift_from_itself() {
        assert_eq!(schema_drift(&committed()), None);
    }

    /// The live form is compact where the committed copy is pretty-printed.
    #[test]
    fn a_reformatted_but_equal_schema_is_not_drift() {
        let compact = serde_json::to_string(&committed()).unwrap();
        assert_ne!(compact, SCHEMA_JSON, "the two forms must differ as bytes");
        let live: Value = serde_json::from_str(&compact).unwrap();
        assert_eq!(schema_drift(&live), None);
    }

    #[test]
    fn a_changed_description_reports_its_pointer() {
        let mut live = committed();
        let (pointer, original) = first_description(&live).expect("the schema has descriptions");
        *live.pointer_mut(&pointer).unwrap() = Value::String(format!("{original} (changed)"));

        let drift = schema_drift(&live).expect("a changed description is drift");
        assert!(drift.starts_with(&pointer), "{drift} vs {pointer}");
    }

    /// JSON pointer and text of some `description` in `v`.
    fn first_description(v: &Value) -> Option<(String, String)> {
        fn walk(v: &Value, path: &mut String) -> Option<(String, String)> {
            match v {
                Value::Object(map) => {
                    for (key, child) in map {
                        let mark = path.len();
                        path.push('/');
                        path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                        if key == "description"
                            && let Value::String(text) = child
                        {
                            return Some((path.clone(), text.clone()));
                        }
                        if let Some(found) = walk(child, path) {
                            return Some(found);
                        }
                        path.truncate(mark);
                    }
                    None
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        let mark = path.len();
                        path.push_str(&format!("/{index}"));
                        if let Some(found) = walk(child, path) {
                            return Some(found);
                        }
                        path.truncate(mark);
                    }
                    None
                }
                _ => None,
            }
        }
        walk(v, &mut String::new())
    }
}
