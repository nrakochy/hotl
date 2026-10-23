//! Schema-shaped answers: fence stripping, validation with an instructive
//! error, and the contract text a child is briefed with. Shared by
//! `hotl --json-schema` and the workflow runner's per-agent schemas.

use serde_json::Value;

/// Strip a ```json … ``` (or bare ``` … ```) fence, returning the inner text.
pub fn strip_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(after) = t.strip_prefix("```") else {
        return t;
    };
    // Drop an optional language tag on the first line.
    let after = after
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or(after);
    after.strip_suffix("```").unwrap_or(after).trim()
}

/// Parse + validate against the schema. `Err` is an *instructive* message (the
/// model reads it on retry): parse errors and up to 3 schema violations.
pub fn validate(schema: &jsonschema::Validator, text: &str) -> Result<Value, String> {
    let inner = strip_fences(text);
    let value: Value =
        serde_json::from_str(inner).map_err(|e| format!("The reply was not valid JSON: {e}"))?;
    let errors: Vec<String> = schema
        .iter_errors(&value)
        .take(3)
        .map(|e| format!("{}: {e}", e.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(format!(
            "The JSON did not match the schema:\n{}",
            errors.join("\n")
        ))
    }
}

/// The output contract as text: pushed into context tagged (`--json-schema`)
/// or inlined ahead of a workflow agent's prompt.
pub fn contract_text(schema: &Value) -> String {
    format!(
        "<output-contract>\nReply with a single JSON object valid against this JSON Schema, \
         and nothing else:\n{schema}\n</output-contract>"
    )
}

/// Keywords the structured-output grammar does not accept (0073 D10). The
/// client-side validator keeps enforcing them against the original schema.
const UNSUPPORTED_KEYWORDS: [&str; 11] = [
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
];

/// The schema as Anthropic's `output_config.format` accepts it (0073 D10): a
/// copy with `additionalProperties: false` on every object and the
/// unsupported constraint keywords removed. `None` when the schema uses
/// `$ref` anywhere — recursion is not cheaply provable, so the caller falls
/// back to the unconstrained retry. Pure; the loop still validates the answer
/// against the original.
pub fn api_schema(schema: &Value) -> Option<Value> {
    let mut out = schema.clone();
    adapt(&mut out).then_some(out)
}

/// Walk one schema node in place. `false` = a `$ref` was found.
fn adapt(node: &mut Value) -> bool {
    let Some(obj) = node.as_object_mut() else {
        // `items: [..]`, `anyOf: [..]` and friends are arrays of schemas.
        if let Some(arr) = node.as_array_mut() {
            return arr.iter_mut().all(adapt);
        }
        return true;
    };
    if obj.contains_key("$ref") {
        return false;
    }
    for key in UNSUPPORTED_KEYWORDS {
        obj.remove(key);
    }
    let is_object =
        obj.get("type").and_then(Value::as_str) == Some("object") || obj.contains_key("properties");
    if is_object && obj.get("additionalProperties") != Some(&Value::Bool(false)) {
        obj.insert("additionalProperties".into(), Value::Bool(false));
    }
    // Sub-schemas live under these keys; `properties`/`$defs`/`patternProperties`
    // map names to schemas, the rest are a schema or a list of them.
    for key in ["properties", "$defs", "definitions", "patternProperties"] {
        if let Some(map) = obj.get_mut(key).and_then(Value::as_object_mut) {
            for child in map.values_mut() {
                if !adapt(child) {
                    return false;
                }
            }
        }
    }
    for key in [
        "items",
        "prefixItems",
        "anyOf",
        "allOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "additionalProperties",
    ] {
        if let Some(child) = obj.get_mut(key) {
            if (child.is_object() || child.is_array()) && !adapt(child) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod api_schema_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_schema_adds_additional_properties_false_to_every_object() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "nested": {"type": "object", "properties": {"x": {"type": "integer"}}},
                "list": {"type": "array", "items": {"properties": {"y": {"type": "string"}}}}
            },
            "required": ["name"]
        });
        let a = api_schema(&s).unwrap();
        assert_eq!(a["additionalProperties"], false);
        assert_eq!(a["properties"]["nested"]["additionalProperties"], false);
        assert_eq!(
            a["properties"]["list"]["items"]["additionalProperties"], false,
            "an object implied by `properties` alone counts"
        );
        assert_eq!(a["required"], json!(["name"]));
        assert!(a["properties"]["name"]
            .get("additionalProperties")
            .is_none());
    }

    #[test]
    fn api_schema_strips_numeric_and_string_constraints_but_keeps_enum_and_required() {
        let s = json!({
            "type": "object",
            "required": ["n", "s"],
            "properties": {
                "n": {"type": "integer", "minimum": 1, "maximum": 9, "multipleOf": 3},
                "s": {"type": "string", "minLength": 2, "maxLength": 5, "pattern": "^a", "enum": ["ab", "ac"]},
                "l": {"type": "array", "minItems": 1, "maxItems": 3, "uniqueItems": true, "items": {"type": "string"}}
            }
        });
        let a = api_schema(&s).unwrap();
        for k in UNSUPPORTED_KEYWORDS {
            assert!(a["properties"]["n"].get(k).is_none(), "{k}");
            assert!(a["properties"]["s"].get(k).is_none(), "{k}");
            assert!(a["properties"]["l"].get(k).is_none(), "{k}");
        }
        assert_eq!(a["properties"]["s"]["enum"], json!(["ab", "ac"]));
        assert_eq!(a["properties"]["n"]["type"], "integer");
        assert_eq!(a["required"], json!(["n", "s"]));
    }

    #[test]
    fn api_schema_returns_none_for_a_schema_with_a_ref() {
        let s = json!({
            "type": "object",
            "properties": {"child": {"$ref": "#/$defs/node"}},
            "$defs": {"node": {"type": "object"}}
        });
        assert_eq!(api_schema(&s), None);
        let deep =
            json!({"type": "array", "items": {"anyOf": [{"type": "string"}, {"$ref": "#"}]}});
        assert_eq!(api_schema(&deep), None);
    }

    #[test]
    fn api_schema_leaves_an_already_closed_object_alone() {
        let s = json!({"type": "object", "additionalProperties": false, "properties": {"a": {"type": "string", "minLength": 1}}});
        let once = api_schema(&s).unwrap();
        let twice = api_schema(&once).unwrap();
        assert_eq!(once, twice, "idempotent");
        assert_eq!(once["additionalProperties"], false);
    }

    /// The loop validates against the original: a stripped `minimum` still
    /// refuses the value the grammar would have let through.
    #[test]
    fn validation_still_enforces_a_stripped_minimum() {
        let original = json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 1}}, "required": ["n"]});
        let adapted = api_schema(&original).unwrap();
        assert!(adapted["properties"]["n"].get("minimum").is_none());
        let v = jsonschema::validator_for(&original).unwrap();
        assert!(validate(&v, r#"{"n": 0}"#).is_err());
        let loose = jsonschema::validator_for(&adapted).unwrap();
        assert!(
            validate(&loose, r#"{"n": 0}"#).is_ok(),
            "the API schema alone would accept it"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_reports_instructive_errors_and_strips_fences() {
        let schema = json!({"type":"object","required":["name"],
            "properties":{"name":{"type":"string"}}});
        let v = jsonschema::validator_for(&schema).unwrap();
        let err = validate(&v, r#"{"nome": "x"}"#).unwrap_err();
        assert!(err.contains("name"), "names the violation: {err}");
        assert!(validate(&v, "not json").unwrap_err().contains("JSON"));
        assert!(
            validate(&v, "```json\n{\"name\":\"x\"}\n```").is_ok(),
            "fences stripped"
        );
        assert_eq!(validate(&v, r#"{"name":"ok"}"#).unwrap()["name"], "ok");
        assert!(contract_text(&schema).contains("output-contract"));
    }
}
