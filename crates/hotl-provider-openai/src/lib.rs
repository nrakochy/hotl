//! OpenAI-compatible chat-completions provider (`POST {base}/chat/completions`).
//!
//! One crate covers every endpoint speaking this dialect — OpenAI itself,
//! Groq, Together, and local servers like Ollama (`http://localhost:11434/v1`,
//! no key needed). The base URL is configurable; auth is optional for
//! non-default bases.
//!
//! **Chat Completions only.** The Responses API is a separate dialect served
//! by the sibling crate `hotl-provider-openai-responses` (select it with
//! `HOTL_MODEL=openai-responses/<model>`); it is deliberately not implemented
//! here (R3 deleted an assembler with no HTTP client behind it — see
//! `specs/exec-plans/tech-debt-tracker.md`).
//!
//! Cross-provider translation (`transform_messages`) lives
//! in this crate's converters, where the corpus says it belongs:
//! - canonical assistant blocks are Anthropic-shaped; text and tool_use map
//!   to `content` / `tool_calls`, and **foreign thinking blocks are dropped**
//!   (signed reasoning never crosses providers);
//! - tool results become one `role:"tool"` message per result;
//! - responses map back to canonical blocks (tool_calls → `tool_use` blocks),
//!   so a session can cross dialects mid-conversation in either direction.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::key::{AuthAction, AuthRetry, KeySource};
use hotl_provider::{
    ArmGuard, Effort, EffortLadder, Provider, ProviderError, SamplingRequest, SseAssembler,
    StreamEvent, ToolDef, Warmable,
};
use hotl_types::{Item, StopReason, TokenUsage};
use serde_json::{json, Value};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// §S3.2: bounds the warm request end-to-end, independent of (and shorter
/// than) the client's own `connect_timeout` — see the twin constant in
/// `hotl-provider-anthropic` for the full rationale.
const ARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Marks a failed tool result in the chat-completions dialect.
///
/// Anthropic carries this structurally (`"is_error": true`); this wire has no
/// equivalent field on a `role: "tool"` message, so the signal is in-band or
/// it is lost. Without it the model cannot tell a failure from a success and
/// the errors-are-prompts loop silently stops working (T3-13). Do not remove
/// as cosmetic.
const TOOL_ERROR_PREFIX: &str = "[tool_error] ";

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

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    key_source: Arc<dyn KeySource>,
    headers_timeout: std::time::Duration,
    stream_idle_timeout: std::time::Duration,
    legacy_max_tokens: bool,
    /// §S3.2 idempotency: `0` when idle, else the generation token of the
    /// in-flight warm request — see the twin field in
    /// `hotl-provider-anthropic` for the full rationale.
    armed: Arc<AtomicU64>,
}

impl OpenAiCompatProvider {
    pub fn new(base_url: String, key_source: Arc<dyn KeySource>) -> Self {
        Self {
            client: http_client(hotl_provider::timeouts::CONNECT),
            base_url,
            key_source,
            headers_timeout: hotl_provider::timeouts::HEADERS,
            stream_idle_timeout: hotl_provider::timeouts::STREAM_IDLE,
            legacy_max_tokens: false,
            armed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The chat-completions endpoint for this provider's base URL. Shared by
    /// `stream()` and the §S3.2 warm request so the two can never disagree
    /// about where the connection pool's origin is.
    fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
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

    /// Send `max_tokens` instead of `max_completion_tokens` from the first
    /// request, for an endpoint already known to be old.
    pub fn legacy_max_tokens(mut self) -> Self {
        self.legacy_max_tokens = true;
        self
    }

    /// The default spelling. `stream()` calls [`Self::body_for`] directly
    /// because it may re-render the body mid-loop after the legacy probe.
    #[cfg(test)]
    fn build_body(req: &SamplingRequest) -> Value {
        Self::body_for(req, false)
    }

    /// `legacy` sends the pre-2024 `max_tokens` spelling. Older
    /// OpenAI-compatible servers (llama.cpp, older vLLM/Ollama) reject
    /// `max_completion_tokens` with a 400, and this crate advertises them.
    fn body_for(req: &SamplingRequest, legacy: bool) -> Value {
        // Derived once per request from static catalog data. The compat
        // family is uncatalogued, so this fails open (images always sent) —
        // a non-vision server answers with its own 400, surfaced honestly.
        let send_images = hotl_provider::catalog::supports_images(&req.model);
        let mut messages = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system.as_ref()}));
        }
        for item in req.items.iter() {
            convert_item(item, &mut messages, send_images);
        }
        // The ephemeral suffix keeps its wire position in this dialect too:
        // after every durable message, before MOIM. There is no marker to be
        // after here — caching is implicit — but the ordering is what the
        // engine's byte-identity claim is made over, so it is not this crate's
        // to reorder.
        for item in req.ephemeral_tail.iter() {
            convert_item(item, &mut messages, send_images);
        }
        if let Some(tc) = &req.turn_context {
            messages.push(json!({"role": "user", "content": tc}));
        }
        let mut body = json!({
            "model": req.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": messages,
        });
        if legacy {
            body["max_tokens"] = json!(req.max_tokens);
        } else {
            body["max_completion_tokens"] = json!(req.max_tokens);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(tool_json).collect::<Vec<_>>());
        }
        // `cache` stays an Anthropic-surface knob: caching is implicit here and
        // this dialect emits no breakpoints under ANY `CachePolicy`. Depth does
        // not — Chat Completions carries a flattened top-level
        // `reasoning_effort`, and `thinking: false` has an exact spelling here
        // (`none`) that the Anthropic dialect has no equivalent for.
        //
        // Only spoken when the session opted into the ladder: an unconfigured
        // request keeps its old bytes even with `thinking: false`, because
        // OpenAI-compatible servers vary in how they greet an unknown key.
        if req.effort.is_some() {
            // `thinking: false` wins: it is the explicit off-switch, and `none`
            // is the rung the neutral ladder deliberately does not carry.
            let level = if !req.thinking {
                Some("none")
            } else {
                req.effort
                    .and_then(|e| OpenAiLadder.resolve(&req.model, e))
                    .map(Effort::as_str)
            };
            if let Some(level) = level {
                body["reasoning_effort"] = json!(level);
            }
        }
        body
    }
}

/// Real OpenAI-compatible servers accept `minimal|low|medium|high` for
/// `reasoning_effort` — no `xhigh`/`max`. Declaring only the shared rungs
/// lets the trait's default `resolve` clamp `XHigh`/`Max` to `High`.
/// (hotl's ladder has no `minimal` rung; `low` is the floor.)
pub struct OpenAiLadder;

impl EffortLadder for OpenAiLadder {
    fn rungs(&self, _model: &str) -> &'static [Effort] {
        &[Effort::Low, Effort::Medium, Effort::High]
    }
}

/// A 400/422 naming `max_completion_tokens` — the one wire error that means
/// "this server predates the parameter", not "your request is wrong".
/// Deliberately narrow: it matches on *structured API error text*, which is
/// the carve-out RELIABILITY.md allows (the rule is about model prose).
fn rejects_max_completion_tokens(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::Http { status, message, .. }
            if (*status == 400 || *status == 422) && message.contains("max_completion_tokens")
    )
}

/// A 400 whose structured message names `reasoning_effort` and
/// `/v1/responses`: OpenAI's reasoning models refusing effort next to
/// function tools on this endpoint. Like [`rejects_max_completion_tokens`],
/// deliberately narrow and matched on *structured API error text* (the
/// RELIABILITY.md carve-out). Unlike it, the fix is a different dialect, not
/// a different spelling — so the caller appends a pointer instead of
/// retrying or degrading.
fn needs_responses_dialect(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::Http { status: 400, message, .. }
            if message.contains("reasoning_effort") && message.contains("/v1/responses")
    )
}

/// Append the Responses-dialect pointer to the one error it explains; every
/// other error passes through untouched.
fn with_responses_hint(err: ProviderError) -> ProviderError {
    if !needs_responses_dialect(&err) {
        return err;
    }
    match err {
        ProviderError::Http {
            status,
            message,
            retry_after,
        } => ProviderError::Http {
            status,
            message: format!(
                "{message} — this model speaks the Responses API; select it with \
                 HOTL_MODEL=openai-responses/<model>"
            ),
            retry_after,
        },
        other => other,
    }
}

fn tool_json(t: &ToolDef) -> Value {
    json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}})
}

fn convert_item(item: &Item, out: &mut Vec<Value>, send_images: bool) {
    match item {
        Item::System { .. } | Item::Unknown => {}
        // INVARIANT: an imageless user item stays `"content": <plain string>`
        // — byte-identical to the pre-image wire. Some OpenAI-compat servers
        // reject the array content form, so it appears only when images ride.
        // Image parts precede the text part, matching the Anthropic dialect:
        // one provider-neutral ordering rule.
        Item::User { text, images, .. } => {
            if send_images && !images.is_empty() {
                let mut parts: Vec<Value> = images
                    .iter()
                    .map(|img| {
                        json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", img.media_type, img.data)
                            }
                        })
                    })
                    .collect();
                parts.push(json!({"type": "text", "text": text}));
                out.push(json!({"role": "user", "content": parts}));
            } else {
                let rendered =
                    hotl_provider::transform::text_with_omitted_images(text, images.len());
                out.push(json!({"role": "user", "content": rendered.as_ref()}));
            }
        }
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
            msg["content"] = if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            out.push(msg);
        }
        Item::ToolResults { results } => {
            for r in results {
                let content = if r.is_error && !r.content.starts_with(TOOL_ERROR_PREFIX) {
                    format!("{TOOL_ERROR_PREFIX}{}", r.content)
                } else {
                    r.content.clone()
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": r.tool_use_id,
                    "content": content,
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
            if let Some(n) = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
            {
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
                        out.push(StreamEvent::BlockStart {
                            index: 0,
                            kind: "text".into(),
                        });
                    }
                    self.text.push_str(text);
                    out.push(StreamEvent::TextDelta {
                        index: 0,
                        text: text.to_string(),
                    });
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let raw = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let idx = usize::try_from(raw).unwrap_or(usize::MAX);
                    if idx > hotl_provider::MAX_BLOCK_INDEX {
                        return Err(ProviderError::Parse(format!(
                            "tool_call index {raw} exceeds the {} block limit",
                            hotl_provider::MAX_BLOCK_INDEX
                        )));
                    }
                    while self.tools.len() <= idx {
                        self.tools
                            .push((String::new(), String::new(), String::new()));
                    }
                    let slot = &mut self.tools[idx];
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        if slot.0.is_empty() {
                            slot.0 = id.to_string();
                            out.push(StreamEvent::BlockStart {
                                index: idx + 1,
                                kind: "tool_use".into(),
                            });
                        }
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        slot.1.push_str(name);
                    }
                    if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str)
                    {
                        slot.2.push_str(args);
                        out.push(StreamEvent::ToolInputDelta {
                            index: idx + 1,
                            json: args.to_string(),
                        });
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(map_finish(reason));
            // INVARIANT (T3-14): close every block that opened. The
            // chat-completions wire has no per-block terminator, so
            // `finish_reason` is the point at which the block set is final —
            // emitted in the same order `finish()` assembles them. Enforced by
            // `every_block_start_has_a_matching_block_end`.
            if !self.got_final {
                if self.text_started {
                    out.push(StreamEvent::BlockEnd { index: 0 });
                }
                for idx in 0..self.tools.len() {
                    out.push(StreamEvent::BlockEnd { index: idx + 1 });
                }
            }
            self.got_final = true;
        }
        Ok(out)
    }

    fn finish(self) -> Result<StreamEvent, ProviderError> {
        if !self.got_final {
            return Err(ProviderError::Parse(
                "stream ended without finish_reason".into(),
            ));
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
    api_key: Option<&str>,
    body: &Value,
    attempt: u32,
) -> Attempt {
    let mut builder = client
        .post(url)
        .header("content-type", "application/json")
        .json(body);
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

/// Process-wide source of `spawn_arm` generation tokens (§S3.2) — see the
/// twin static in `hotl-provider-anthropic` for the full rationale.
static NEXT_ARM_TOKEN: AtomicU64 = AtomicU64::new(1);

/// §S3.2: fire one lightweight, credential-free GET at `url` to populate
/// `client`'s connection pool. See the twin function in
/// `hotl-provider-anthropic` for the full rationale — kept duplicated rather
/// than shared, matching this pair of crates' existing convention (compare
/// `http_client`, `classify_send`, `send_attempt` above).
fn spawn_arm(client: reqwest::Client, url: String, armed: Arc<AtomicU64>) -> ArmGuard {
    if armed.load(Ordering::Acquire) != 0 {
        return ArmGuard::noop();
    }
    let token = NEXT_ARM_TOKEN.fetch_add(1, Ordering::Relaxed);
    if armed
        .compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
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
        let _ = cancel_flag.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    })
}

impl Warmable for OpenAiCompatProvider {
    fn arm(&self) -> ArmGuard {
        spawn_arm(
            self.client.clone(),
            self.completions_url(),
            self.armed.clone(),
        )
    }
}

impl Provider for OpenAiCompatProvider {
    fn arm(&self) -> ArmGuard {
        <Self as Warmable>::arm(self)
    }

    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let client = self.client.clone();
        let url = self.completions_url();
        let source = self.key_source.clone();
        let headers_timeout = self.headers_timeout;
        let stream_idle_timeout = self.stream_idle_timeout;
        let mut legacy = self.legacy_max_tokens;
        let mut body = Self::body_for(&req, legacy);

        Box::pin(async_stream::stream! {
            // INVARIANT (T2-16): `attempt` counts network/HTTP attempts only.
            // An auth refresh is a separate, once-per-request budget owned by
            // `AuthRetry`, so a 401 → refresh → 429 sequence still has its
            // full retry allowance.
            let mut attempts_used: u32 = 0;
            let mut auth_retry = AuthRetry::default();
            let response = loop {
                let attempt = attempts_used + 1;
                let key = match source.get().await {
                    Ok(k) => k,
                    Err(e) => {
                        yield Err(ProviderError::Auth(e.0));
                        return;
                    }
                };
                let sent = tokio::time::timeout(
                    headers_timeout,
                    send_attempt(&client, &url, key.as_deref(), &body, attempt),
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
                    Attempt::Fail(e) if !legacy && rejects_max_completion_tokens(&e) => {
                        // A capability probe, not a failure: no retry slot.
                        legacy = true;
                        body = Self::body_for(&req, true);
                        yield Ok(StreamEvent::Retrying {
                            attempt,
                            reason: "endpoint predates max_completion_tokens — \
                                     retrying with max_tokens".into(),
                        });
                    }
                    Attempt::Fail(e) => {
                        yield Err(with_responses_hint(e));
                        return;
                    }
                }
            };
            yield Ok(StreamEvent::Started);
            let inner = hotl_provider::drive_sse(
                response.bytes_stream(),
                Assembler::default(),
                stream_idle_timeout,
            );
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

    /// R3/T1: the Responses-API assembler was deleted (dead code, and its two
    /// tests read as dialect coverage that did not exist). If this crate grows a
    /// Responses dialect again, it arrives with an HTTP client and an
    /// HTTP-level test, not an assembler nothing constructs.
    #[test]
    fn only_the_chat_completions_dialect_is_compiled_in() {
        let src = include_str!("lib.rs");
        // Split so this test's own source is not a match for itself.
        assert!(
            !src.contains(concat!("mod ", "responses")),
            "responses.rs is dead code — delete it or wire it to a real endpoint"
        );
    }

    /// INVARIANT (T3-13): a failed tool is legible as failed in both dialects.
    /// The chat-completions wire has no `is_error` field on a role:"tool"
    /// message, so the marker is in-band — but it must be there.
    #[test]
    fn tool_result_errors_are_legible_in_the_openai_dialect() {
        let req = SamplingRequest {
            model: "m".into(),
            max_tokens: 16,
            system: "".into(),
            items: hotl_provider::arc_items(vec![Item::ToolResults {
                results: vec![
                    ToolResultItem {
                        tool_use_id: "ok".into(),
                        content: "all good".into(),
                        is_error: false,
                    },
                    ToolResultItem {
                        tool_use_id: "bad".into(),
                        content: "No such file: a.rs. Check the path with `glob`.".into(),
                        is_error: true,
                    },
                ],
            }]),
            ephemeral_tail: std::sync::Arc::new(Vec::new()),
            tools: std::sync::Arc::from(Vec::<ToolDef>::new()),
            thinking: false,
            effort: None,
            cache: hotl_provider::CachePolicy::Off,
            turn_context: None,
            cache_key: None,
        };
        let body = OpenAiCompatProvider::build_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0]["content"], "all good",
            "success must not be decorated"
        );
        let failed = msgs[1]["content"].as_str().unwrap();
        assert!(failed.starts_with(TOOL_ERROR_PREFIX), "{failed}");
        assert!(
            failed.contains("Check the path"),
            "the prompt content must survive: {failed}"
        );
    }

    #[test]
    fn body_shape_drops_thinking_and_splits_tool_results() {
        let req = SamplingRequest {
            model: "gpt-test".into(),
            max_tokens: 512,
            system: "sys".into(),
            items: hotl_provider::arc_items(vec![
                Item::User {
                    text: "hi".into(),
                    synthetic: None,
                    images: Vec::new(),
                },
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
                        ToolResultItem {
                            tool_use_id: "toolu_1".into(),
                            content: "out1".into(),
                            is_error: false,
                        },
                        ToolResultItem {
                            tool_use_id: "toolu_2".into(),
                            content: "out2".into(),
                            is_error: true,
                        },
                    ],
                },
            ]),
            ephemeral_tail: hotl_provider::arc_items(vec![Item::User {
                text: "<todos>\n[~] a\n</todos>".into(),
                synthetic: Some(hotl_types::SyntheticReason::Todos),
                images: Vec::new(),
            }]),
            tools: vec![ToolDef {
                name: "read".into(),
                description: "d".into(),
                input_schema: json!({"type":"object"}),
            }]
            .into(),
            thinking: true,
            effort: None,
            cache: hotl_provider::CachePolicy::Static {
                prefix_ttl: hotl_provider::CacheTtl::FiveMinutes,
            },
            turn_context: Some("<turn-context/>".into()),
            cache_key: None,
        };
        let body = OpenAiCompatProvider::build_body(&req);
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap()["content"],
            "<turn-context/>"
        );
        let s = body.to_string();
        assert!(
            !s.contains("secret chain"),
            "foreign thinking must not cross providers"
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        // tool results become one role:"tool" message each
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "toolu_1");
        assert_eq!(msgs[4]["role"], "tool");
        // …then the ephemeral tail as a plain user message, THEN MOIM: the
        // suffix keeps its position in this dialect even though nothing here
        // is cached explicitly.
        assert_eq!(msgs[5]["role"], "user");
        assert_eq!(msgs[5]["content"], "<todos>\n[~] a\n</todos>");
        assert_eq!(msgs[6]["content"], "<turn-context/>");
        assert_eq!(msgs.len(), 7);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    /// The chat-completions twin of T2-9: `while self.tools.len() <= idx { push }`.
    #[test]
    fn an_absurd_tool_call_index_is_a_parse_error() {
        let mut a = Assembler::default();
        let data = r#"{"choices":[{"delta":{"tool_calls":[
            {"index":4000000000,"id":"c","function":{"name":"read","arguments":""}}]}}]}"#;
        assert!(matches!(a.handle(data), Err(ProviderError::Parse(_))));
    }

    /// INVARIANT (T3-14): every block the assembler opens is closed, in every
    /// dialect. `BlockEnd` is the contract's flush signal; a consumer must not
    /// have to know which provider is behind it.
    #[test]
    fn every_block_start_has_a_matching_block_end() {
        let chunks = [
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"read","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c1","function":{"name":"glob","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let mut a = Assembler::default();
        let mut events = Vec::new();
        for c in chunks {
            events.extend(a.handle(c).unwrap());
        }
        let starts: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::BlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        let ends: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::BlockEnd { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![0, 1, 2]);
        assert_eq!(ends, vec![0, 1, 2], "every opened block must be closed");
        // And every BlockEnd follows its BlockStart.
        for i in &starts {
            let s = events
                .iter()
                .position(|e| matches!(e, StreamEvent::BlockStart { index, .. } if index == i))
                .unwrap();
            let e = events
                .iter()
                .position(|e| matches!(e, StreamEvent::BlockEnd { index } if index == i))
                .unwrap();
            assert!(s < e, "BlockEnd {i} preceded its BlockStart");
        }
    }

    /// A text-only response closes block 0 and nothing else.
    #[test]
    fn a_text_only_response_closes_exactly_one_block() {
        let mut a = Assembler::default();
        let mut events = a
            .handle(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
            .unwrap();
        events.extend(
            a.handle(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
                .unwrap(),
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::BlockEnd { .. }))
                .count(),
            1
        );
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
        let StreamEvent::Completed {
            stop,
            usage,
            blocks,
        } = a.finish().unwrap()
        else {
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
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolInputDelta { .. })));
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
        fn refreshable(&self) -> bool {
            true
        }
    }

    const SSE_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
    const AUTH_401: &str = "HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad api key";

    /// Serve `responses` to consecutive connections; record each request's
    /// `authorization` header (lowercased) into `seen`.
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

    /// Sibling of [`tcp_double`] that records each request's **body** rather
    /// than its `authorization` header: read to the header break, take
    /// `content-length`, then read exactly that many more bytes.
    async fn tcp_double_bodies(
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
                let mut req = Vec::new();
                let head_end = loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    req.extend_from_slice(&buf[..n]);
                    if let Some(p) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&req[..head_end]).to_string();
                let len: usize = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split_once(':'))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                while req.len() < head_end + len {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req[head_end..]).to_string());
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
        });
        base
    }

    const UNKNOWN_PARAM_400: &str = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 85\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"Unrecognized request argument supplied: max_completion_tokens\"}}";

    /// An endpoint that predates `max_completion_tokens` gets the old spelling on
    /// a second try, rather than a dead end — the crate doc advertises Ollama.
    #[tokio::test]
    async fn an_older_endpoint_falls_back_to_max_tokens() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double_bodies(vec![UNKNOWN_PARAM_400, SSE_OK], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        let bodies = seen.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains("max_completion_tokens"));
        assert!(bodies[1].contains("\"max_tokens\""));
        assert!(!bodies[1].contains("max_completion_tokens"));
    }

    /// The one 400 whose fix is a different dialect gains the pointer to it
    /// — surfaced, never retried or degraded (plan 0031 task 7) — while a
    /// plain 400 stays untouched.
    #[tokio::test]
    async fn a_reasoning_effort_400_carries_the_responses_hint() {
        const LUNA_400: &str = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"Function tools with reasoning_effort are not supported for gpt-5.6-luna-1 in /v1/chat/completions. Please use /v1/responses instead.\",\"type\":\"invalid_request_error\"}}";
        const PLAIN_400: &str = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"bad schema\",\"type\":\"invalid_request_error\"}}";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![LUNA_400, PLAIN_400], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));

        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        let Some(Err(ProviderError::Http {
            status, message, ..
        })) = events.last()
        else {
            panic!("expected a terminal Http error: {events:?}");
        };
        assert_eq!(*status, 400);
        assert!(
            message.contains("HOTL_MODEL=openai-responses/<model>"),
            "{message}"
        );
        assert!(
            message.contains("reasoning_effort"),
            "the API's own sentence must survive: {message}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no retry, no degradation probe"
        );

        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        let Some(Err(ProviderError::Http { message, .. })) = events.last() else {
            panic!("expected a terminal Http error: {events:?}");
        };
        assert!(
            !message.contains("openai-responses"),
            "a plain 400 must not gain the hint: {message}"
        );
    }

    /// The success path over HTTP: the event sequence a consumer actually
    /// sees, including the `BlockEnd` that T3-14 was missing.
    #[tokio::test]
    async fn a_successful_stream_yields_the_full_event_sequence() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![SSE_OK], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        let oks: Vec<_> = events.into_iter().map(|e| e.expect("no error")).collect();
        assert!(matches!(oks[0], StreamEvent::Started));
        assert!(oks
            .iter()
            .any(|e| matches!(e, StreamEvent::BlockStart { index: 0, .. })));
        assert!(oks
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "hi")));
        assert!(
            oks.iter()
                .any(|e| matches!(e, StreamEvent::BlockEnd { index: 0 })),
            "T3-14: every dialect closes its blocks"
        );
        let Some(StreamEvent::Completed {
            stop,
            usage,
            blocks,
        }) = oks.last()
        else {
            panic!("no terminal event: {oks:?}")
        };
        assert_eq!(*stop, StopReason::EndTurn);
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(blocks[0]["text"], "hi");
    }

    /// The OpenAI twin of the retry test.
    #[tokio::test]
    async fn a_429_is_retried_over_a_real_socket() {
        const RETRY_429: &str = "HTTP/1.1 429 Too Many Requests\r\ncontent-type: text/plain\r\nretry-after: 0\r\ncontent-length: 5\r\nconnection: close\r\n\r\nslow!";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![RETRY_429, SSE_OK], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// The unterminated-final-line case at the HTTP layer (Task 3): a server
    /// that closes the socket right after its last `data:` line still
    /// produces a complete response.
    #[tokio::test]
    async fn a_stream_ending_without_a_trailing_newline_still_completes() {
        const SSE_NO_TRAILING_NEWLINE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![SSE_NO_TRAILING_NEWLINE], seen.clone()).await;
        let p = OpenAiCompatProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(
            matches!(events.last(), Some(Ok(StreamEvent::Completed { .. }))),
            "the terminal event was dropped with the unterminated line: {events:?}"
        );
    }

    /// §S3.2: the HTTP/2 keep-alive knobs must ride the same builder as the
    /// existing timeouts.
    #[test]
    fn the_client_enables_http2_keep_alive() {
        let src = include_str!("lib.rs");
        assert!(src.contains("http2_keep_alive_interval"));
        assert!(src.contains("http2_keep_alive_while_idle"));
    }

    /// §S3.2 unit: dropping an armed guard resets "currently arming" state
    /// synchronously.
    #[tokio::test]
    async fn spawn_arm_drop_resets_the_armed_flag_immediately() {
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            "http://192.0.2.1/".into(),
            armed.clone(),
        );
        assert_ne!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "arm() must mark the provider as arming"
        );
        drop(guard);
        assert_eq!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "drop must reset armed state immediately — not wait for the background \
             task or its internal timeout"
        );
    }

    /// §S3.2 unit: a second `arm()` while the first is still armed is a
    /// no-op — dropping it must not disturb the still-live first guard's
    /// state.
    #[tokio::test]
    async fn spawn_arm_second_call_while_armed_is_a_noop() {
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client = http_client(std::time::Duration::from_secs(1));
        let g1 = spawn_arm(client.clone(), "http://192.0.2.1/".into(), armed.clone());
        let before = armed.load(std::sync::atomic::Ordering::Acquire);
        let g2 = spawn_arm(client, "http://192.0.2.1/".into(), armed.clone());
        drop(g2);
        assert_eq!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            before,
            "a no-op re-arm's drop must not reset state owned by the still-live first guard"
        );
        drop(g1);
        assert_eq!(armed.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    /// §S3.2 unit: arming against a refused-connection target must not
    /// panic and must not leak its background task.
    #[tokio::test]
    async fn spawn_arm_against_a_refused_target_does_not_leak_its_task() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            format!("http://{addr}/"),
            armed.clone(),
        );
        // Windows refuses a connection to a dropped-listener port slowly, so
        // bound this well above ARM_TIMEOUT rather than at 1s (as the sibling
        // black-holed test already does).
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(std::sync::atomic::Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a refused warm request must not leak its background task");
        drop(guard);
    }

    /// §S3.2 unit: a target that never answers (RFC 5737 TEST-NET-1) must
    /// still be bounded by the internal arm timeout.
    #[tokio::test]
    async fn spawn_arm_against_a_black_holed_target_is_bounded_by_the_arm_timeout() {
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            "http://192.0.2.1/".into(),
            armed.clone(),
        );
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(std::sync::atomic::Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a black-holed target must not hang past the internal arm timeout");
        drop(guard);
    }

    /// §S3.2 unit: a guard whose own warm request already finished must not
    /// clobber a later, still-in-flight arm when dropped late. See the twin
    /// test in `hotl-provider-anthropic` for the full rationale.
    #[tokio::test]
    async fn a_late_dropped_stale_guard_does_not_clobber_a_newer_arm() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client = http_client(std::time::Duration::from_secs(1));
        let g1 = spawn_arm(client.clone(), format!("http://{addr}/"), armed.clone());
        // Windows refuses a connection to a dropped-listener port slowly, so
        // bound this well above ARM_TIMEOUT rather than at 1s (as the sibling
        // black-holed test already does).
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(std::sync::atomic::Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("g1's task must finish on its own before we proceed");
        let g2 = spawn_arm(client, "http://192.0.2.1/".into(), armed.clone());
        assert_ne!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "g2 must be in flight"
        );
        drop(g1);
        assert_ne!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "dropping the stale g1 must not clobber g2's still-in-flight state"
        );
        drop(g2);
        assert_eq!(armed.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    /// The public wiring: `Provider::arm` must reach the same `Warmable`
    /// impl this crate defines.
    #[tokio::test]
    async fn provider_arm_reaches_the_warmable_impl_without_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let p = OpenAiCompatProvider::new(
            format!("http://{addr}"),
            Arc::new(hotl_provider::key::StaticKey(None)),
        );
        let guard = Provider::arm(&p);
        drop(guard);
    }

    /// The explicit opt-in skips the probe entirely.
    #[test]
    fn legacy_max_tokens_uses_the_old_spelling_from_the_start() {
        let body = OpenAiCompatProvider::body_for(&sampling_req(), true);
        assert_eq!(body["max_tokens"], 16);
        assert!(body.get("max_completion_tokens").is_none());
    }

    fn sampling_req() -> SamplingRequest {
        SamplingRequest {
            model: "m".into(),
            max_tokens: 16,
            system: "".into(),
            items: hotl_provider::arc_items(vec![Item::User {
                text: "hi".into(),
                synthetic: None,
                images: Vec::new(),
            }]),
            ephemeral_tail: std::sync::Arc::new(Vec::new()),
            tools: std::sync::Arc::from(Vec::<ToolDef>::new()),
            thinking: false,
            effort: None,
            cache: hotl_provider::CachePolicy::Off,
            turn_context: None,
            cache_key: None,
        }
    }

    #[test]
    fn effort_rides_top_level_as_reasoning_effort() {
        let mut req = sampling_req();
        req.thinking = true;
        req.effort = Some(Effort::XHigh);
        let body = OpenAiCompatProvider::build_body(&req);
        // `xhigh` is not a real OpenAI-compat rung; the ladder clamps it.
        assert_eq!(body["reasoning_effort"], "high");
    }

    /// Real servers 400 on `xhigh`/`max`; the ladder clamps down to the
    /// dialect's ceiling while the floor passes through untouched.
    #[test]
    fn max_clamps_to_high_on_the_openai_dialect() {
        let mut req = sampling_req();
        req.thinking = true;
        req.effort = Some(Effort::Max);
        let body = OpenAiCompatProvider::build_body(&req);
        assert_eq!(body["reasoning_effort"], "high");
        req.effort = Some(Effort::Low);
        let body = OpenAiCompatProvider::build_body(&req);
        assert_eq!(body["reasoning_effort"], "low");
    }

    /// The byte-identity guard.
    #[test]
    fn an_unset_effort_with_thinking_on_emits_nothing() {
        let mut req = sampling_req();
        req.thinking = true;
        let body = OpenAiCompatProvider::build_body(&req);
        assert!(body.get("reasoning_effort").is_none());
    }

    /// The shipped narrowing of the plan's 3.3: depth is only spoken about at
    /// all once the session opted into the ladder, so a `thinking: false`
    /// request that never set an effort keeps its pre-ladder bytes.
    #[test]
    fn thinking_off_alone_still_emits_nothing() {
        let req = sampling_req();
        assert!(!req.thinking && req.effort.is_none());
        let body = OpenAiCompatProvider::build_body(&req);
        assert!(body.get("reasoning_effort").is_none());
    }

    /// The off-switch wins: `none` is the rung the neutral ladder does not
    /// carry, and this dialect is the only one with a word for it.
    #[test]
    fn thinking_off_beats_a_set_effort() {
        let mut req = sampling_req();
        req.thinking = false;
        req.effort = Some(Effort::Max);
        let body = OpenAiCompatProvider::build_body(&req);
        assert_eq!(body["reasoning_effort"], "none");
    }

    /// The compat family is uncatalogued by design, so nothing gates the field.
    #[test]
    fn a_local_model_gets_the_field_too() {
        let mut req = sampling_req();
        req.model = "llama3".into();
        req.thinking = true;
        req.effort = Some(Effort::Low);
        let body = OpenAiCompatProvider::build_body(&req);
        assert_eq!(body["reasoning_effort"], "low");
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
        let p = OpenAiCompatProvider::new(base, Arc::new(FlippingKey(StdMutex::new(1))));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(
            events.iter().all(|e| e.is_ok()),
            "no error expected: {events:?}"
        );
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer key-1", "Bearer key-2"]);
    }

    #[tokio::test]
    async fn static_source_auth_401_surfaces_immediately() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![AUTH_401], seen.clone()).await;
        let p = OpenAiCompatProvider::new(
            base,
            Arc::new(hotl_provider::key::StaticKey(Some("sk".into()))),
        );
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Err(ProviderError::Auth(_)))));
        assert_eq!(seen.lock().unwrap().len(), 1); // exactly one request — no blind retry
    }

    /// An image-bearing user item renders as the array content form: data-URL
    /// image parts first, the text part last (the same ordering as the
    /// Anthropic dialect). Model "m" is uncatalogued → the gate fails open.
    #[test]
    fn image_parts_render_as_data_urls_before_the_text_part() {
        let mut req = sampling_req();
        req.items = hotl_provider::arc_items(vec![Item::User {
            text: "what is this?".into(),
            synthetic: None,
            images: vec![hotl_types::UserImage {
                media_type: "image/png".into(),
                data: "aW1n".into(),
            }],
        }]);
        let body = OpenAiCompatProvider::build_body(&req);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,aW1n");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this?");
    }

    /// INVARIANT: an imageless user item keeps the plain-string content form,
    /// byte-identical to the pre-image wire — some OpenAI-compat servers
    /// reject array content.
    #[test]
    fn an_imageless_user_item_stays_a_plain_string() {
        let body = OpenAiCompatProvider::build_body(&sampling_req());
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    /// The gated path: images dropped, one deterministic omission note inside
    /// the plain-string content. Threaded directly through `convert_item`
    /// because no catalog row combines the OpenAI dialect with images: false.
    #[test]
    fn a_gated_model_gets_the_plain_string_with_an_omission_note() {
        let mut out = Vec::new();
        convert_item(
            &Item::User {
                text: "see [Image #1]".into(),
                synthetic: None,
                images: vec![hotl_types::UserImage {
                    media_type: "image/png".into(),
                    data: "aW1n".into(),
                }],
            },
            &mut out,
            false,
        );
        let content = out[0]["content"].as_str().unwrap();
        assert!(content.starts_with("see [Image #1]\n\n[note: 1 attached image(s)"));
    }
}
