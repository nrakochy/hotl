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

use std::sync::Arc;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::key::{AuthAction, AuthRetry, KeySource};
use hotl_provider::{Provider, ProviderError, SamplingRequest, StreamEvent, ToolDef};
use hotl_types::{Item, StopReason, TokenUsage};
use serde_json::{json, Value};

pub const DEFAULT_MODEL: &str = "claude-opus-4-8";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const API_VERSION: &str = "2023-06-01";

/// Sent as `x-api-key` in subscription mode. Not a credential and not secret —
/// it exists only because some bridges validate that the header is *present*.
///
/// Still open (no bridge was reachable when this landed): if endpoints accept
/// the request with no `x-api-key` header at all, drop this and omit the
/// header — hotl would then transmit nothing resembling a credential, which is
/// strictly better. Nothing else in the design depends on the outcome.
const SUBSCRIPTION_PLACEHOLDER: &str = "hotl";

/// Resolve a configured base URL to the messages endpoint.
///
/// Two spellings are accepted on purpose. hotl's own convention (and the
/// OpenAI provider's) puts the version in the base — `.../v1`. Local bridges
/// document the bare origin instead, because official SDKs append the whole
/// `/v1/messages` path themselves, and users copy that. Guessing wrong is a
/// confusing 404, so both work.
pub fn messages_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    key_source: Arc<dyn KeySource>,
    api_url: String,
    no_credential: bool,
}

impl AnthropicProvider {
    pub fn new(key_source: Arc<dyn KeySource>) -> Self {
        Self {
            client: reqwest::Client::new(),
            key_source,
            api_url: messages_url(DEFAULT_BASE_URL),
            no_credential: false,
        }
    }

    /// Point at an Anthropic-shaped endpoint that is not Anthropic.
    pub fn with_base_url(mut self, base: &str) -> Self {
        self.api_url = messages_url(base);
        self
    }

    /// Subscription mode: hotl holds no credential; the endpoint
    /// authenticates upstream on its own.
    ///
    /// The `key_source` is never consulted once this is set. That is the
    /// point, not an optimization: a user switching to a local bridge will
    /// often still have `ANTHROPIC_API_KEY` exported, and forwarding it would
    /// hand a production credential to a proxy they never meant to trust with
    /// one. Suppression lives here as well as in provider selection so no
    /// wiring mistake upstream can leak the key.
    pub fn subscription(mut self) -> Self {
        self.no_credential = true;
        self
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
            let mut sys = json!({"type": "text", "text": req.system.as_ref()});
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
        hotl_provider::retry::Decision::Retry { after_secs } => Attempt::Retry {
            reason,
            wait_secs: after_secs,
        },
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
    let err = ProviderError::Http {
        status,
        message,
        retry_after,
    };
    classify_send(err, attempt, format!("HTTP {status}"))
}

async fn send_attempt(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &Value,
    attempt: u32,
) -> Attempt {
    let sent = client
        .post(url)
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

/// Handles one `Attempt::Fail(Auth)` outcome: refresh-and-retry once per
/// request (via `auth_retry`), or surface. `Ok(reason)` means the key was
/// refreshed — yield a `Retrying` event with `reason` and loop again.
/// `Err` means surface the auth error and stop.
async fn handle_auth_fail(
    source: &Arc<dyn KeySource>,
    auth_retry: &mut AuthRetry,
    msg: String,
) -> Result<String, ProviderError> {
    match auth_retry.on_auth_error(source.refreshable()) {
        AuthAction::RefreshAndRetry => match source.refresh().await {
            Ok(()) => Ok("auth failed — re-running api_key_helper".into()),
            Err(ke) => Err(ProviderError::Auth(format!(
                "{msg} (key refresh also failed: {ke})"
            ))),
        },
        AuthAction::Surface => Err(ProviderError::Auth(msg)),
    }
}

impl Provider for AnthropicProvider {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let client = self.client.clone();
        let body = Self::build_body(&req);
        let source = self.key_source.clone();
        let api_url = self.api_url.clone();
        let no_credential = self.no_credential;

        Box::pin(async_stream::stream! {
            let mut attempt: u32 = 0;
            let mut auth_retry = AuthRetry::default();
            let response = loop {
                attempt += 1;
                // Subscription mode short-circuits before `source.get()` — the
                // key source is never consulted, so an environment key cannot
                // reach a bridge even if one is configured.
                let key = if no_credential {
                    SUBSCRIPTION_PLACEHOLDER.to_string()
                } else {
                    match source.get().await {
                        Ok(Some(k)) => k,
                        Ok(None) => {
                            yield Err(ProviderError::Auth(
                                "no Anthropic key: set ANTHROPIC_API_KEY, configure [provider] api_key_helper, \
                                 or point [provider] base_url at an endpoint that authenticates for you and set \
                                 auth = \"subscription\"".into(),
                            ));
                            return;
                        }
                        Err(e) => {
                            yield Err(ProviderError::Auth(e.0));
                            return;
                        }
                    }
                };
                match send_attempt(&client, &api_url, &key, &body, attempt).await {
                    Attempt::Ok(resp) => break resp,
                    Attempt::Retry { reason, wait_secs } => {
                        yield Ok(StreamEvent::Retrying { attempt, reason });
                        tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                    }
                    Attempt::Fail(ProviderError::Auth(msg)) => {
                        match handle_auth_fail(&source, &mut auth_retry, msg).await {
                            Ok(reason) => yield Ok(StreamEvent::Retrying { attempt, reason }),
                            Err(e) => {
                                yield Err(e);
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
        fn refreshable(&self) -> bool {
            true
        }
    }

    const SSE_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    const AUTH_401: &str = "HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad api key";

    /// Serve `responses` to consecutive connections; record each request's
    /// `x-api-key` header (lowercased) into `seen`.
    async fn tcp_double(responses: Vec<&'static str>, seen: Arc<StdMutex<Vec<String>>>) -> String {
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
                    if req.contains("\r\n\r\n") {
                        break;
                    }
                }
                let auth = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("x-api-key:"))
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
            items: std::sync::Arc::new(vec![Item::User {
                text: "hi".into(),
                synthetic: None,
            }]),
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
        let p =
            AnthropicProvider::new(Arc::new(FlippingKey(StdMutex::new(1)))).with_base_url(&base);
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        assert_eq!(*seen.lock().unwrap(), vec!["key-1", "key-2"]);
    }

    #[test]
    fn base_url_accepts_bare_origin_and_v1_suffix() {
        // Bridges document the bare origin; hotl's own convention includes /v1.
        assert_eq!(
            messages_url("http://127.0.0.1:3456"),
            "http://127.0.0.1:3456/v1/messages"
        );
        assert_eq!(
            messages_url("http://127.0.0.1:3456/v1"),
            "http://127.0.0.1:3456/v1/messages"
        );
        // Trailing slashes are noise in either spelling.
        assert_eq!(
            messages_url("http://127.0.0.1:3456/"),
            "http://127.0.0.1:3456/v1/messages"
        );
        assert_eq!(
            messages_url("http://127.0.0.1:3456/v1/"),
            "http://127.0.0.1:3456/v1/messages"
        );
        assert_eq!(
            messages_url(DEFAULT_BASE_URL),
            "https://api.anthropic.com/v1/messages"
        );
    }

    /// The security guard. Do not delete: a user switching to a local bridge
    /// will often still have ANTHROPIC_API_KEY exported, and forwarding it
    /// hands a production credential to a proxy they never meant to trust.
    #[tokio::test]
    async fn subscription_mode_never_sends_a_real_key() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![SSE_OK], seen.clone()).await;
        let p = AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(Some(
            "sk-ant-real-secret".into(),
        ))))
        .with_base_url(&base)
        .subscription();
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        let sent = seen.lock().unwrap()[0].clone();
        assert!(
            !sent.contains("sk-ant") && !sent.contains("real-secret"),
            "subscription mode leaked the environment key: {sent}"
        );
    }

    /// Subscription mode must work with no credential available at all.
    #[tokio::test]
    async fn subscription_mode_succeeds_with_a_keyless_source() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![SSE_OK], seen.clone()).await;
        let p = AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(None)))
            .with_base_url(&base)
            .subscription();
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
    }

    #[tokio::test]
    async fn keyless_source_is_an_auth_error_with_instruction() {
        let p = AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        match events.last() {
            Some(Err(ProviderError::Auth(m))) => assert!(m.contains("ANTHROPIC_API_KEY"), "{m}"),
            other => panic!("expected Auth error, got {other:?}"),
        }
    }

    #[test]
    fn body_shape_and_cache_placement() {
        let req = SamplingRequest {
            model: DEFAULT_MODEL.into(),
            max_tokens: 1024,
            system: "sys".into(),
            items: std::sync::Arc::new(vec![
                Item::User {
                    text: "instructions".into(),
                    synthetic: None,
                },
                Item::Assistant {
                    blocks: vec![serde_json::json!({"type":"text","text":"ok"})],
                },
                Item::ToolResults {
                    results: vec![ToolResultItem {
                        tool_use_id: "t1".into(),
                        content: "out".into(),
                        is_error: false,
                    }],
                },
            ]),
            tools: vec![ToolDef {
                name: "read".into(),
                description: "d".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }]
            .into(),
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
        assert_eq!(
            msgs[3]["content"][0]["text"],
            "<turn-context sample=\"1\"/>"
        );
        assert!(
            msgs[3]["content"][0].get("cache_control").is_none(),
            "MOIM is never cached"
        );
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
