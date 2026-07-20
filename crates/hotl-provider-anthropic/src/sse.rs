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

fn index_of(v: &Value) -> usize {
    v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
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
            "content_block_start" => Ok(vec![self.on_block_start(&v)]),
            "content_block_delta" => Ok(self.on_block_delta(&v)),
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
                let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown");
                Err(ProviderError::Http {
                    status: 200,
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
    fn on_block_start(&mut self, v: &Value) -> StreamEvent {
        let index = index_of(v);
        let block = v.get("content_block").cloned().unwrap_or(Value::Null);
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        if self.blocks.len() <= index {
            self.blocks.resize(index + 1, Value::Null);
        }
        self.blocks[index] = block;
        StreamEvent::BlockStart { index, kind }
    }

    fn on_block_delta(&mut self, v: &Value) -> Vec<StreamEvent> {
        let index = index_of(v);
        let delta = v.get("delta").cloned().unwrap_or(Value::Null);
        let text_of = |field: &str| delta.get(field).and_then(Value::as_str).unwrap_or("").to_string();
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => {
                let text = text_of("text");
                self.append_str(index, "text", &text);
                vec![StreamEvent::TextDelta { index, text }]
            }
            "thinking_delta" => {
                let text = text_of("thinking");
                self.append_str(index, "thinking", &text);
                vec![StreamEvent::ThinkingDelta { index, text }]
            }
            "input_json_delta" => {
                let json = text_of("partial_json");
                self.partial_json.entry(index).or_default().push_str(&json);
                vec![StreamEvent::ToolInputDelta { index, json }]
            }
            "signature_delta" => {
                self.append_str(index, "signature", &text_of("signature"));
                vec![]
            }
            // Unknown delta kinds: skipped, not fatal (forward compat).
            _ => vec![],
        }
    }

    /// Seal accumulated tool input into the block's `input`.
    fn on_block_stop(&mut self, v: &Value) -> Result<StreamEvent, ProviderError> {
        let index = index_of(v);
        if let Some(partial) = self.partial_json.remove(&index) {
            if !partial.trim().is_empty() {
                // Arg healing (M3a): conservative repair before giving up.
                let input = hotl_provider::repair::parse_or_repair(&partial).ok_or_else(|| {
                    ProviderError::Parse(format!("tool input didn't parse at block {index}"))
                })?;
                if let Some(b) = self.blocks.get_mut(index) {
                    b["input"] = input;
                }
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
            blocks: self.blocks.iter().filter(|b| !b.is_null()).cloned().collect(),
        });
    }

    fn append_str(&mut self, index: usize, field: &str, s: &str) {
        if let Some(b) = self.blocks.get_mut(index) {
            match b.get_mut(field) {
                Some(Value::String(existing)) => existing.push_str(s),
                _ => {
                    b[field] = Value::String(s.to_string());
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_and_assembles_blocks() {
        let mut p = hotl_provider::SseParser::default();
        let mut a = Assembler::default();
        // A realistic stream, deliberately split mid-line across chunks.
        let wire = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":3}}}"#, "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#, "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#, "\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#, "\n",
            r#"data: {"type":"content_block_stop","index":0}"#, "\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}"#, "\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#, "\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#, "\n",
            r#"data: {"type":"content_block_stop","index":1}"#, "\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#, "\n",
            r#"data: {"type":"message_stop"}"#, "\n",
        );
        let bytes = wire.as_bytes();
        let mut events = Vec::new();
        // Feed in awkward 17-byte chunks to prove reassembly.
        for chunk in bytes.chunks(17) {
            for data in p.feed(chunk) {
                events.extend(a.handle(&data).unwrap());
            }
        }
        let done = a.finish().expect("completed");
        let StreamEvent::Completed { stop, usage, blocks } = done else {
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
        assert!(events.iter().any(|e| matches!(e, StreamEvent::TextDelta { .. })));
    }
}
