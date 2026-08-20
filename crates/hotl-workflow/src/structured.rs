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
