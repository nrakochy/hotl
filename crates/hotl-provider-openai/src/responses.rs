//! OpenAI **Responses API** stream assembler (M1 residual: the honesty test,
//! Arch #6). The Responses API is a third wire dialect on top of the same
//! `Provider`/`StreamEvent` contract the Messages and chat-completions
//! dialects use. This module proves the block-structure abstraction
//! generalizes: a different event vocabulary folds into the *same* verbatim
//! assistant blocks and `Completed` terminal event, with no engine change.
//!
//! Events handled (the subset a coding agent needs):
//! - `response.output_text.delta`            → text block deltas
//! - `response.output_item.added` (function_call) → a tool_use block opens
//! - `response.function_call_arguments.delta` → tool input accumulates
//! - `response.completed`                     → terminal usage + blocks
//!
//! The provider *selection* wiring (an HTTP client hitting `/v1/responses`)
//! is not added here — it needs a live key to be meaningful, and no key was
//! available. This assembler + its golden test are the honesty test: they
//! prove the contract holds for the dialect before a live run confirms it.

use hotl_provider::{ProviderError, SseAssembler, StreamEvent};
use hotl_types::{StopReason, TokenUsage};
use serde_json::{json, Value};

#[derive(Default)]
pub struct ResponsesAssembler {
    text: String,
    text_started: bool,
    /// (call_id, name, accumulated args) per output index.
    tools: Vec<(String, String, String)>,
    usage: TokenUsage,
    stop: Option<StopReason>,
    done: bool,
}

impl SseAssembler for ResponsesAssembler {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Parse(format!("bad Responses SSE json: {e}")))?;
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                let text = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    if !self.text_started {
                        self.text_started = true;
                        out.push(StreamEvent::BlockStart { index: 0, kind: "text".into() });
                    }
                    self.text.push_str(text);
                    out.push(StreamEvent::TextDelta { index: 0, text: text.to_string() });
                }
            }
            "response.output_item.added" => {
                if let Some(item) = v.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let id = str_at(item, "call_id");
                        let name = str_at(item, "name");
                        self.tools.push((id, name, String::new()));
                        out.push(StreamEvent::BlockStart { index: self.tools.len(), kind: "tool_use".into() });
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some((_, _, args)) = self.tools.last_mut() {
                    let delta = v.get("delta").and_then(Value::as_str).unwrap_or("");
                    args.push_str(delta);
                    out.push(StreamEvent::ToolInputDelta { index: self.tools.len(), json: delta.to_string() });
                }
            }
            "response.completed" => {
                if let Some(u) = v.pointer("/response/usage") {
                    if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
                        self.usage.input_tokens = n;
                    }
                    if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
                        self.usage.output_tokens = n;
                    }
                }
                self.stop = Some(if self.tools.is_empty() { StopReason::EndTurn } else { StopReason::ToolUse });
                self.done = true;
            }
            _ => {} // ping / other events: ignored (forward compat)
        }
        Ok(out)
    }

    fn finish(self) -> Result<StreamEvent, ProviderError> {
        if !self.done {
            return Err(ProviderError::Parse("Responses stream ended before response.completed".into()));
        }
        let mut blocks = Vec::new();
        if !self.text.is_empty() {
            blocks.push(json!({"type": "text", "text": self.text}));
        }
        for (id, name, args) in &self.tools {
            let input: Value = if args.trim().is_empty() {
                json!({})
            } else {
                hotl_provider::repair::parse_or_repair(args)
                    .ok_or_else(|| ProviderError::Parse(format!("tool args for `{name}` didn't parse")))?
            };
            blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        Ok(StreamEvent::Completed {
            stop: self.stop.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            blocks,
        })
    }
}

fn str_at(v: &Value, field: &str) -> String {
    v.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_dialect_folds_into_the_same_blocks() {
        // A Responses-API stream: text, then a function call, then completion.
        let events = [
            r#"{"type":"response.output_text.delta","delta":"I'll read "}"#,
            r#"{"type":"response.output_text.delta","delta":"the file."}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_9","name":"read"}}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"\"a.rs\"}"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":42,"output_tokens":8}}}"#,
        ];
        let mut a = ResponsesAssembler::default();
        let mut streamed = Vec::new();
        for e in events {
            streamed.extend(a.handle(e).unwrap());
        }
        let StreamEvent::Completed { stop, usage, blocks } = a.finish().unwrap() else {
            panic!("wrong terminal event")
        };
        // Same contract as every other dialect: verbatim blocks + usage + stop.
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.input_tokens, 42);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "I'll read the file.");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_9");
        assert_eq!(blocks[1]["name"], "read");
        assert_eq!(blocks[1]["input"]["path"], "a.rs");
        // Deltas surfaced for live display, just like the other dialects.
        assert!(streamed.iter().any(|e| matches!(e, StreamEvent::TextDelta { .. })));
        assert!(streamed.iter().any(|e| matches!(e, StreamEvent::ToolInputDelta { .. })));
    }

    #[test]
    fn truncated_stream_is_an_honest_error() {
        let mut a = ResponsesAssembler::default();
        a.handle(r#"{"type":"response.output_text.delta","delta":"hi"}"#).unwrap();
        assert!(a.finish().is_err(), "no response.completed → error, not a silent empty result");
    }
}
