//! SSE line parsing + Messages-API stream assembly.
//!
//! `SseParser` turns raw byte chunks into `data:` payload strings (chunks can
//! split mid-line; a UTF-8 code point can split mid-chunk, so bytes are
//! buffered, not lossily decoded). `Assembler` folds the Messages-API event
//! sequence into `StreamEvent`s and the final verbatim block list.

use hotl_provider::{ProviderError, SseAssembler, StreamEvent};
use hotl_types::{StopReason, TokenUsage};
use serde_json::Value;

#[derive(Default)]
pub(crate) struct Assembler {
    blocks: Vec<Value>,
    /// Accumulated partial_json per tool_use block index.
    partial_json: std::collections::HashMap<usize, String>,
    usage: TokenUsage,
    stop: Option<StopReason>,
    done: Option<StreamEvent>,
}

/// INVARIANT (T2-9): every wire index is bounded before it reaches a `Vec`.
/// Enforced by `an_absurd_wire_index_is_a_parse_error_not_an_allocation`.
fn index_of(v: &Value) -> Result<usize, ProviderError> {
    let raw = v.get("index").and_then(Value::as_u64).unwrap_or(0);
    let index = usize::try_from(raw).unwrap_or(usize::MAX);
    if index > hotl_provider::MAX_BLOCK_INDEX {
        return Err(ProviderError::Parse(format!(
            "block index {raw} exceeds the {} block limit",
            hotl_provider::MAX_BLOCK_INDEX
        )));
    }
    Ok(index)
}

impl SseAssembler for Assembler {
    /// Dispatch one wire event; unknown kinds (ping, future events) are ignored.
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Parse(format!("bad SSE json: {e}")))?;
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    crate::merge_usage(&mut self.usage, u);
                }
                Ok(vec![])
            }
            "content_block_start" => Ok(vec![self.on_block_start(&v)?]),
            "content_block_delta" => self.on_block_delta(&v),
            "content_block_stop" => self.on_block_stop(&v).map(|ev| vec![ev]),
            "message_delta" => {
                self.on_message_delta(&v);
                Ok(vec![])
            }
            "message_stop" => {
                self.seal();
                Ok(vec![])
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                // Map the wire error type to the status it would have carried
                // pre-stream, so retry/fallback classification treats an
                // in-stream overload exactly like an HTTP-level one (a 200
                // here would make the fallback-model chain unreachable for
                // the most common availability signal).
                let status = match v.pointer("/error/type").and_then(Value::as_str) {
                    Some("overloaded_error") => 529,
                    Some("rate_limit_error") => 429,
                    Some("api_error") => 500,
                    _ => 400,
                };
                Err(ProviderError::Http {
                    status,
                    message: format!("in-stream error: {msg}"),
                    retry_after: None,
                })
            }
            _ => Ok(vec![]),
        }
    }

    fn finish(mut self) -> Result<StreamEvent, ProviderError> {
        self.done
            .take()
            .ok_or_else(|| ProviderError::Parse("stream ended before message_stop".into()))
    }
}

impl Assembler {
    fn on_block_start(&mut self, v: &Value) -> Result<StreamEvent, ProviderError> {
        let index = index_of(v)?;
        let block = v.get("content_block").cloned().unwrap_or(Value::Null);
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if self.blocks.len() <= index {
            self.blocks.resize(index + 1, Value::Null);
        }
        self.blocks[index] = block;
        Ok(StreamEvent::BlockStart { index, kind })
    }

    fn on_block_delta(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = index_of(v)?;
        let delta = v.get("delta").cloned().unwrap_or(Value::Null);
        let text_of = |field: &str| {
            delta
                .get(field)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => {
                let text = text_of("text");
                self.append_str(index, "text", &text)?;
                Ok(vec![StreamEvent::TextDelta { index, text }])
            }
            "thinking_delta" => {
                let text = text_of("thinking");
                self.append_str(index, "thinking", &text)?;
                Ok(vec![StreamEvent::ThinkingDelta { index, text }])
            }
            "input_json_delta" => {
                // `partial_json` is a HashMap and would accept any index, so
                // placement is checked explicitly to keep the same guarantee.
                if index >= self.blocks.len() {
                    return Err(ProviderError::Parse(format!(
                        "tool input delta for block {index} arrived before its \
                         content_block_start; the stream is malformed"
                    )));
                }
                let json = text_of("partial_json");
                self.partial_json.entry(index).or_default().push_str(&json);
                Ok(vec![StreamEvent::ToolInputDelta { index, json }])
            }
            "signature_delta" => {
                self.append_str(index, "signature", &text_of("signature"))?;
                Ok(vec![])
            }
            // Unknown delta kinds: skipped, not fatal (forward compat).
            _ => Ok(vec![]),
        }
    }

    /// Seal accumulated tool input into the block's `input`.
    fn on_block_stop(&mut self, v: &Value) -> Result<StreamEvent, ProviderError> {
        let index = index_of(v)?;
        if let Some(partial) = self.partial_json.remove(&index) {
            if !partial.trim().is_empty() {
                // Arg healing (M3a): conservative repair before giving up.
                let input = hotl_provider::repair::parse_or_repair(&partial).ok_or_else(|| {
                    ProviderError::Parse(format!("tool input didn't parse at block {index}"))
                })?;
                let block = self.blocks.get_mut(index).ok_or_else(|| {
                    ProviderError::Parse(format!(
                        "tool input for block {index} arrived with no content_block_start; \
                         the stream is malformed"
                    ))
                })?;
                block["input"] = input;
            }
        }
        Ok(StreamEvent::BlockEnd { index })
    }

    fn on_message_delta(&mut self, v: &Value) {
        if let Some(u) = v.get("usage") {
            crate::merge_usage(&mut self.usage, u);
        }
        if let Some(s) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop = Some(crate::stop_reason_from_wire(s));
        }
    }

    fn seal(&mut self) {
        self.done = Some(StreamEvent::Completed {
            stop: self.stop.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            blocks: self
                .blocks
                .iter()
                .filter(|b| !b.is_null())
                .cloned()
                .collect(),
        });
    }

    /// INVARIANT (T2-11): text that reaches the UI also reaches the final
    /// block list — an unplaceable delta is an error, never a silent drop.
    /// Enforced by `a_delta_before_its_block_start_is_a_parse_error`.
    fn append_str(&mut self, index: usize, field: &str, s: &str) -> Result<(), ProviderError> {
        let block = self.blocks.get_mut(index).ok_or_else(|| {
            ProviderError::Parse(format!(
                "delta for block {index} arrived before its content_block_start; \
                 the stream is malformed"
            ))
        })?;
        match block.get_mut(field) {
            Some(Value::String(existing)) => existing.push_str(s),
            _ => {
                block[field] = Value::String(s.to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INVARIANT (T2-9): a wire-supplied block index above `MAX_BLOCK_INDEX` is a
    /// parse error, never an allocation. `"index": 4000000000` used to ask for
    /// four billion `Value`s.
    #[test]
    fn an_absurd_wire_index_is_a_parse_error_not_an_allocation() {
        let mut a = Assembler::default();
        let data = r#"{"type":"content_block_start","index":4000000000,
                       "content_block":{"type":"text","text":""}}"#;
        match a.handle(data) {
            Err(ProviderError::Parse(m)) => assert!(m.contains("index"), "{m}"),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn indices_at_the_cap_still_work() {
        let mut a = Assembler::default();
        let idx = hotl_provider::MAX_BLOCK_INDEX;
        let data = format!(
            r#"{{"type":"content_block_start","index":{idx},"content_block":{{"type":"text","text":""}}}}"#
        );
        assert!(a.handle(&data).is_ok());
    }

    /// INVARIANT (T2-11): a delta the assembler cannot place in a block is a
    /// parse error. It must never be emitted to the UI and then dropped from
    /// history — the user would read text the model never sees again.
    #[test]
    fn a_delta_before_its_block_start_is_a_parse_error() {
        let mut a = Assembler::default();
        let data = r#"{"type":"content_block_delta","index":0,
                       "delta":{"type":"text_delta","text":"orphan"}}"#;
        match a.handle(data) {
            Err(ProviderError::Parse(m)) => assert!(m.contains("block 0"), "{m}"),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    /// Same for an index inside the cap but past the blocks actually opened.
    #[test]
    fn an_out_of_range_delta_is_a_parse_error() {
        let mut a = Assembler::default();
        a.handle(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        )
        .unwrap();
        let data = r#"{"type":"content_block_delta","index":5,
                       "delta":{"type":"text_delta","text":"x"}}"#;
        assert!(matches!(a.handle(data), Err(ProviderError::Parse(_))));
    }

    /// The expensive variant: repair succeeds, the block is missing, and today
    /// `BlockEnd` returns Ok — so the tool call ships with `input: {}`.
    #[test]
    fn healed_tool_input_with_no_block_is_a_parse_error_not_an_empty_call() {
        let mut a = Assembler::default();
        // No content_block_start for index 3; only a partial_json delta would
        // normally precede the stop, so drive the partial buffer directly.
        let delta = r#"{"type":"content_block_delta","index":3,
                        "delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#;
        // The delta itself is already unplaceable, and that is the first error.
        assert!(matches!(a.handle(delta), Err(ProviderError::Parse(_))));
    }

    /// A block that *was* opened still seals normally — the loud path must not
    /// break the happy path.
    #[test]
    fn an_opened_tool_block_still_seals_its_healed_input() {
        let mut a = Assembler::default();
        a.handle(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"read","input":{}}}"#).unwrap();
        // Cut mid-string, the shape `repair::parse_or_repair` heals (a comma
        // left dangling at the cut point is a separate, still-unhealed case —
        // see this plan's decision log).
        a.handle(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"a.rs"}}"#).unwrap();
        a.handle(r#"{"type":"content_block_stop","index":0}"#)
            .unwrap();
        a.handle(r#"{"type":"message_stop"}"#).unwrap();
        let StreamEvent::Completed { blocks, .. } = a.finish().unwrap() else {
            panic!("wrong terminal event")
        };
        assert_eq!(blocks[0]["input"]["path"], "a.rs", "repair must still land");
    }

    #[test]
    fn in_stream_errors_carry_availability_statuses() {
        let cases = [
            ("overloaded_error", 529),
            ("rate_limit_error", 429),
            ("api_error", 500),
            ("invalid_request_error", 400),
        ];
        for (etype, want) in cases {
            let mut a = Assembler::default();
            let data =
                format!(r#"{{"type":"error","error":{{"type":"{etype}","message":"boom"}}}}"#);
            let Err(ProviderError::Http { status, .. }) = a.handle(&data) else {
                panic!("expected an Http error for {etype}");
            };
            assert_eq!(status, want, "{etype}");
        }
        // The overload must classify as availability so fallback models fire.
        let mut a = Assembler::default();
        let err = a
            .handle(r#"{"type":"error","error":{"type":"overloaded_error","message":"o"}}"#)
            .unwrap_err();
        assert!(hotl_provider::retry::is_availability(&err));
    }

    #[test]
    fn parses_split_chunks_and_assembles_blocks() {
        let mut p = hotl_provider::SseParser::default();
        let mut a = Assembler::default();
        // A realistic stream, deliberately split mid-line across chunks.
        let wire = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":3}}}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
            "\n\n",
            // Deliberately no trailing newline: the terminal event must
            // survive the flush path (it was silently dropped before R3).
            r#"data: {"type":"message_stop"}"#,
        );
        let bytes = wire.as_bytes();
        let mut events = Vec::new();
        // Feed in awkward 17-byte chunks to prove reassembly.
        for chunk in bytes.chunks(17) {
            for data in p.feed(chunk).unwrap() {
                events.extend(a.handle(&data).unwrap());
            }
        }
        for data in p.finish().unwrap() {
            events.extend(a.handle(&data).unwrap());
        }
        let done = a.finish().expect("completed");
        let StreamEvent::Completed {
            stop,
            usage,
            blocks,
        } = done
        else {
            panic!("wrong terminal event")
        };
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "Hello");
        assert_eq!(blocks[1]["input"]["path"], "a.rs");
        // Delta events surfaced for display.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. })));
    }

    #[test]
    fn message_start_usage_carries_both_cache_fields() {
        let mut a = Assembler::default();
        a.handle(
            r#"{"type":"message_start","message":{"usage":{
                "input_tokens":20,"cache_read_input_tokens":300,
                "cache_creation_input_tokens":150
            }}}"#,
        )
        .unwrap();
        a.handle(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#)
            .unwrap();
        a.handle(r#"{"type":"message_stop"}"#).unwrap();
        let StreamEvent::Completed { usage, .. } = a.finish().expect("completed") else {
            panic!("wrong terminal event")
        };
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_creation_input_tokens, 150);
        assert_eq!(usage.output_tokens, 9);
    }
}
