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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::key::{AuthAction, AuthRetry, KeySource};
use hotl_provider::{
    ArmGuard, Provider, ProviderError, SamplingRequest, StreamEvent, ToolDef, Warmable,
};
use hotl_types::{Item, StopReason, TokenUsage};
use serde_json::{json, Value};

/// §S3.2: bounds the warm request end-to-end (connect + TLS + response),
/// independent of (and shorter than) the client's own `connect_timeout` —
/// arming is opportunistic, so a slow/unroutable origin must not hold a
/// background task open for the client's full, much more generous, bound.
const ARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

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

/// Resolve a configured base URL to the messages endpoint. Both base
/// spellings are accepted — see [`hotl_provider::v1_base`].
pub fn messages_url(base: &str) -> String {
    format!("{}/messages", hotl_provider::v1_base(base))
}

/// The one client constructor. A default `reqwest` client has no connect
/// timeout, no read timeout, and no keepalive; T1-4 traces a wedged session
/// straight back to it.
///
/// `expect` rather than a fallback to a default client on purpose: T3-12 found
/// exactly that fallback silently discarding a redirect policy and a timeout
/// elsewhere in the tree. A client that cannot be built with these options is
/// a broken TLS backend, and shipping an unbounded client instead is worse
/// than failing loudly at construction.
fn http_client(connect: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        // §S3.2: an HTTP/2 ping every 15s, even while the connection is
        // otherwise idle, keeps a pooled connection alive across a human's
        // multi-minute pause instead of it being reclaimed and re-paying
        // DNS+TCP+TLS on the next sample.
        .http2_keep_alive_interval(std::time::Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("reqwest client with timeouts (TLS backend unavailable?)")
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    key_source: Arc<dyn KeySource>,
    api_url: String,
    no_credential: bool,
    headers_timeout: std::time::Duration,
    stream_idle_timeout: std::time::Duration,
    /// §S3.2 idempotency: `0` when idle, else the generation token of the
    /// in-flight warm request — so a re-arm while already armed is a no-op
    /// instead of a duplicate handshake. A generation (not a plain bool) so
    /// a guard whose own task already finished can never clobber a *later*
    /// arm's state if it is dropped late: its cancel closure only clears
    /// the flag if it still holds *its own* token (see `spawn_arm`). Shared
    /// (not per-`ArmGuard`) because idempotency is a property of the
    /// provider's pool, not of any one caller's guard.
    armed: Arc<AtomicU64>,
}

impl AnthropicProvider {
    pub fn new(key_source: Arc<dyn KeySource>) -> Self {
        Self {
            client: http_client(hotl_provider::timeouts::CONNECT),
            key_source,
            api_url: messages_url(DEFAULT_BASE_URL),
            no_credential: false,
            headers_timeout: hotl_provider::timeouts::HEADERS,
            stream_idle_timeout: hotl_provider::timeouts::STREAM_IDLE,
            armed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Override the defaults (a slow local endpoint, or a test that wants a
    /// short bound). `connect` rebuilds the client.
    pub fn with_timeouts(
        mut self,
        connect: std::time::Duration,
        headers: std::time::Duration,
        stream_idle: std::time::Duration,
    ) -> Self {
        self.client = http_client(connect);
        self.headers_timeout = headers;
        self.stream_idle_timeout = stream_idle;
        self
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
            // `display` defaults to "omitted" on current models: without this
            // the stream still bills reasoning tokens and every
            // `thinking_delta` arrives empty (T3-15).
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        }
        body
    }
}

/// The exact JSON body this provider puts on the wire for `req`.
///
/// `#[doc(hidden)] pub` so a test can assert two `SamplingRequest`s
/// serialize *identically at the wire level*, not merely structurally —
/// commit-protocol.md case 9 requires the adopted speculative request to be
/// proven equal to the sequential rebuild "against the provider-serialized
/// body". Production code has no reason to call this; the streaming path
/// builds its own body inline.
#[doc(hidden)]
pub fn wire_body(req: &SamplingRequest) -> Value {
    AnthropicProvider::build_body(req)
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
    Retry {
        reason: String,
        wait: std::time::Duration,
    },
    Fail(ProviderError),
}

fn classify_send(err: ProviderError, attempt: u32, reason: String) -> Attempt {
    match hotl_provider::retry::classify(&err, attempt) {
        hotl_provider::retry::Decision::Retry { delay } => Attempt::Retry {
            reason,
            wait: hotl_provider::retry::with_jitter(delay),
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
        .and_then(|v| hotl_provider::retry::parse_retry_after(v, hotl_provider::retry::now_unix()));
    // Read the error class before the body consumes the response.
    let error_type = resp
        .headers()
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = resp.text().await.unwrap_or_default();
    let message = hotl_provider::api_error::detail(error_type.as_deref(), &body);
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

/// Process-wide source of `spawn_arm` generation tokens (§S3.2). Global
/// rather than per-provider: uniqueness across the process's lifetime is all
/// a token needs, and a single counter is simpler than threading a
/// per-provider one through — see the field doc on `AnthropicProvider::armed`
/// for why a token, not a plain bool, is load-bearing here.
static NEXT_ARM_TOKEN: AtomicU64 = AtomicU64::new(1);

/// §S3.2: fire one lightweight, credential-free GET at `url` to populate
/// `client`'s connection pool — the TCP+TLS handshake is the point; a 4xx
/// response is a fine outcome and every error is swallowed (failure must be
/// invisible: arming a session must never surface a warning, let alone fail
/// it, over a background optimization). `armed` makes concurrent/back-to-back
/// arms idempotent: a re-arm while a warm request is already in flight is a
/// no-op rather than a second connection. Each real (non-noop) call claims a
/// fresh generation token and only ever clears `armed` via a
/// compare-exchange against *that* token, so a guard whose task already
/// finished can't clobber a later, still-in-flight arm if it's dropped late.
fn spawn_arm(client: reqwest::Client, url: String, armed: Arc<AtomicU64>) -> ArmGuard {
    if armed.load(Ordering::Acquire) != 0 {
        return ArmGuard::noop();
    }
    let token = NEXT_ARM_TOKEN.fetch_add(1, Ordering::Relaxed);
    if armed
        .compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Lost a race to another concurrent arm() — that one owns arming now.
        return ArmGuard::noop();
    }
    let task_flag = armed.clone();
    let handle = tokio::spawn(async move {
        let _ = tokio::time::timeout(ARM_TIMEOUT, client.get(&url).send()).await;
        let _ = task_flag.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    });
    let cancel_flag = armed;
    ArmGuard::new(move || {
        handle.abort();
        // A no-op if the task already cleared it (raced us to completion)
        // or if a later arm() has already superseded this token — either
        // way, this guard only ever clears state it still owns.
        let _ = cancel_flag.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    })
}

impl Warmable for AnthropicProvider {
    fn arm(&self) -> ArmGuard {
        spawn_arm(
            self.client.clone(),
            self.api_url.clone(),
            self.armed.clone(),
        )
    }
}

impl Provider for AnthropicProvider {
    fn arm(&self) -> ArmGuard {
        <Self as Warmable>::arm(self)
    }

    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let client = self.client.clone();
        let body = Self::build_body(&req);
        let source = self.key_source.clone();
        let api_url = self.api_url.clone();
        let no_credential = self.no_credential;
        let headers_timeout = self.headers_timeout;
        let stream_idle_timeout = self.stream_idle_timeout;

        Box::pin(async_stream::stream! {
            // INVARIANT (T2-16): `attempt` counts network/HTTP attempts only.
            // An auth refresh is a separate, once-per-request budget owned by
            // `AuthRetry`, so a 401 → refresh → 429 sequence still has its
            // full retry allowance. Enforced by
            // `the_auth_refresh_does_not_consume_a_retry_slot`.
            let mut attempts_used: u32 = 0;
            let mut auth_retry = AuthRetry::default();
            let response = loop {
                let attempt = attempts_used + 1;
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
                let sent = tokio::time::timeout(
                    headers_timeout,
                    send_attempt(&client, &api_url, &key, &body, attempt),
                )
                .await;
                let outcome = match sent {
                    Ok(a) => a,
                    // A timed-out attempt is retryable, not terminal.
                    Err(_) => classify_send(
                        ProviderError::Transport(format!(
                            "no response headers within {}s",
                            headers_timeout.as_secs()
                        )),
                        attempt,
                        "response header timeout".into(),
                    ),
                };
                match outcome {
                    Attempt::Ok(resp) => break resp,
                    Attempt::Retry { reason, wait } => {
                        attempts_used = attempt;
                        yield Ok(StreamEvent::Retrying { attempt, reason });
                        tokio::time::sleep(wait).await;
                    }
                    Attempt::Fail(ProviderError::Auth(msg)) => {
                        // attempts_used deliberately unchanged.
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
            let inner = hotl_provider::drive_sse(
                response.bytes_stream(),
                sse::Assembler::default(),
                stream_idle_timeout,
            );
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
    const RETRY_429: &str = "HTTP/1.1 429 Too Many Requests\r\ncontent-type: text/plain\r\nretry-after: 0\r\ncontent-length: 5\r\nconnection: close\r\n\r\nslow!";

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

    /// INVARIANT (T1-4): the HTTP client is never the default one.
    /// `Client::new()` has no connect timeout, so a stalled TLS handshake
    /// hangs the session forever and Ctrl-C cannot reach it.
    #[test]
    fn the_client_is_built_with_timeouts() {
        let src = include_str!("lib.rs");
        // Split so this test's own source is not a match for itself.
        assert!(
            !src.contains(concat!("reqwest::Client", "::new()")),
            "use http_client(); a default reqwest client has no timeout of any kind"
        );
        assert!(src.contains("connect_timeout"));
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

    /// INVARIANT (T2-16): an auth refresh is not a retry. A 401 → refresh →
    /// 429 sequence must still have its full retry budget.
    #[tokio::test]
    async fn the_auth_refresh_does_not_consume_a_retry_slot() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        // 401 (refresh), then 429 (retry #1), then success.
        let base = tcp_double(vec![AUTH_401, RETRY_429, SSE_OK], seen.clone()).await;
        let p =
            AnthropicProvider::new(Arc::new(FlippingKey(StdMutex::new(1)))).with_base_url(&base);
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        assert_eq!(seen.lock().unwrap().len(), 3);
        // The HTTP retry reports attempt 1, not attempt 2 — the auth pass did
        // not consume the counter.
        let attempts: Vec<u32> = events
            .iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::Retrying { attempt, reason }) if reason.starts_with("HTTP") => {
                    Some(*attempt)
                }
                _ => None,
            })
            .collect();
        assert_eq!(attempts, vec![1]);
    }

    /// End-to-end retry: 429 → backoff → success, over a real socket.
    /// `retry-after: 0` keeps it instant; the policy itself is unit-tested.
    #[tokio::test]
    async fn a_429_is_retried_over_a_real_socket() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![RETRY_429, SSE_OK], seen.clone()).await;
        let p = AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(Some("k".into()))))
            .with_base_url(&base);
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        assert_eq!(seen.lock().unwrap().len(), 2, "the request was not retried");
        let retrying = events
            .iter()
            .filter(|e| matches!(e, Ok(StreamEvent::Retrying { .. })))
            .count();
        assert_eq!(retrying, 1, "the user must be told a retry happened");
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Completed { .. }))
        ));
    }

    /// §S3.2: the HTTP/2 keep-alive knobs must ride the same builder as the
    /// existing timeouts, not a separate client construction path a future
    /// edit could drift out of sync with.
    #[test]
    fn the_client_enables_http2_keep_alive() {
        let src = include_str!("lib.rs");
        assert!(src.contains("http2_keep_alive_interval"));
        assert!(src.contains("http2_keep_alive_while_idle"));
    }

    /// §S3.2 unit: dropping an armed guard resets "currently arming" state
    /// synchronously — the RAII contract, tested directly against
    /// `spawn_arm` rather than by racing a mock server's socket-close timing
    /// (hyper's own cancel-to-close latency is an implementation detail,
    /// not the contract this guard promises).
    #[tokio::test]
    async fn spawn_arm_drop_resets_the_armed_flag_immediately() {
        let armed = Arc::new(AtomicU64::new(0));
        // Never resolved before the test ends — irrelevant here, since the
        // assertion is about `drop`'s synchronous effect, not the task's.
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            "http://192.0.2.1/".into(),
            armed.clone(),
        );
        assert_ne!(
            armed.load(Ordering::Acquire),
            0,
            "arm() must mark the provider as arming"
        );
        drop(guard);
        assert_eq!(
            armed.load(Ordering::Acquire),
            0,
            "drop must reset armed state immediately — not wait for the background \
             task or its internal timeout"
        );
    }

    /// §S3.2 unit: a second `arm()` while the first is still armed is a
    /// no-op — dropping it must not disturb the still-live first guard's
    /// state (only dropping *that one* resets `armed`).
    #[tokio::test]
    async fn spawn_arm_second_call_while_armed_is_a_noop() {
        let armed = Arc::new(AtomicU64::new(0));
        let client = http_client(std::time::Duration::from_secs(1));
        let g1 = spawn_arm(client.clone(), "http://192.0.2.1/".into(), armed.clone());
        let before = armed.load(Ordering::Acquire);
        let g2 = spawn_arm(client, "http://192.0.2.1/".into(), armed.clone());
        drop(g2);
        assert_eq!(
            armed.load(Ordering::Acquire),
            before,
            "a no-op re-arm's drop must not reset state owned by the still-live first guard"
        );
        drop(g1);
        assert_eq!(armed.load(Ordering::Acquire), 0);
    }

    /// §S3.2 unit: arming against a target nothing is listening on (a
    /// connection-refused failure — the fastest-failing "unroutable" shape
    /// available offline) must not panic and must not leak its background
    /// task: `armed` comes back down to idle on its own well within the arm
    /// timeout.
    #[tokio::test]
    async fn spawn_arm_against_a_refused_target_does_not_leak_its_task() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // nothing listens on `addr` from here on: refused fast
        let armed = Arc::new(AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            format!("http://{addr}/"),
            armed.clone(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while armed.load(Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a refused warm request must not leak its background task");
        drop(guard); // already finished — a no-op abort, must not panic
    }

    /// §S3.2 unit: a target that never answers at all (no refusal, no
    /// response — the genuinely "unroutable" case, RFC 5737 TEST-NET-1) must
    /// still be bounded by the internal arm timeout rather than hanging
    /// forever.
    #[tokio::test]
    async fn spawn_arm_against_a_black_holed_target_is_bounded_by_the_arm_timeout() {
        let armed = Arc::new(AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            "http://192.0.2.1/".into(),
            armed.clone(),
        );
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a black-holed target must not hang past the internal arm timeout");
        drop(guard);
    }

    /// §S3.2 unit: a guard whose own warm request already finished (on its
    /// own, before the guard was dropped) must not clobber a *later,
    /// still-in-flight* arm when it is finally dropped late. Without this,
    /// "idempotent" would only hold while callers never hold a guard past
    /// their task's own completion — a real usage pattern this API invites
    /// (nothing stops a caller from holding a guard and re-arming later).
    #[tokio::test]
    async fn a_late_dropped_stale_guard_does_not_clobber_a_newer_arm() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // refused fast: g1's task finishes quickly, on its own
        let armed = Arc::new(AtomicU64::new(0));
        let client = http_client(std::time::Duration::from_secs(1));
        let g1 = spawn_arm(client.clone(), format!("http://{addr}/"), armed.clone());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while armed.load(Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("g1's task must finish on its own before we proceed");
        // A genuinely new, still-in-flight arm — legitimate, since g1 is done.
        let g2 = spawn_arm(client, "http://192.0.2.1/".into(), armed.clone());
        assert_ne!(armed.load(Ordering::Acquire), 0, "g2 must be in flight");
        // g1 is stale (its own task is long finished) but still held — drop
        // it now, late.
        drop(g1);
        assert_ne!(
            armed.load(Ordering::Acquire),
            0,
            "dropping the stale g1 must not clobber g2's still-in-flight state"
        );
        drop(g2);
        assert_eq!(armed.load(Ordering::Acquire), 0);
    }

    /// The public wiring: `Provider::arm` (the dyn-dispatchable entry point
    /// every call site outside this crate uses) must reach the same
    /// `Warmable` impl, not a stray default. A fast-refusing target keeps
    /// this smoke test quick.
    #[tokio::test]
    async fn provider_arm_reaches_the_warmable_impl_without_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let p = AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(None)))
            .with_base_url(&format!("http://{addr}"));
        let guard = Provider::arm(&p);
        drop(guard); // must not panic
    }

    /// A 400 is terminal — no blind retry, and the message reaches the user.
    #[tokio::test]
    async fn a_400_is_not_retried() {
        const BAD_REQUEST: &str = "HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad request";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![BAD_REQUEST], seen.clone()).await;
        let p = AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(Some("k".into()))))
            .with_base_url(&base);
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::Http { status: 400, .. }))
        ));
        assert_eq!(seen.lock().unwrap().len(), 1);
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

    /// INVARIANT (T3-15): reasoning that is billed is also *sent*. Without
    /// `display: "summarized"` the model's default is `"omitted"` and every
    /// `ThinkingDelta` carries an empty string — paid for, never visible.
    #[test]
    fn thinking_requests_a_visible_summary() {
        let mut req = sampling_req();
        req.thinking = true;
        let body = AnthropicProvider::build_body(&req);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");

        // And it is absent entirely when thinking is off.
        let mut off = sampling_req();
        off.thinking = false;
        assert!(AnthropicProvider::build_body(&off)
            .get("thinking")
            .is_none());
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
        assert_eq!(body["thinking"]["display"], "summarized");
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
