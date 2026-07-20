//! Anthropic Messages API provider (`POST /v1/messages`, SSE streaming).
//!
//! Wire shapes per the Claude API docs: `message_start` → `content_block_start`
//! → `content_block_delta`* → `content_block_stop` → `message_delta` →
//! `message_stop`. Blocks are assembled verbatim from the wire (including
//! thinking signatures) so the next request can echo them byte-faithfully.
//!
//! M0 retry policy: 2 retries on 429/5xx/transport *before first event*,
//! honoring `retry-after` (a stream that dies mid-flight is surfaced, not
//! retried — replaying half a stream is M1 recovery work).

mod sse;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::{Provider, ProviderError, SamplingRequest, StreamEvent, ToolDef};
use hotl_types::{Item, StopReason, TokenUsage};
use serde_json::{json, Value};

pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self { client: reqwest::Client::new(), api_key }
    }

    fn build_body(req: &SamplingRequest) -> Value {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "messages": build_messages(&req.items, req.cache_static),
        });
        if !req.system.is_empty() {
            let mut sys = json!({"type": "text", "text": req.system});
            if req.cache_static {
                sys["cache_control"] = json!({"type": "ephemeral"});
            }
            body["system"] = json!([sys]);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(tool_json).collect::<Vec<_>>());
        }
        if req.thinking {
            body["thinking"] = json!({"type": "adaptive"});
        }
        body
    }
}

fn tool_json(t: &ToolDef) -> Value {
    json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})
}

fn build_messages(items: &[Item], cache_static: bool) -> Vec<Value> {
    let last_user_idx = items
        .iter()
        .rposition(|i| matches!(i, Item::User { .. } | Item::ToolResults { .. }));
    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        let mark = cache_static && Some(idx) == last_user_idx;
        match item {
            // System items never reach the wire from here — the system prompt
            // travels in the request's `system` field (context assembly owns it).
            Item::System { .. } | Item::Unknown => continue,
            Item::User { text, .. } => {
                let mut block = json!({"type": "text", "text": text});
                if mark {
                    block["cache_control"] = json!({"type": "ephemeral"});
                }
                out.push(json!({"role": "user", "content": [block]}));
            }
            Item::Assistant { blocks } => {
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Item::ToolResults { results } => {
                let mut content: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        let mut v = json!({
                            "type": "tool_result",
                            "tool_use_id": r.tool_use_id,
                            "content": r.content,
                        });
                        if r.is_error {
                            v["is_error"] = json!(true);
                        }
                        v
                    })
                    .collect();
                if mark {
                    if let Some(last) = content.last_mut() {
                        last["cache_control"] = json!({"type": "ephemeral"});
                    }
                }
                out.push(json!({"role": "user", "content": content}));
            }
        }
    }
    out
}

impl Provider for AnthropicProvider {
    fn stream(&self, req: SamplingRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let body = Self::build_body(&req);

        Box::pin(async_stream::stream! {
            let mut attempt: u32 = 0;
            let response = loop {
                attempt += 1;
                let sent = client
                    .post(API_URL)
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", API_VERSION)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await;
                match sent {
                    Ok(resp) if resp.status().is_success() => break resp,
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let retry_after = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok());
                        let message = resp.text().await.unwrap_or_default();
                        if status == 401 || status == 403 {
                            yield Err(ProviderError::Auth(message));
                            return;
                        }
                        let err = ProviderError::Http { status, message, retry_after };
                        match hotl_provider::retry::classify(&err, attempt) {
                            hotl_provider::retry::Decision::Retry { after_secs } => {
                                yield Ok(StreamEvent::Retrying { attempt, reason: format!("HTTP {status}") });
                                tokio::time::sleep(std::time::Duration::from_secs(after_secs)).await;
                                continue;
                            }
                            hotl_provider::retry::Decision::Fatal => {
                                yield Err(err);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let err = ProviderError::Transport(e.to_string());
                        match hotl_provider::retry::classify(&err, attempt) {
                            hotl_provider::retry::Decision::Retry { after_secs } => {
                                yield Ok(StreamEvent::Retrying { attempt, reason: e.to_string() });
                                tokio::time::sleep(std::time::Duration::from_secs(after_secs)).await;
                                continue;
                            }
                            hotl_provider::retry::Decision::Fatal => {
                                yield Err(err);
                                return;
                            }
                        }
                    }
                }
            };

            yield Ok(StreamEvent::Started);

            let mut assembler = sse::Assembler::default();
            let mut parser = sse::SseParser::default();
            let mut bytes = response.bytes_stream();
            while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::Transport(format!("stream interrupted: {e}")));
                        return;
                    }
                };
                for data in parser.feed(&chunk) {
                    match assembler.handle(&data) {
                        Ok(events) => {
                            for ev in events {
                                yield Ok(ev);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    }
                }
            }
            if let Some(done) = assembler.finish() {
                yield Ok(done);
            } else {
                yield Err(ProviderError::Parse("stream ended before message_stop".into()));
            }
        })
    }
}

/// Re-exported for the honesty test and the CLI.
pub fn stop_reason_from_wire(s: &str) -> StopReason {
    serde_json::from_value(Value::String(s.to_string())).unwrap_or(StopReason::Other)
}

/// Merge usage fields that may arrive on message_start and message_delta.
pub(crate) fn merge_usage(into: &mut TokenUsage, v: &Value) {
    if let Some(n) = v.get("input_tokens").and_then(Value::as_u64) {
        into.input_tokens = n;
    }
    if let Some(n) = v.get("output_tokens").and_then(Value::as_u64) {
        into.output_tokens = n;
    }
    if let Some(n) = v.get("cache_read_input_tokens").and_then(Value::as_u64) {
        into.cache_read_input_tokens = n;
    }
    if let Some(n) = v.get("cache_creation_input_tokens").and_then(Value::as_u64) {
        into.cache_creation_input_tokens = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;

    #[test]
    fn body_shape_and_cache_placement() {
        let req = SamplingRequest {
            model: DEFAULT_MODEL.into(),
            max_tokens: 1024,
            system: "sys".into(),
            items: vec![
                Item::User { text: "instructions".into(), synthetic: None },
                Item::Assistant { blocks: vec![serde_json::json!({"type":"text","text":"ok"})] },
                Item::ToolResults { results: vec![ToolResultItem { tool_use_id: "t1".into(), content: "out".into(), is_error: false }] },
            ],
            tools: vec![],
            thinking: true,
            cache_static: true,
        };
        let body = AnthropicProvider::build_body(&req);
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        // system block carries the cache marker
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        // last user-role message (the tool results) carries the second marker
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        let last = &msgs[2];
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["cache_control"]["type"], "ephemeral");
        // the earlier user message does NOT carry a marker
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
    }
}
