//! `hotl -p "…" --json-schema <file>` — structured output with validation and
//! bounded retry. The schema rides into context as a
//! tagged instruction item; the final answer is validated against it; a
//! validation error feeds back as `RetryFeedback` for up to 2 retries
//! (LangChain `ToolStrategy.handle_errors` shape). Valid JSON → stdout;
//! exhaustion → non-zero exit.

use hotl_engine::{EngineEvent, Outcome, SessionHandle};
use hotl_types::{Item, SyntheticReason};
use serde_json::Value;

pub const MAX_RETRIES: u32 = 2;

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

/// The schema as a tagged instruction item pushed into the session's context.
pub fn contract_item(schema: &Value) -> Item {
    Item::User {
        text: format!(
            "<output-contract>\nReply with a single JSON object valid against this JSON Schema, \
             and nothing else:\n{schema}\n</output-contract>"
        ),
        synthetic: Some(SyntheticReason::SystemReminder),
    }
}

/// Drive the session: prompt, validate the answer, and on a validation error
/// feed it back (tagged `RetryFeedback`) up to `max_retries` times.
pub async fn run_structured(
    handle: &mut SessionHandle,
    schema: &Value,
    prompt: &str,
    max_retries: u32,
) -> Result<Value, String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| format!("the --json-schema file is not a valid JSON Schema: {e}"))?;
    handle.prompt(prompt.to_string()).await;
    let mut attempts = 0;
    loop {
        let text = wait_for_done(handle).await?;
        match validate(&validator, &text) {
            Ok(value) => return Ok(value),
            Err(e) if attempts < max_retries => {
                attempts += 1;
                handle
                    .prompt_tagged(
                        format!("Validation failed: {e}\nReply with only the corrected JSON object, nothing else."),
                        SyntheticReason::RetryFeedback,
                    )
                    .await;
            }
            Err(e) => {
                return Err(format!(
                    "output did not validate after {max_retries} retries: {e}"
                ))
            }
        }
    }
}

/// Wait for the turn to complete, returning the assistant text or an error for
/// a non-`Done` outcome. Ask events cannot occur (headless default-deny), but
/// are denied defensively.
async fn wait_for_done(handle: &mut SessionHandle) -> Result<String, String> {
    while let Some(event) = handle.events.recv().await {
        match event {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Deny { message: None });
            }
            EngineEvent::TurnDone { outcome, .. } => {
                return match outcome {
                    Outcome::Done { text } => Ok(text),
                    Outcome::Refused => Err("the model refused the request".into()),
                    Outcome::TurnLimit => Err("hit the turn limit before answering".into()),
                    other => Err(format!("the turn did not complete: {other:?}")),
                };
            }
            _ => {}
        }
    }
    Err("session ended before the turn completed".into())
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
    }

    #[test]
    fn contract_item_is_tagged() {
        let item = contract_item(&json!({"type":"object"}));
        let Item::User { text, synthetic } = item else {
            panic!()
        };
        assert_eq!(synthetic, Some(SyntheticReason::SystemReminder));
        assert!(text.contains("output-contract"));
    }

    #[tokio::test]
    async fn retries_on_invalid_then_succeeds() {
        use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
        use hotl_platform::SystemClock;
        use hotl_provider::ScriptedProvider;
        use hotl_store::{Masker, SessionLog};
        use hotl_tools::{rules::Rules, Registry};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).unwrap();
        // sample 1: invalid ("{}" — missing `name`); sample 2: valid.
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::text_reply("{}"),
            ScriptedProvider::text_reply(r#"{"name":"ok"}"#),
        ]));
        let mut handle = spawn_session(SessionDeps {
            provider: provider.clone(),
            registry: Arc::new(Registry::builtin()),
            rules: Arc::new(Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "sys".into(),
            cwd: std::env::temp_dir(),
            snapshots: None,
            hooks: None,
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            config: EngineConfig {
                max_turns: 4,
                ..Default::default()
            },
        });
        let schema = json!({"type":"object","required":["name"]});
        let out = run_structured(&mut handle, &schema, "give me a name", 2)
            .await
            .unwrap();
        assert_eq!(out["name"], "ok");
        // The retry request carried tagged feedback, not bare user text.
        let second = &provider.requests()[1];
        assert!(
            second.items.iter().any(|i| matches!(
                i,
                Item::User {
                    synthetic: Some(SyntheticReason::RetryFeedback),
                    ..
                }
            )),
            "the retry must feed back as a tagged RetryFeedback item"
        );
    }
}
