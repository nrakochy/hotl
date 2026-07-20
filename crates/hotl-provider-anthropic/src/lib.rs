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
        let mut messages = build_messages(&req.items, req.cache_static);
        // MOIM rides after the cache marker: it changes every sample without
        // invalidating the cached prefix (suffix position).
        if let Some(tc) = &req.turn_context {
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": tc}]}));
        }
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "messages": messages,
        });
        if !req.system.is_empty() {
            let mut sys = json!({"type": "text", "text": req.system});
            if req.cache_static {
                sys["cache_control"] = json!({"type": "ephemeral"});
            }
            body["system"] = json!([sys]);
        }
        if !req.tools.is_empty() {
            let mut tools: Vec<Value> = req.tools.iter().map(tool_json).collect();
            // Auto breakpoint on the last tool def (M2 cache policy): tools
            // render before system in the prefix, so this seals the whole
            // tool block. 3 markers total (tools/system/latest-user) ≤ 4.
            if req.cache_static {
                if let Some(last) = tools.last_mut() {
                    last["cache_control"] = json!({"type": "ephemeral"});
                }
            }
            body["tools"] = json!(tools);
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


/// One send attempt, classified. Keeps the stream generator small while
/// letting it yield `Retrying` events live (during the backoff, not after).
enum Attempt {
    Ok(reqwest::Response),
    Retry { reason: String, wait_secs: u64 },
    Fail(ProviderError),
}

fn classify_send(err: ProviderError, attempt: u32, reason: String) -> Attempt {
    match hotl_provider::retry::classify(&err, attempt) {
        hotl_provider::retry::Decision::Retry { after_secs } => Attempt::Retry { reason, wait_secs: after_secs },
        hotl_provider::retry::Decision::Fatal => Attempt::Fail(err),
    }
}

async fn classify_response(resp: reqwest::Response, attempt: u32) -> Attempt {
    if resp.status().is_success() {
        return Attempt::Ok(resp);
    }
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let message = resp.text().await.unwrap_or_default();
    if status == 401 || status == 403 {
        return Attempt::Fail(ProviderError::Auth(message));
    }
    let err = ProviderError::Http { status, message, retry_after };
    classify_send(err, attempt, format!("HTTP {status}"))
}

async fn send_attempt(
    client: &reqwest::Client,
    api_key: &str,
    body: &Value,
    attempt: u32,
) -> Attempt {
    let sent = client
        .post(API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", API_VERSION)
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await;
    match sent {
        Ok(resp) => classify_response(resp, attempt).await,
        Err(e) => {
            let reason = e.to_string();
            classify_send(ProviderError::Transport(reason.clone()), attempt, reason)
        }
    }
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
                match send_attempt(&client, &api_key, &body, attempt).await {
                    Attempt::Ok(resp) => break resp,
                    Attempt::Retry { reason, wait_secs } => {
                        yield Ok(StreamEvent::Retrying { attempt, reason });
                        tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                    }
                    Attempt::Fail(e) => {
                        yield Err(e);
                        return;
                    }
                }
            };
            yield Ok(StreamEvent::Started);
            let inner = hotl_provider::drive_sse(response.bytes_stream(), sse::Assembler::default());
            futures_util::pin_mut!(inner);
            while let Some(ev) = inner.next().await {
                yield ev;
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
            tools: vec![ToolDef {
                name: "read".into(),
                description: "d".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            thinking: true,
            cache_static: true,
            turn_context: Some("<turn-context sample=\"1\"/>".into()),
        };
        let body = AnthropicProvider::build_body(&req);
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        // system block carries a cache marker; so does the last tool def
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        let msgs = body["messages"].as_array().unwrap();
        // 3 items + the ephemeral MOIM block at the end
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3]["content"][0]["text"], "<turn-context sample=\"1\"/>");
        assert!(msgs[3]["content"][0].get("cache_control").is_none(), "MOIM is never cached");
        // last *item* user-role message (the tool results) carries the marker —
        // the MOIM block after it doesn't shift the cache point
        let last = &msgs[2];
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["cache_control"]["type"], "ephemeral");
        // the earlier user message does NOT carry a marker
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
    }
}
