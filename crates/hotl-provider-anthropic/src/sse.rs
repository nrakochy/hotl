//! SSE line parsing + Messages-API stream assembly.
//!
//! `SseParser` turns raw byte chunks into `data:` payload strings (chunks can
//! split mid-line; a UTF-8 code point can split mid-chunk, so bytes are
//! buffered, not lossily decoded). `Assembler` folds the Messages-API event
//! sequence into `StreamEvent`s and the final verbatim block list.

use hotl_provider::{ProviderError, StreamEvent};
use hotl_types::{StopReason, TokenUsage};
use serde_json::Value;

#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    /// Feed a chunk; return complete `data:` payloads.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start();
                if !data.is_empty() && data != "[DONE]" {
                    out.push(data.to_string());
                }
            }
            // `event:` lines are redundant — every payload carries "type".
        }
        out
    }
}

#[derive(Default)]
pub(crate) struct Assembler {
    blocks: Vec<Value>,
    /// Accumulated partial_json per tool_use block index.
    partial_json: std::collections::HashMap<usize, String>,
    usage: TokenUsage,
    stop: Option<StopReason>,
    done: Option<StreamEvent>,
}

impl Assembler {
    pub(crate) fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Parse(format!("bad SSE json: {e}")))?;
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    crate::merge_usage(&mut self.usage, u);
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                let block_kind = block.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                if self.blocks.len() <= index {
                    self.blocks.resize(index + 1, Value::Null);
                }
                self.blocks[index] = block;
                out.push(StreamEvent::BlockStart { index, kind: block_kind });
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        self.append_str(index, "text", &text);
                        out.push(StreamEvent::TextDelta { index, text });
                    }
                    "thinking_delta" => {
                        let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("").to_string();
                        self.append_str(index, "thinking", &text);
                        out.push(StreamEvent::ThinkingDelta { index, text });
                    }
                    "input_json_delta" => {
                        let json = delta.get("partial_json").and_then(Value::as_str).unwrap_or("").to_string();
                        self.partial_json.entry(index).or_default().push_str(&json);
                        out.push(StreamEvent::ToolInputDelta { index, json });
                    }
                    "signature_delta" => {
                        let sig = delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        self.append_str(index, "signature", sig);
                    }
                    // Unknown delta kinds: skipped, not fatal (forward compat).
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                // Seal accumulated tool input into the block's `input`.
                if let Some(partial) = self.partial_json.remove(&index) {
                    if !partial.trim().is_empty() {
                        let input: Value = serde_json::from_str(&partial).map_err(|e| {
                            ProviderError::Parse(format!("tool input didn't parse at block {index}: {e}"))
                        })?;
                        if let Some(b) = self.blocks.get_mut(index) {
                            b["input"] = input;
                        }
                    }
                }
                out.push(StreamEvent::BlockEnd { index });
            }
            "message_delta" => {
                if let Some(u) = v.get("usage") {
                    crate::merge_usage(&mut self.usage, u);
                }
                if let Some(s) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(crate::stop_reason_from_wire(s));
                }
            }
            "message_stop" => {
                self.done = Some(StreamEvent::Completed {
                    stop: self.stop.unwrap_or(StopReason::EndTurn),
                    usage: self.usage,
                    blocks: self.blocks.iter().filter(|b| !b.is_null()).cloned().collect(),
                });
            }
            "error" => {
                let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown");
                return Err(ProviderError::Http { status: 200, message: format!("in-stream error: {msg}"), retry_after: None });
            }
            // ping and future event types: ignore.
            _ => {}
        }
        Ok(out)
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

    pub(crate) fn finish(&mut self) -> Option<StreamEvent> {
        self.done.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_and_assembles_blocks() {
        let mut p = SseParser::default();
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
