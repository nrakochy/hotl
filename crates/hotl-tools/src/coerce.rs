//! Schema-shape mismatches in a tool call's arguments (0075 T1, D11). A body
//! that parses but has the wrong *shape* (`"limit": "5"`, a scalar where the
//! schema wants an array, a string that is itself JSON, `null` on an optional
//! property) reads as *missing* to a builtin's `input.get(..).and_then(as_u64)`.
//! This module only **detects** such calls; `hotl doctor --args` counts them
//! over the session logs so the owner can decide whether coercion is worth
//! building (D12). The walk is total and pure: `properties`, `items`,
//! `required` only — `additionalProperties`, `oneOf`/`anyOf`/`allOf` and
//! `$ref` subtrees are left alone, and undeclared properties are never judged.

use serde_json::Value;

/// One place where the input does not fit the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    /// A scalar of the wrong JSON type where a scalar type is declared
    /// (`"5"` for an integer, `1` for a string, `"true"` for a boolean).
    Type {
        path: String,
        want: &'static str,
        got: &'static str,
    },
    /// A scalar where the schema wants an array.
    ScalarForArray { path: String },
    /// A string that parses as JSON where the schema wants an object or array.
    EncodedJson { path: String },
    /// `null` on a property that is not required.
    NullOptional { path: String },
    /// A required property that is absent.
    MissingRequired { path: String },
}

impl Mismatch {
    /// The property path the mismatch is at (`limit`, `paths[0]`, `a.b`).
    pub fn path(&self) -> &str {
        match self {
            Self::Type { path, .. }
            | Self::ScalarForArray { path }
            | Self::EncodedJson { path }
            | Self::NullOptional { path }
            | Self::MissingRequired { path } => path,
        }
    }

    /// A short class name for a table column.
    pub fn class(&self) -> &'static str {
        match self {
            Self::Type { .. } => "type",
            Self::ScalarForArray { .. } => "scalar-for-array",
            Self::EncodedJson { .. } => "encoded-json",
            Self::NullOptional { .. } => "null-optional",
            Self::MissingRequired { .. } => "missing-required",
        }
    }
}

/// Every mismatch between `input` and `schema`, in property order.
pub fn mismatches(schema: &Value, input: &Value) -> Vec<Mismatch> {
    let mut out = Vec::new();
    walk_object(schema, input, "", &mut out);
    out
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The schema's declared type, when it is a single plain type we judge.
fn declared(schema: &Value) -> Option<&str> {
    // A union, `oneOf`/`anyOf`/`allOf` or `$ref` is out of scope (D11's stop set).
    if schema.get("oneOf").is_some()
        || schema.get("anyOf").is_some()
        || schema.get("allOf").is_some()
        || schema.get("$ref").is_some()
    {
        return None;
    }
    schema.get("type").and_then(Value::as_str)
}

fn walk_object(schema: &Value, input: &Value, prefix: &str, out: &mut Vec<Mismatch>) {
    let Some(obj) = input.as_object() else {
        return;
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (key, sub) in props {
        let path = join(prefix, key);
        match obj.get(key) {
            None if required.contains(&key.as_str()) => {
                out.push(Mismatch::MissingRequired { path })
            }
            None => {}
            Some(Value::Null) if !required.contains(&key.as_str()) => {
                out.push(Mismatch::NullOptional { path })
            }
            Some(v) => walk_value(sub, v, &path, out),
        }
    }
}

fn walk_value(schema: &Value, v: &Value, path: &str, out: &mut Vec<Mismatch>) {
    let Some(want) = declared(schema) else {
        return;
    };
    let got = type_name(v);
    match want {
        "object" => match v {
            Value::Object(_) => walk_object(schema, v, path, out),
            Value::String(s) if looks_like_json(s, '{') => {
                out.push(Mismatch::EncodedJson { path: path.into() })
            }
            Value::Null => {}
            _ => out.push(Mismatch::Type {
                path: path.into(),
                want: "object",
                got,
            }),
        },
        "array" => match v {
            Value::Array(items) => {
                if let Some(item_schema) = schema.get("items") {
                    for (i, item) in items.iter().enumerate() {
                        walk_value(item_schema, item, &format!("{path}[{i}]"), out);
                    }
                }
            }
            Value::String(s) if looks_like_json(s, '[') => {
                out.push(Mismatch::EncodedJson { path: path.into() })
            }
            Value::Null | Value::Object(_) => out.push(Mismatch::Type {
                path: path.into(),
                want: "array",
                got,
            }),
            _ => out.push(Mismatch::ScalarForArray { path: path.into() }),
        },
        "string" | "integer" | "number" | "boolean" => {
            let fits = match (want, v) {
                ("string", Value::String(_)) => true,
                ("integer", Value::Number(n)) => n.is_i64() || n.is_u64(),
                ("number", Value::Number(_)) => true,
                ("boolean", Value::Bool(_)) => true,
                _ => false,
            };
            if !fits && !v.is_null() {
                out.push(Mismatch::Type {
                    path: path.into(),
                    want: match want {
                        "string" => "string",
                        "integer" => "integer",
                        "number" => "number",
                        _ => "boolean",
                    },
                    got,
                });
            }
        }
        _ => {}
    }
}

/// A string that is JSON of the expected shape — double-encoded by the model.
fn looks_like_json(s: &str, open: char) -> bool {
    let t = s.trim();
    t.starts_with(open) && serde_json::from_str::<Value>(t).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"},
                "minified": {"type": "boolean"}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn mismatches_sees_a_numeric_string_for_an_integer() {
        let m = mismatches(&read_schema(), &json!({"path": "a.rs", "limit": "5"}));
        assert_eq!(
            m,
            vec![Mismatch::Type {
                path: "limit".into(),
                want: "integer",
                got: "string"
            }]
        );
        assert_eq!(m[0].class(), "type");
        assert!(mismatches(&read_schema(), &json!({"path": "a.rs", "limit": 5})).is_empty());
        // A float where an integer is wanted is a type mismatch too.
        assert_eq!(
            mismatches(&read_schema(), &json!({"path": "a.rs", "limit": 5.5}))[0].class(),
            "type"
        );
    }

    #[test]
    fn mismatches_sees_a_scalar_for_an_array() {
        let schema = json!({
            "type": "object",
            "properties": {"paths": {"type": "array", "items": {"type": "string"}}},
            "required": ["paths"]
        });
        assert_eq!(
            mismatches(&schema, &json!({"paths": "a.rs"})),
            vec![Mismatch::ScalarForArray {
                path: "paths".into()
            }]
        );
        assert!(mismatches(&schema, &json!({"paths": ["a.rs"]})).is_empty());
        // Items are walked: a number inside a string array is a `type` at `paths[1]`.
        let m = mismatches(&schema, &json!({"paths": ["a.rs", 7]}));
        assert_eq!(m[0].path(), "paths[1]");
        assert_eq!(m[0].class(), "type");
    }

    #[test]
    fn mismatches_sees_double_encoded_json() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "string"}},
                "opts": {"type": "object", "properties": {"a": {"type": "integer"}}}
            }
        });
        let m = mismatches(
            &schema,
            &json!({"items": "[\"a\", \"b\"]", "opts": "{\"a\": 1}"}),
        );
        assert_eq!(
            m,
            vec![
                Mismatch::EncodedJson {
                    path: "items".into()
                },
                Mismatch::EncodedJson {
                    path: "opts".into()
                },
            ]
        );
        // A plain string that merely starts with `[` is not JSON.
        assert_eq!(
            mismatches(&schema, &json!({"items": "[not json"}))[0].class(),
            "scalar-for-array"
        );
    }

    #[test]
    fn mismatches_sees_null_on_an_optional() {
        let m = mismatches(&read_schema(), &json!({"path": "a.rs", "limit": null}));
        assert_eq!(
            m,
            vec![Mismatch::NullOptional {
                path: "limit".into()
            }]
        );
        // `null` on a required property is not "optional null"; it is a
        // missing value the tool reports itself — one class, not two.
        let m = mismatches(&read_schema(), &json!({"path": null}));
        assert!(m.is_empty(), "{m:?}");
    }

    #[test]
    fn mismatches_reports_a_missing_required() {
        assert_eq!(
            mismatches(&read_schema(), &json!({"limit": 5})),
            vec![Mismatch::MissingRequired {
                path: "path".into()
            }]
        );
    }

    #[test]
    fn mismatches_ignores_undeclared_properties_and_oneof() {
        let schema = json!({
            "type": "object",
            "properties": {
                "either": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
                "any": {"anyOf": [{"type": "string"}, {"type": "array"}]},
                "linked": {"$ref": "#/$defs/x"},
                "loose": {}
            }
        });
        let input = json!({
            "either": true, "any": 3, "linked": 4, "loose": [1],
            "undeclared": "whatever", "another": null
        });
        assert!(mismatches(&schema, &input).is_empty());
        // A non-object input is not judged at all.
        assert!(mismatches(&read_schema(), &json!("just a string")).is_empty());
        assert!(mismatches(&json!({"type": "object"}), &json!({"x": 1})).is_empty());
    }

    /// Each builtin's documented example input walks clean against its real
    /// schema — pins the walker against what the tools actually declare.
    #[test]
    fn every_builtin_schema_walks_clean_on_its_own_example() {
        let registry = crate::Registry::builtin();
        let examples: Vec<(&str, Value)> = vec![
            (
                "read",
                json!({"path": "src/main.rs", "offset": 10, "limit": 40}),
            ),
            ("write", json!({"path": "notes.md", "content": "hello"})),
            (
                "edit",
                json!({"path": "src/lib.rs", "old_string": "a", "new_string": "b"}),
            ),
            ("glob", json!({"pattern": "**/*.rs"})),
            ("grep", json!({"pattern": "fn main", "path": "src"})),
            ("bash", json!({"command": "ls -la", "timeout_ms": 5000})),
            (
                "todo_write",
                json!({"todos": [{"id": "1", "content": "do it", "status": "pending", "active_form": "doing it"}]}),
            ),
        ];
        let defs = registry.defs();
        let mut seen = 0;
        for (name, example) in examples {
            let Some(def) = defs.iter().find(|d| d.name == name) else {
                continue;
            };
            let m = mismatches(&def.input_schema, &example);
            assert!(m.is_empty(), "{name}: {m:?} against {}", def.input_schema);
            seen += 1;
        }
        assert!(
            seen >= 6,
            "the builtin registry must expose the file and shell tools"
        );
    }
}
