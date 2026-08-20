//! `hotl -p "…" --json-schema <file>` — structured output with validation and
//! bounded retry. The schema rides into context as a
//! tagged instruction item; the final answer is validated against it; a
//! validation error feeds back as `RetryFeedback` for up to 2 retries
//! (LangChain `ToolStrategy.handle_errors` shape). Valid JSON → stdout;
//! exhaustion → non-zero exit.

use hotl_engine::{EngineEvent, Outcome, SessionHandle};
use hotl_types::{Item, SyntheticReason, TokenUsage};
use serde_json::Value;

pub const MAX_RETRIES: u32 = 2;

/// Fence stripping, validation and the contract text live in `hotl-workflow`
/// (0044) so the workflow runner's per-agent schemas and `--json-schema` share
/// one validator; re-exported here for the existing call sites.
pub use hotl_workflow::structured::validate;

/// The schema as a tagged instruction item pushed into the session's context.
pub fn contract_item(schema: &Value) -> Item {
    Item::User {
        text: hotl_workflow::structured::contract_text(schema),
        synthetic: Some(SyntheticReason::SystemReminder),
        images: Vec::new(),
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
    handle.prompt(prompt.to_string()).await;
    structured_loop(handle, schema, max_retries, wait_for_done)
        .await
        .map(|(value, _)| value)
}

/// The validate-and-retry loop behind [`run_structured`], for a caller that
/// has already issued the first prompt and drains the session its own way
/// (`wait` answers asks, forwards events, whatever the surface needs). Usage
/// is summed across every attempt.
pub async fn structured_loop<F>(
    handle: &mut SessionHandle,
    schema: &Value,
    max_retries: u32,
    mut wait: F,
) -> Result<(Value, TokenUsage), String>
where
    F: AsyncFnMut(&mut SessionHandle) -> Result<(String, TokenUsage), String>,
{
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| format!("the --json-schema file is not a valid JSON Schema: {e}"))?;
    let mut attempts = 0;
    let mut total = TokenUsage::default();
    loop {
        let (text, usage) = wait(handle).await?;
        total += usage;
        match validate(&validator, &text) {
            Ok(value) => return Ok((value, total)),
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

/// Wait for the turn to complete, returning the assistant text and the turn's
/// usage, or an error for a non-`Done` outcome. Ask events cannot occur
/// (headless default-deny), but are denied defensively.
async fn wait_for_done(handle: &mut SessionHandle) -> Result<(String, TokenUsage), String> {
    while let Some(event) = handle.events.recv().await {
        match event {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Deny { message: None });
            }
            // Headless never installs the egress sink, so this should be
            // unreachable — answered explicitly anyway, because "unreachable
            // and therefore fine to drop" is how a fail-closed path turns into
            // a 120-second hang (0026 Step 4.5).
            EngineEvent::EgressAsk { reply, .. } => {
                let _ = reply.send(hotl_tools::net::EgressDecision::NoAnswer);
            }
            EngineEvent::TurnDone { outcome, usage } => {
                return match outcome {
                    Outcome::Done { text } => Ok((text, usage)),
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
        let Item::User {
            text, synthetic, ..
        } = item
        else {
            panic!()
        };
        assert_eq!(synthetic, Some(SyntheticReason::SystemReminder));
        assert!(text.contains("output-contract"));
    }

    /// A session whose first sample is invalid (`{}` — missing `name`) and
    /// whose second is valid. The guard is the log's directory.
    fn invalid_then_valid() -> (
        SessionHandle,
        std::sync::Arc<hotl_provider::ScriptedProvider>,
        tempfile::TempDir,
    ) {
        use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
        use hotl_platform::SystemClock;
        use hotl_provider::ScriptedProvider;
        use hotl_store::{Masker, SessionLog};
        use hotl_tools::{rules::Rules, Registry};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::text_reply("{}"),
            ScriptedProvider::text_reply(r#"{"name":"ok"}"#),
        ]));
        let handle = spawn_session(SessionDeps {
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
            initial_goal: None,
            config: EngineConfig {
                max_turns: 4,
                ..Default::default()
            },
        });
        (handle, provider, dir)
    }

    #[tokio::test]
    async fn retries_on_invalid_then_succeeds() {
        let (mut handle, provider, _dir) = invalid_then_valid();
        let schema = json!({"type":"object","required":["name"]});
        let out = run_structured(&mut handle, &schema, "give me a name", 2)
            .await
            .unwrap();
        assert_eq!(out["name"], "ok");
        // The retry request carried tagged feedback, not bare user text.
        let second = &provider.requests()[1];
        assert!(
            second.items.iter().any(|i| matches!(
                &**i,
                Item::User {
                    synthetic: Some(SyntheticReason::RetryFeedback),
                    ..
                }
            )),
            "the retry must feed back as a tagged RetryFeedback item"
        );
    }

    /// The loop itself, driven by a caller-issued prompt: usage is the sum
    /// over every attempt, not the last one's.
    #[tokio::test]
    async fn structured_loop_sums_usage_across_attempts() {
        let (mut handle, _provider, _dir) = invalid_then_valid();
        let schema = json!({"type":"object","required":["name"]});
        handle.prompt("give me a name".into()).await;
        let (out, usage) = structured_loop(&mut handle, &schema, 2, wait_for_done)
            .await
            .unwrap();
        assert_eq!(out["name"], "ok");
        // `text_reply` bills 10 in / 5 out per sample; two samples ran.
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 10);
    }
}
