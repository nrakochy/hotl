//! OpenAI-compatible chat-completions provider (`POST {base}/chat/completions`).
//!
//! One crate covers every endpoint speaking this dialect — OpenAI itself,
//! Groq, Together, and local servers like Ollama (`http://localhost:11434/v1`,
//! no key needed). The base URL is configurable; auth is optional for
//! non-default bases.
//!
//! Cross-provider translation (`transform_messages`) lives
//! in this crate's converters, where the corpus says it belongs:
//! - canonical assistant blocks are Anthropic-shaped; text and tool_use map
//!   to `content` / `tool_calls`, and **foreign thinking blocks are dropped**
//!   (signed reasoning never crosses providers);
//! - tool results become one `role:"tool"` message per result;
//! - responses map back to canonical blocks (tool_calls → `tool_use` blocks),
//!   so a session can cross dialects mid-conversation in either direction.

pub mod responses;

use std::sync::Arc;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::key::{AuthAction, AuthRetry, KeySource};
use hotl_provider::{Provider, ProviderError, SamplingRequest, SseAssembler, StreamEvent, ToolDef};
use hotl_types::{Item, StopReason, TokenUsage};
use serde_json::{json, Value};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    key_source: Arc<dyn KeySource>,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: String, key_source: Arc<dyn KeySource>) -> Self {
        Self { client: reqwest::Client::new(), base_url, key_source }
    }

    fn build_body(req: &SamplingRequest) -> Value {
        let mut messages = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system.as_ref()}));
        }
        for item in req.items.iter() {
            convert_item(item, &mut messages);
        }
        if let Some(tc) = &req.turn_context {
            messages.push(json!({"role": "user", "content": tc}));
        }
        let mut body = json!({
            "model": req.model,
            "max_completion_tokens": req.max_tokens,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": messages,
        });
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(tool_json).collect::<Vec<_>>());
        }
        // `thinking` / cache_static are Anthropic-surface knobs: reasoning
        // models decide depth server-side here, and caching is implicit.
        body
    }
}

fn tool_json(t: &ToolDef) -> Value {
    json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}})
}

fn convert_item(item: &Item, out: &mut Vec<Value>) {
    match item {
        Item::System { .. } | Item::Unknown => {}
        Item::User { text, .. } => out.push(json!({"role": "user", "content": text})),
        Item::Assistant { blocks } => {
            // Named canonicalization stage: provider-bound reasoning from a
            // foreign dialect never crosses (hotl_provider::transform).
            let blocks = hotl_provider::transform::strip_foreign_reasoning(blocks);
            let text = hotl_types::assistant_text(&blocks);
            let tool_calls: Vec<Value> = hotl_types::assistant_tool_uses(&blocks)
                .into_iter()
                .map(|tu| {
                    json!({
                        "id": tu.id,
                        "type": "function",
                        "function": {
                            "name": tu.name,
                            "arguments": serde_json::to_string(&tu.input).unwrap_or_else(|_| "{}".into()),
                        }
                    })
                })
                .collect();
            // Thinking blocks are dropped here by construction: only text and
            // tool_use views are read.
            let mut msg = json!({"role": "assistant"});
            msg["content"] = if text.is_empty() { Value::Null } else { Value::String(text) };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            out.push(msg);
        }
        Item::ToolResults { results } => {
            for r in results {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": r.tool_use_id,
                    "content": r.content,
                }));
            }
        }
    }
}

fn map_finish(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

/// Folds chat-completions stream chunks into canonical blocks.
/// Text is block 0; tool calls occupy 1 + their wire index.
#[derive(Default)]
struct Assembler {
    text: String,
    text_started: bool,
    /// (id, name, accumulated argument json) per tool-call index.
    tools: Vec<(String, String, String)>,
    finish: Option<StopReason>,
    usage: TokenUsage,
    got_final: bool,
}

impl SseAssembler for Assembler {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Parse(format!("bad SSE json: {e}")))?;
        let mut out = Vec::new();
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
                self.usage.input_tokens = n;
            }
            if let Some(n) = u.get("completion_tokens").and_then(Value::as_u64) {
                self.usage.output_tokens = n;
            }
            if let Some(n) = u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64) {
                self.usage.cache_read_input_tokens = n;
            }
        }
        let Some(choice) = v.pointer("/choices/0") else {
            return Ok(out);
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    if !self.text_started {
                        self.text_started = true;
                        out.push(StreamEvent::BlockStart { index: 0, kind: "text".into() });
                    }
                    self.text.push_str(text);
                    out.push(StreamEvent::TextDelta { index: 0, text: text.to_string() });
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while self.tools.len() <= idx {
                        self.tools.push((String::new(), String::new(), String::new()));
                    }
                    let slot = &mut self.tools[idx];
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if slot.0.is_empty() {
                            slot.0 = id.to_string();
                            out.push(StreamEvent::BlockStart { index: idx + 1, kind: "tool_use".into() });
                        }
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        slot.1.push_str(name);
                    }
                    if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                        slot.2.push_str(args);
                        out.push(StreamEvent::ToolInputDelta { index: idx + 1, json: args.to_string() });
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(map_finish(reason));
            self.got_final = true;
        }
        Ok(out)
    }

    fn finish(self) -> Result<StreamEvent, ProviderError> {
        if !self.got_final {
            return Err(ProviderError::Parse("stream ended without finish_reason".into()));
        }
        let mut blocks = Vec::new();
        if !self.text.is_empty() {
            blocks.push(json!({"type": "text", "text": self.text}));
        }
        for (id, name, args) in &self.tools {
            let input: Value = if args.trim().is_empty() {
                json!({})
            } else {
                // Arg healing (M3a): conservative repair before giving up.
                hotl_provider::repair::parse_or_repair(args).ok_or_else(|| {
                    ProviderError::Parse(format!("tool arguments for `{name}` didn't parse"))
                })?
            };
            blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
        Ok(StreamEvent::Completed {
            stop: self.finish.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            blocks,
        })
    }
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
    url: &str,
    api_key: Option<&str>,
    body: &Value,
    attempt: u32,
) -> Attempt {
    let mut builder = client.post(url).header("content-type", "application/json").json(body);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    match builder.send().await {
        Ok(resp) => classify_response(resp, attempt).await,
        Err(e) => {
            let reason = e.to_string();
            classify_send(ProviderError::Transport(reason.clone()), attempt, reason)
        }
    }
}

impl Provider for OpenAiCompatProvider {
    fn stream(&self, req: SamplingRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let client = self.client.clone();
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = Self::build_body(&req);
        let source = self.key_source.clone();

        Box::pin(async_stream::stream! {
            let mut attempt: u32 = 0;
            let mut auth_retry = AuthRetry::default();
            let response = loop {
                attempt += 1;
                let key = match source.get().await {
                    Ok(k) => k,
                    Err(e) => {
                        yield Err(ProviderError::Auth(e.0));
                        return;
                    }
                };
                match send_attempt(&client, &url, key.as_deref(), &body, attempt).await {
                    Attempt::Ok(resp) => break resp,
                    Attempt::Retry { reason, wait_secs } => {
                        yield Ok(StreamEvent::Retrying { attempt, reason });
                        tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                    }
                    Attempt::Fail(ProviderError::Auth(msg)) => {
                        match auth_retry.on_auth_error(source.refreshable()) {
                            AuthAction::RefreshAndRetry => match source.refresh().await {
                                Ok(()) => {
                                    yield Ok(StreamEvent::Retrying {
                                        attempt,
                                        reason: "auth failed — re-running api_key_helper".into(),
                                    });
                                }
                                Err(ke) => {
                                    yield Err(ProviderError::Auth(format!(
                                        "{msg} (key refresh also failed: {ke})"
                                    )));
                                    return;
                                }
                            },
                            AuthAction::Surface => {
                                yield Err(ProviderError::Auth(msg));
                                return;
                            }
                        }
                    }
                    Attempt::Fail(e) => {
                        yield Err(e);
                        return;
                    }
                }
            };
            yield Ok(StreamEvent::Started);
            let inner = hotl_provider::drive_sse(response.bytes_stream(), Assembler::default());
            futures_util::pin_mut!(inner);
            while let Some(ev) = inner.next().await {
                yield ev;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;

    #[test]
    fn body_shape_drops_thinking_and_splits_tool_results() {
        let req = SamplingRequest {
            model: "gpt-test".into(),
            max_tokens: 512,
            system: "sys".into(),
            items: std::sync::Arc::new(vec![
                Item::User { text: "hi".into(), synthetic: None },
                Item::Assistant {
                    blocks: vec![
                        // A signed Claude thinking block crossing providers: must vanish.
                        json!({"type": "thinking", "thinking": "secret chain", "signature": "sig=="}),
                        json!({"type": "text", "text": "I'll check."}),
                        json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a"}}),
                    ],
                },
                Item::ToolResults {
                    results: vec![
                        ToolResultItem { tool_use_id: "toolu_1".into(), content: "out1".into(), is_error: false },
                        ToolResultItem { tool_use_id: "toolu_2".into(), content: "out2".into(), is_error: true },
                    ],
                },
            ]),
            tools: vec![ToolDef { name: "read".into(), description: "d".into(), input_schema: json!({"type":"object"}) }].into(),
            thinking: true,
            cache_static: true,
            turn_context: Some("<turn-context/>".into()),
        };
        let body = OpenAiCompatProvider::build_body(&req);
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap()["content"],
            "<turn-context/>"
        );
        let s = body.to_string();
        assert!(!s.contains("secret chain"), "foreign thinking must not cross providers");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}");
        // tool results become one role:"tool" message each
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "toolu_1");
        assert_eq!(msgs[4]["role"], "tool");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn assembles_streamed_text_and_tool_calls() {
        let chunks = [
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":5}}}"#,
        ];
        let mut a = Assembler::default();
        let mut events = Vec::new();
        for c in chunks {
            events.extend(a.handle(c).unwrap());
        }
        let StreamEvent::Completed { stop, usage, blocks } = a.finish().unwrap() else {
            panic!("wrong terminal")
        };
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_read_input_tokens, 5);
        assert_eq!(blocks[0]["text"], "Hello");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["input"]["path"], "a.rs");
        assert!(events.iter().any(|e| matches!(e, StreamEvent::TextDelta { .. })));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolInputDelta { .. })));
    }

    use std::sync::{Arc, Mutex as StdMutex};

    use futures_util::future::BoxFuture;
    use hotl_provider::key::{KeyError, KeySource};

    /// Key source yielding key-1, then key-2 after refresh.
    struct FlippingKey(StdMutex<u32>);
    impl KeySource for FlippingKey {
        fn get(&self) -> BoxFuture<'_, Result<Option<String>, KeyError>> {
            let n = *self.0.lock().unwrap();
            Box::pin(async move { Ok(Some(format!("key-{n}"))) })
        }
        fn refresh(&self) -> BoxFuture<'_, Result<(), KeyError>> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        }
        fn refreshable(&self) -> bool { true }
    }

    const SSE_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
    const AUTH_401: &str = "HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad api key";

    /// Serve `responses` to consecutive connections; record each request's
    /// `authorization` header (lowercased) into `seen`.
    async fn tcp_double(
        responses: Vec<&'static str>,
        seen: Arc<StdMutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut req = String::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    req.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if req.contains("\r\n\r\n") { break; }
                }
                let auth = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .map(|l| l.split_once(':').unwrap().1.trim().to_string())
                    .unwrap_or_default();
                seen.lock().unwrap().push(auth);
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
        });
        base
    }

    fn sampling_req() -> SamplingRequest {
        SamplingRequest {
            model: "m".into(),
            max_tokens: 16,
            system: "".into(),
            items: std::sync::Arc::new(vec![Item::User { text: "hi".into(), synthetic: None }]),
            tools: std::sync::Arc::from(Vec::<ToolDef>::new()),
            thinking: false,
            cache_static: false,
            turn_context: None,
        }
    }

    #[tokio::test]
    async fn auth_401_refreshes_key_once_and_retries() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![AUTH_401, SSE_OK], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(FlippingKey(StdMutex::new(1))));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "no error expected: {events:?}");
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer key-1", "Bearer key-2"]);
    }

    #[tokio::test]
    async fn static_source_auth_401_surfaces_immediately() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![AUTH_401], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(hotl_provider::key::StaticKey(Some("sk".into()))));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Err(ProviderError::Auth(_)))));
        assert_eq!(seen.lock().unwrap().len(), 1); // exactly one request — no blind retry
    }
}
