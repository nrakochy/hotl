//! L2 — the provider seam.
//!
//! `stream()` is the one required method. Events carry block structure
//! (review Arch #6): every delta names its block index, and the provider —
//! the only layer that understands its own wire format — assembles the final
//! verbatim assistant blocks and hands them over in `Completed`. The engine
//! never demuxes provider wire formats.
//!
//! Native (Send) variants are authoritative for M0; the `?Send` browser twins
//! are derived at the gated milestone (rust-implementation §Key trait signatures).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::stream::BoxStream;
use hotl_types::{Item, StopReason, TokenUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The three bounds every HTTP provider applies, in one place so the two
/// dialect crates cannot drift.
///
/// A single request-level timeout is deliberately *not* used: it would bound
/// the response body, and the response body of a streaming sample is the whole
/// turn. Each bound below covers one place a request can actually stop making
/// progress.
pub mod timeouts {
    use std::time::Duration;

    /// TCP + TLS handshake. A stalled handshake is the fastest-failing case
    /// and needs no generosity.
    pub const CONNECT: Duration = Duration::from_secs(10);

    /// Request sent, response headers not yet received. Generous: a local
    /// server loading model weights can delay headers by a minute.
    pub const HEADERS: Duration = Duration::from_secs(120);

    /// Maximum gap between two SSE chunks once the stream is open. Long
    /// enough for extended thinking between pings, short enough that a dead
    /// connection is noticed within a coffee break.
    pub const STREAM_IDLE: Duration = Duration::from_secs(300);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct SamplingRequest {
    pub model: String,
    pub max_tokens: u32,
    /// Byte-stable owner system prompt (L6 discipline).
    pub system: Arc<str>,
    /// The **durable** projection only — exactly the items the session log
    /// carries. Byte-stable between supersede events, which is what makes it
    /// the one region a cache breakpoint may be placed in (L6 discipline).
    pub items: Arc<Vec<Item>>,
    /// Ephemeral per-sample suffix (the `<todos>` reminder today): regenerated
    /// on every read of the projection head, never committed, and serialized
    /// AFTER every cache marker (and before MOIM). Splitting it from `items`
    /// is what makes "a marker on ephemeral content" unrepresentable rather
    /// than merely avoided — a serializer has no ephemeral item to pick.
    pub ephemeral_tail: Arc<Vec<Item>>,
    pub tools: Arc<[ToolDef]>,
    /// Adaptive thinking on models that support it.
    pub thinking: bool,
    /// Cache-breakpoint placement for explicit-cache providers.
    pub cache: CachePolicy,
    /// MOIM (M2): ephemeral per-turn context, sent as a trailing user block
    /// after the cache marker AND after [`Self::ephemeral_tail`] — always the
    /// last thing on the wire. Never persisted; it exists only on the wire.
    pub turn_context: Option<String>,
}

/// How long a cache entry a breakpoint writes stays readable. Anthropic's two
/// offered lifetimes; the wire spelling is the serializer's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}

/// What a provider may do with cache breakpoints for this request.
///
/// A policy, not a hint: `Off` means a serializer emits no `cache_control` at
/// all, and only the byte-stable prefix — never [`SamplingRequest::items`]'s
/// ephemeral companion — is ever eligible under `Static`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// No explicit markers: implicit-caching providers (OpenAI), the
    /// compaction summarize call, and fixtures.
    Off,
    /// Explicit marker placement. `prefix_ttl` is the lifetime the *prefix*
    /// breakpoints (and the rolling anchors — `cache_plan::Plan::anchors`)
    /// ask for; the serializer's LATEST marker always renders plain
    /// regardless of `prefix_ttl` (its segment is rewritten every sample, so
    /// the longer-lived write premium there recurs per turn and buys
    /// nothing — see `hotl-provider-anthropic`'s marker sites).
    Static { prefix_ttl: CacheTtl },
}

impl CachePolicy {
    /// Whether this request wants explicit breakpoints at all. The one
    /// question serializers ask before consulting `prefix_ttl` for the
    /// per-marker lifetime.
    pub fn marks_breakpoints(self) -> bool {
        matches!(self, Self::Static { .. })
    }
}

/// The unified, channel-tagged, block-structured event enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    Started,
    BlockStart {
        index: usize,
        kind: String,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        text: String,
    },
    ToolInputDelta {
        index: usize,
        json: String,
    },
    BlockEnd {
        index: usize,
    },
    Retrying {
        attempt: u32,
        reason: String,
    },
    /// Terminal event: the provider-assembled verbatim assistant blocks
    /// (echo these back on the next request — replay-safe by construction).
    Completed {
        stop: StopReason,
        usage: TokenUsage,
        blocks: Vec<Value>,
    },
}

/// `Clone` so a scripted stream can hand its script back when it is
/// abandoned (see [`ScriptedProvider`]); every variant is plain data.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after: Option<u64>,
    },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("stream parse error: {0}")]
    Parse(String),
}

pub trait Provider: Send + Sync {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>>;

    /// Pre-warm the connection pool ahead of the first real sample (§S3.2:
    /// typing-time connection arming). No-op by default — only a provider
    /// with a real pooled connection to warm overrides it (via
    /// [`Warmable`]); a scripted/test provider has nothing to arm.
    fn arm(&self) -> ArmGuard {
        ArmGuard::noop()
    }
}

/// A provider whose connection pool can be pre-warmed. Implemented by the
/// HTTP dialect crates (`hotl-provider-anthropic`, `hotl-provider-openai`).
/// `Provider::arm` is the dynamic-dispatch entry point call sites use
/// (`Arc<dyn Provider>` erases the concrete type long before a trigger
/// fires); this trait names the capability and lets each dialect crate
/// implement it against its own `reqwest::Client` without this crate having
/// to depend on `reqwest` itself (this crate stays wasm32-buildable — see
/// the `tokio` dependency comment above).
pub trait Warmable {
    fn arm(&self) -> ArmGuard;
}

/// RAII handle for a connection warm-up. Dropping it cancels any warm
/// request still in flight; [`ArmGuard::noop`] (nothing to warm, or a
/// re-arm while already armed) holds and cancels nothing.
///
/// Deliberately generic over *any* cancel callback rather than naming
/// `tokio::task::JoinHandle` directly: this crate has no `rt` feature and no
/// `reqwest` dependency (wasm32-buildable), so the dialect crates hand in a
/// closure that aborts their own real task.
///
/// `#[must_use]`: a bare `provider.arm();` statement drops the guard at the
/// end of that statement, immediately cancelling the very request it just
/// started — the opposite of the caller's intent. Bind it (even to `_name`)
/// or call [`ArmGuard::detach`] explicitly.
#[must_use]
pub struct ArmGuard {
    cancel: Option<Box<dyn FnOnce() + Send>>,
}

impl ArmGuard {
    /// Nothing to warm — holds and cancels nothing on drop.
    pub fn noop() -> Self {
        Self { cancel: None }
    }

    /// Wrap a cancel callback: `drop` invokes it exactly once.
    pub fn new(cancel: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cancel: Some(Box::new(cancel)),
        }
    }

    /// Let the warm task run to completion on its own: this guard no longer
    /// cancels it on drop. For trigger sites with no session-scoped owner to
    /// hold the guard across — safe because the underlying task is
    /// short-lived by construction (bounded by its own internal timeout).
    pub fn detach(mut self) {
        self.cancel = None;
    }
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

#[cfg(test)]
mod arm_guard_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The core RAII contract: dropping an armed guard cancels it.
    #[test]
    fn drop_invokes_the_cancel_callback() {
        let called = Arc::new(AtomicBool::new(false));
        let flag = called.clone();
        let guard = ArmGuard::new(move || flag.store(true, Ordering::SeqCst));
        assert!(!called.load(Ordering::SeqCst));
        drop(guard);
        assert!(called.load(Ordering::SeqCst));
    }

    /// Nothing to warm means nothing happens on drop — and, just as
    /// important, dropping it must not panic.
    #[test]
    fn noop_cancels_nothing() {
        drop(ArmGuard::noop());
    }

    /// `detach` is the escape hatch for call sites with no natural owner:
    /// the task is left to finish (or hit its own timeout) rather than being
    /// cancelled when the guard goes out of scope.
    #[test]
    fn detach_suppresses_the_cancel_callback() {
        let called = Arc::new(AtomicBool::new(false));
        let flag = called.clone();
        let guard = ArmGuard::new(move || flag.store(true, Ordering::SeqCst));
        guard.detach();
        assert!(!called.load(Ordering::SeqCst));
    }

    /// `Provider::arm`'s default: a provider with nothing to warm (every
    /// scripted/fake provider in the workspace) is armable without error —
    /// callers never need to special-case "does this provider support
    /// arming".
    #[test]
    fn provider_default_arm_is_a_noop() {
        let provider = ScriptedProvider::new(vec![]);
        // Must not panic; dropping immediately must not either.
        drop(provider.arm());
    }
}

/// The honest "second impl" (D9): a scripted provider driving the real engine
/// in tests. Each `stream()` call pops the next script.
pub struct ScriptedProvider {
    /// `Arc` so a handed-out stream can give its script back on drop —
    /// see [`ScriptedStream`].
    scripts: ScriptQueue,
    /// Every request the engine made, for test assertions on what the model saw.
    requests: Mutex<Vec<SamplingRequest>>,
}

/// One sample's worth of scripted events.
type Script = Vec<Result<StreamEvent, ProviderError>>;
/// The scripts a [`ScriptedProvider`] has left to hand out, shared with
/// every stream it hands out so an abandoned one can give its script back.
type ScriptQueue = Arc<Mutex<VecDeque<Script>>>;

/// One scripted sample, in flight.
///
/// **A stream abandoned before end-of-stream returns its script** to the
/// front of the queue. That is what makes optimistic dispatch (S2c) testable:
/// a speculative sample cancelled on a mispredict must not eat the reply the
/// sequential rebuild is about to ask for. A stream the consumer drove to
/// `None` restores nothing — that script was really consumed — so every
/// ordinary scenario is unaffected, and an engine that abandons a stream
/// mid-flight (a cancelled turn, a mispredict) leaves the queue exactly as
/// it found it.
struct ScriptedStream {
    script: Script,
    pos: usize,
    /// Set when `poll_next` has answered `None`: the script is spent.
    exhausted: bool,
    scripts: ScriptQueue,
}

impl futures_util::Stream for ScriptedStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.script.get(this.pos) {
            Some(event) => {
                this.pos += 1;
                std::task::Poll::Ready(Some(event.clone()))
            }
            None => {
                this.exhausted = true;
                std::task::Poll::Ready(None)
            }
        }
    }
}

impl Drop for ScriptedStream {
    fn drop(&mut self) {
        if self.exhausted {
            return;
        }
        self.scripts
            .lock()
            .expect("scripted provider mutex")
            .push_front(std::mem::take(&mut self.script));
    }
}

impl ScriptedProvider {
    pub fn new(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every captured request. Cheap since a request's history/tools are
    /// shared (`Arc`) — the clone copies pointers, not items.
    pub fn requests(&self) -> Vec<SamplingRequest> {
        self.requests.lock().expect("requests mutex").clone()
    }

    /// The most recent request, if any.
    pub fn last_request(&self) -> Option<SamplingRequest> {
        self.requests
            .lock()
            .expect("requests mutex")
            .last()
            .cloned()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests mutex").len()
    }

    /// Append a script after construction (tests that need the harness's
    /// paths before the scripts can be written).
    pub fn push_script(&self, script: Vec<Result<StreamEvent, ProviderError>>) {
        self.scripts
            .lock()
            .expect("scripted provider mutex")
            .push_back(script);
    }

    /// Convenience: a one-sample script that answers with plain text.
    pub fn text_reply(text: &str) -> Vec<Result<StreamEvent, ProviderError>> {
        vec![
            Ok(StreamEvent::Started),
            Ok(StreamEvent::BlockStart {
                index: 0,
                kind: "text".into(),
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                text: text.into(),
            }),
            Ok(StreamEvent::BlockEnd { index: 0 }),
            Ok(StreamEvent::Completed {
                stop: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
                blocks: vec![serde_json::json!({"type": "text", "text": text})],
            }),
        ]
    }

    /// Convenience: a sample that calls one tool.
    pub fn tool_call(
        id: &str,
        name: &str,
        input: Value,
    ) -> Vec<Result<StreamEvent, ProviderError>> {
        let block = serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input});
        vec![
            Ok(StreamEvent::Started),
            Ok(StreamEvent::BlockStart {
                index: 0,
                kind: "tool_use".into(),
            }),
            Ok(StreamEvent::BlockEnd { index: 0 }),
            Ok(StreamEvent::Completed {
                stop: StopReason::ToolUse,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                    ..Default::default()
                },
                blocks: vec![block],
            }),
        ]
    }
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        self.requests.lock().expect("requests mutex").push(req);
        let script = self
            .scripts
            .lock()
            .expect("scripted provider mutex")
            .pop_front()
            .unwrap_or_else(|| {
                vec![Err(ProviderError::Transport(
                    "scripted provider exhausted".into(),
                ))]
            });
        Box::pin(ScriptedStream {
            script,
            pos: 0,
            exhausted: false,
            scripts: Arc::clone(&self.scripts),
        })
    }
}

/// Normalize a configured base URL to one ending in `/v1`.
///
/// Two spellings are in the wild and users copy whichever their endpoint's
/// docs show: hotl's own convention (and the OpenAI provider's) puts the
/// version in the base, while bridges document the bare origin because
/// official SDKs append the whole versioned path themselves. Everything that
/// builds a URL from a configured base goes through here, so the provider and
/// `hotl doctor` can never disagree about where an endpoint lives.
pub fn v1_base(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

#[cfg(test)]
mod base_url_tests {
    use super::v1_base;

    #[test]
    fn both_spellings_and_trailing_slashes_resolve_alike() {
        for input in [
            "http://127.0.0.1:3456",
            "http://127.0.0.1:3456/",
            "http://127.0.0.1:3456/v1",
            "http://127.0.0.1:3456/v1/",
        ] {
            assert_eq!(v1_base(input), "http://127.0.0.1:3456/v1", "input: {input}");
        }
    }
}

/// SSE parsing shared by HTTP providers (WHATWG server-sent events).
///
/// Chunks split mid-line and mid-UTF-8 code point, so bytes are buffered, not
/// lossily decoded per chunk. Fields accumulate until a blank line dispatches
/// the event; consecutive `data:` fields join with `\n`.
///
/// INVARIANT: no input can make this allocate without bound — a line is capped
/// at [`SSE_MAX_BUFFER`] and an event at [`SSE_MAX_EVENT`]. Enforced by
/// `sse_parser_tests::over_cap_input_is_a_parse_error_not_an_oom`.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// `data:` field values for the event currently accumulating.
    data: Vec<String>,
    data_len: usize,
}

/// A stream that never sends a newline must not buffer without bound.
pub const SSE_MAX_BUFFER: usize = 1024 * 1024;
/// Nor may one that sends endless `data:` lines and never a blank line.
pub const SSE_MAX_EVENT: usize = 4 * 1024 * 1024;

impl SseParser {
    /// Feed a chunk; returns the payloads of every event completed by it
    /// (`[DONE]` filtered).
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<String>, ProviderError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[start..start + pos])
                .trim_end_matches('\r')
                .to_string();
            start += pos + 1;
            self.line(&line, &mut out)?;
        }
        self.buf.drain(..start);
        if self.buf.len() > SSE_MAX_BUFFER {
            return Err(ProviderError::Parse(format!(
                "SSE line exceeded {SSE_MAX_BUFFER} bytes without a newline"
            )));
        }
        Ok(out)
    }

    /// End-of-stream flush: a final line with no trailing newline, and an
    /// event never terminated by a blank line, are both real (servers close
    /// the socket after the last `data:`). Dropping them loses the terminal
    /// event and turns a complete response into a parse error.
    pub fn finish(&mut self) -> Result<Vec<String>, ProviderError> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let tail = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&tail)
                .trim_end_matches('\r')
                .to_string();
            self.line(&line, &mut out)?;
        }
        self.dispatch(&mut out);
        Ok(out)
    }

    fn line(&mut self, line: &str, out: &mut Vec<String>) -> Result<(), ProviderError> {
        if line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(()); // comment / keep-alive
        }
        // `event:`, `id:`, `retry:` carry nothing this seam uses; both
        // dialects put the whole event in `data`.
        let Some(value) = line.strip_prefix("data:") else {
            return Ok(());
        };
        // Spec: strip exactly one leading space, not all whitespace.
        let value = value.strip_prefix(' ').unwrap_or(value);
        self.data_len += value.len() + 1;
        if self.data_len > SSE_MAX_EVENT {
            return Err(ProviderError::Parse(format!(
                "SSE event exceeded {SSE_MAX_EVENT} bytes without a blank line"
            )));
        }
        self.data.push(value.to_string());
        Ok(())
    }

    fn dispatch(&mut self, out: &mut Vec<String>) {
        self.data_len = 0;
        if self.data.is_empty() {
            return;
        }
        let payload = std::mem::take(&mut self.data).join("\n");
        if !payload.is_empty() && payload != "[DONE]" {
            out.push(payload);
        }
    }
}

#[cfg(test)]
mod sse_parser_tests {
    use super::*;

    /// INVARIANT: consecutive `data:` fields in one event are joined with a
    /// newline and dispatched at the blank line (WHATWG SSE). A payload split
    /// across two `data:` lines is legal and must parse as one JSON document.
    #[test]
    fn multi_line_data_fields_are_joined() {
        let mut p = SseParser::default();
        let out = p
            .feed(b"event: x\ndata: {\"a\":\ndata: 1}\n\ndata: {\"b\":2}\n\n")
            .unwrap();
        assert_eq!(
            out,
            vec!["{\"a\":\n1}".to_string(), "{\"b\":2}".to_string()]
        );
        // Both are valid JSON, which is the whole point.
        for payload in &out {
            serde_json::from_str::<serde_json::Value>(payload).expect(payload);
        }
    }

    /// INVARIANT: a final event with no trailing newline is delivered, not
    /// dropped. Servers close the socket after the last line all the time.
    #[test]
    fn the_unterminated_final_line_is_flushed() {
        let mut p = SseParser::default();
        assert!(p
            .feed(b"data: {\"type\":\"message_stop\"}")
            .unwrap()
            .is_empty());
        assert_eq!(p.finish().unwrap(), vec!["{\"type\":\"message_stop\"}"]);
    }

    /// An event terminated by end-of-stream rather than a blank line is also
    /// flushed — same failure mode, one newline later.
    #[test]
    fn a_trailing_event_without_a_blank_line_is_flushed() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: {\"n\":1}\n").unwrap().is_empty());
        assert_eq!(p.finish().unwrap(), vec!["{\"n\":1}"]);
    }

    /// Comments (`:` keep-alives) and non-`data` fields are ignored, and
    /// `[DONE]` never reaches an assembler.
    #[test]
    fn comments_other_fields_and_done_are_filtered() {
        let mut p = SseParser::default();
        let out = p
            .feed(b": keepalive\nevent: ping\nid: 7\nretry: 100\ndata: [DONE]\n\ndata: {}\n\n")
            .unwrap();
        assert_eq!(out, vec!["{}".to_string()]);
    }

    /// INVARIANT: a stream that never sends a newline, and one that never
    /// sends a blank line, are both bounded. Neither may grow without limit.
    #[test]
    fn over_cap_input_is_a_parse_error_not_an_oom() {
        let mut p = SseParser::default();
        let big = vec![b'x'; SSE_MAX_BUFFER + 1];
        assert!(matches!(p.feed(&big), Err(ProviderError::Parse(_))));

        let mut q = SseParser::default();
        let line = format!("data: {}\n", "y".repeat(64 * 1024));
        let err = loop {
            if let Err(e) = q.feed(line.as_bytes()) {
                break e;
            }
        };
        assert!(matches!(err, ProviderError::Parse(_)), "{err:?}");
    }

    /// Reassembly across arbitrary chunk boundaries still holds, including
    /// mid-UTF-8 splits.
    #[test]
    fn chunk_boundaries_and_utf8_splits_survive() {
        let wire = "data: {\"t\":\"héllo → wörld\"}\n\n".as_bytes();
        let mut p = SseParser::default();
        let mut out = Vec::new();
        for c in wire.chunks(3) {
            out.extend(p.feed(c).unwrap());
        }
        out.extend(p.finish().unwrap());
        assert_eq!(out.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(v["t"], "héllo → wörld");
    }
}

/// Pure-data retry classification (RELIABILITY.md — never regex
/// on prose). Both HTTP providers consult this; budgets reset per sample.
pub mod retry {
    use super::ProviderError;
    use std::time::Duration;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Decision {
        /// Wait this long, then retry. The value is the *deterministic* base;
        /// apply [`with_jitter`] at the sleep site.
        Retry { delay: Duration },
        /// Not recoverable by retrying (auth, parse, client errors).
        Fatal,
    }

    /// Five attempts: four retries at base 1/2/4/8s. Three was thin for
    /// `overloaded_error` (529), the case this loop primarily exists for.
    pub const MAX_ATTEMPTS: u32 = 5;

    /// No single backoff exceeds this, however large `Retry-After` is. A
    /// server answering `Retry-After: 3600` must not sleep the session for an
    /// hour with only Ctrl-C as an exit (T2-16).
    pub const RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

    /// `attempt` is 1-based (the attempt that just failed). Deterministic and
    /// jitter-free on purpose, so the policy is exactly assertable.
    pub fn classify(err: &ProviderError, attempt: u32) -> Decision {
        if attempt >= MAX_ATTEMPTS {
            return Decision::Fatal;
        }
        let backoff = Duration::from_secs(1u64 << (attempt - 1));
        match err {
            ProviderError::Http {
                status,
                retry_after,
                ..
            } if *status == 429 || *status >= 500 => Decision::Retry {
                delay: retry_after
                    .map(Duration::from_secs)
                    .unwrap_or(backoff)
                    .min(RETRY_AFTER_CAP),
            },
            ProviderError::Transport(_) => Decision::Retry {
                delay: backoff.min(RETRY_AFTER_CAP),
            },
            _ => Decision::Fatal,
        }
    }

    /// Full jitter (AWS "Exponential Backoff and Jitter"): a uniform draw in
    /// `[base/2, base]`.
    ///
    /// INVARIANT (T2-16): concurrent hotl processes do not retry in lockstep.
    /// Enforced by `jitter_stays_in_range_and_actually_varies`.
    ///
    /// The entropy source is deliberately dependency-free: `RandomState` is
    /// seeded per process and advances per call, which is exactly the
    /// cross-process divergence a thundering herd needs. Not a security
    /// context — do not reuse this for anything that is.
    pub fn with_jitter(base: Duration) -> Duration {
        use std::hash::{BuildHasher, Hasher};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(nanos);
        h.write_u32(std::process::id());
        let half = base / 2;
        let span = base.saturating_sub(half).as_nanos() as u64;
        if span == 0 {
            return base;
        }
        half + Duration::from_nanos(h.finish() % (span + 1))
    }

    /// Seconds since the Unix epoch, for [`parse_retry_after`].
    pub fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Parse a `Retry-After` header in **either** RFC 9110 form: delta-seconds
    /// (`120`) or an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`). Returns
    /// seconds to wait; a date already in the past yields `0`.
    ///
    /// Only the fixdate form is accepted for the date variant — the two
    /// obsolete forms (RFC 850, asctime) are not emitted by any modern API and
    /// a miss falls back to exponential backoff, which is safe.
    pub fn parse_retry_after(value: &str, now_unix: u64) -> Option<u64> {
        let v = value.trim();
        if let Ok(secs) = v.parse::<u64>() {
            return Some(secs);
        }
        // "Wed, 21 Oct 2015 07:28:00 GMT"
        let rest = v.split_once(", ")?.1;
        let mut parts = rest.split(' ');
        let day: u32 = parts.next()?.parse().ok()?;
        let month = match parts.next()? {
            "Jan" => 1,
            "Feb" => 2,
            "Mar" => 3,
            "Apr" => 4,
            "May" => 5,
            "Jun" => 6,
            "Jul" => 7,
            "Aug" => 8,
            "Sep" => 9,
            "Oct" => 10,
            "Nov" => 11,
            "Dec" => 12,
            _ => return None,
        };
        let year: i64 = parts.next()?.parse().ok()?;
        let mut hms = parts.next()?.split(':');
        let h: u64 = hms.next()?.parse().ok()?;
        let m: u64 = hms.next()?.parse().ok()?;
        let s: u64 = hms.next()?.parse().ok()?;
        if h > 23 || m > 59 || s > 60 || !(1..=31).contains(&day) {
            return None;
        }
        let secs =
            days_from_civil(year, month, day).checked_mul(86_400)? as u64 + h * 3600 + m * 60 + s;
        Some(secs.saturating_sub(now_unix))
    }

    /// Days since 1970-01-01 for a proleptic-Gregorian civil date
    /// (Howard Hinnant's `days_from_civil`). ~15 lines of `std` instead of a
    /// date crate for one header.
    fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400; // [0, 399]
        let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // Mar = 0
        let doy = (153 * mp + 2) / 5 + d as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// Availability-class errors are the only ones that justify falling back
    /// to another model (never auth/billing/parse).
    pub fn is_availability(err: &ProviderError) -> bool {
        matches!(
            err,
            ProviderError::Http { status, .. } if *status == 429 || *status >= 500
        ) || matches!(err, ProviderError::Transport(_))
    }

    /// Context-overflow detection (M2 compaction trigger). Both dialects
    /// report overflow as a 400 whose message names the context/length limit;
    /// this matches on *wire error* text (structured API data, not model
    /// prose — the RELIABILITY.md rule concerns the latter). A miss is safe:
    /// the pre-sample threshold catches what this doesn't.
    pub fn is_context_overflow(err: &ProviderError) -> bool {
        let ProviderError::Http {
            status: 400,
            message,
            ..
        } = err
        else {
            return false;
        };
        let m = message.to_lowercase();
        [
            "too long",
            "context length",
            "context window",
            "tokens exceed",
        ]
        .iter()
        .any(|needle| m.contains(needle))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn overflow_detection() {
            let overflow = ProviderError::Http {
                status: 400,
                message:
                    r#"{"error":{"message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#
                        .into(),
                retry_after: None,
            };
            assert!(is_context_overflow(&overflow));
            let oai = ProviderError::Http {
                status: 400,
                message: "This model's maximum context length is 128000 tokens".into(),
                retry_after: None,
            };
            assert!(is_context_overflow(&oai));
            let plain_400 = ProviderError::Http {
                status: 400,
                message: "bad schema".into(),
                retry_after: None,
            };
            assert!(!is_context_overflow(&plain_400));
        }

        /// INVARIANT (T2-16): a server-supplied `Retry-After` never sleeps the
        /// session for longer than `RETRY_AFTER_CAP`. `Retry-After: 3600` used
        /// to sleep an hour, escapable only by killing the process.
        #[test]
        fn retry_after_is_capped() {
            let hostile = ProviderError::Http {
                status: 429,
                message: String::new(),
                retry_after: Some(3600),
            };
            assert_eq!(
                classify(&hostile, 1),
                Decision::Retry {
                    delay: RETRY_AFTER_CAP
                }
            );
        }

        /// Backoff deepens for 529s: five attempts, base 1/2/4/8s.
        #[test]
        fn budget_is_deep_enough_for_an_overload() {
            let overload = ProviderError::Http {
                status: 529,
                message: String::new(),
                retry_after: None,
            };
            assert_eq!(MAX_ATTEMPTS, 5);
            for (attempt, secs) in [(1u32, 1u64), (2, 2), (3, 4), (4, 8)] {
                assert_eq!(
                    classify(&overload, attempt),
                    Decision::Retry {
                        delay: std::time::Duration::from_secs(secs)
                    },
                    "attempt {attempt}"
                );
            }
            assert_eq!(classify(&overload, MAX_ATTEMPTS), Decision::Fatal);
        }

        /// INVARIANT (T2-16): retries are jittered, so N processes backing off
        /// the same upstream do not return in lockstep.
        #[test]
        fn jitter_stays_in_range_and_actually_varies() {
            let base = std::time::Duration::from_secs(8);
            let mut seen = std::collections::HashSet::new();
            for _ in 0..500 {
                let d = with_jitter(base);
                assert!(d >= base / 2 && d <= base, "{d:?} outside [base/2, base]");
                seen.insert(d.as_millis());
            }
            assert!(
                seen.len() > 10,
                "jitter is not varying: {} values",
                seen.len()
            );
        }

        /// Both `Retry-After` forms parse. The date form silently fell back to
        /// exponential before R3.
        #[test]
        fn retry_after_parses_seconds_and_http_date() {
            // 2015-10-21T07:28:00Z
            const WHEN: u64 = 1_445_412_480;
            assert_eq!(parse_retry_after("120", WHEN), Some(120));
            assert_eq!(parse_retry_after("  120  ", WHEN), Some(120));
            assert_eq!(
                parse_retry_after("Wed, 21 Oct 2015 07:30:00 GMT", WHEN),
                Some(120)
            );
            // A date already in the past means "retry now".
            assert_eq!(
                parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", WHEN),
                Some(0)
            );
            assert_eq!(parse_retry_after("not a date", WHEN), None);
            // Leap-year and month-boundary sanity for the civil-days math.
            assert_eq!(
                parse_retry_after("Mon, 29 Feb 2016 00:00:01 GMT", 1_456_704_000),
                Some(1)
            );
        }

        #[test]
        fn classify_rules() {
            let overload = ProviderError::Http {
                status: 529,
                message: String::new(),
                retry_after: Some(7),
            };
            assert_eq!(
                classify(&overload, 1),
                Decision::Retry {
                    delay: std::time::Duration::from_secs(7)
                }
            );
            assert_eq!(classify(&overload, MAX_ATTEMPTS), Decision::Fatal);
            let auth = ProviderError::Auth("bad".into());
            assert_eq!(classify(&auth, 1), Decision::Fatal);
            assert!(!is_availability(&auth));
            let transport = ProviderError::Transport("reset".into());
            assert_eq!(
                classify(&transport, 2),
                Decision::Retry {
                    delay: std::time::Duration::from_secs(2)
                }
            );
            assert!(is_availability(&transport));
            let bad_req = ProviderError::Http {
                status: 400,
                message: String::new(),
                retry_after: None,
            };
            assert_eq!(classify(&bad_req, 1), Decision::Fatal);
        }
    }
}

/// The named cross-provider canonicalization stage (`transform_messages`).
/// Canonical assistant blocks are
/// Anthropic-shaped; when a request targets a *different* provider than the
/// one that produced a block, provider-bound reasoning must not cross.
pub mod transform {
    use serde_json::Value;

    /// Drop blocks that are provider-bound (signed/redacted thinking) when
    /// sending history to a foreign dialect. Text and tool_use always pass.
    pub fn strip_foreign_reasoning(blocks: &[Value]) -> Vec<Value> {
        blocks
            .iter()
            .filter(|b| {
                !matches!(
                    b.get("type").and_then(Value::as_str),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
            .cloned()
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn strips_thinking_keeps_rest() {
            let blocks = vec![
                json!({"type":"thinking","thinking":"x","signature":"s"}),
                json!({"type":"redacted_thinking","data":"d"}),
                json!({"type":"text","text":"hi"}),
                json!({"type":"tool_use","id":"1","name":"read","input":{}}),
            ];
            let out = strip_foreign_reasoning(&blocks);
            assert_eq!(out.len(), 2);
            assert_eq!(out[0]["type"], "text");
            assert_eq!(out[1]["type"], "tool_use");
        }
    }
}

/// Arg healing at the erasure boundary (M3a): streamed tool
/// arguments sometimes arrive as *almost*-JSON. Repair is conservative —
/// only unambiguous damage is fixed; anything else stays a parse error that
/// feeds back to the model as a tool result.
pub mod repair {
    use serde_json::Value;

    /// Strict parse, then repairs: strip trailing commas, then close
    /// truncated strings/objects/arrays (streams cut mid-argument).
    pub fn parse_or_repair(raw: &str) -> Option<Value> {
        if let Ok(v) = serde_json::from_str(raw) {
            return Some(v);
        }
        let without_commas = strip_trailing_commas(raw);
        if let Ok(v) = serde_json::from_str(&without_commas) {
            return Some(v);
        }
        serde_json::from_str(&close_truncation(&without_commas)).ok()
    }

    /// `{"a": 1,}` / `[1, 2,]` → valid. Only commas directly before a closer.
    fn strip_trailing_commas(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut in_string = false;
        let mut escaped = false;
        for c in s.chars() {
            if in_string {
                out.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                '}' | ']' => {
                    while out.ends_with(char::is_whitespace) || out.ends_with(',') {
                        if out.ends_with(',') {
                            out.pop();
                            break;
                        }
                        out.pop();
                    }
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// Close an unterminated string and any open brackets, in nesting order.
    fn close_truncation(s: &str) -> String {
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        for c in s.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' => {
                    stack.pop();
                }
                _ => {}
            }
        }
        let mut out = s.to_string();
        if in_string {
            out.push('"');
        }
        while let Some(closer) = stack.pop() {
            out.push(closer);
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn repairs_common_damage_and_rejects_garbage() {
            assert_eq!(
                parse_or_repair(r#"{"path": "a.rs"}"#).unwrap()["path"],
                "a.rs"
            );
            assert_eq!(
                parse_or_repair(r#"{"path": "a.rs",}"#).unwrap()["path"],
                "a.rs"
            );
            assert_eq!(
                parse_or_repair(r#"{"items": [1, 2,]}"#).unwrap()["items"][1],
                2
            );
            // Truncated mid-string (stream cut): closed and parsed.
            let v = parse_or_repair(r#"{"command": "cargo tes"#).unwrap();
            assert_eq!(v["command"], "cargo tes");
            // A comma inside a string is untouched.
            assert_eq!(parse_or_repair(r#"{"t": "a,}"}"#).unwrap()["t"], "a,}");
            // Escaped quotes don't confuse the scanner.
            let v = parse_or_repair(r#"{"t": "say \"hi\"",}"#).unwrap();
            assert_eq!(v["t"], "say \"hi\"");
            assert!(parse_or_repair("not json at all").is_none());
        }
    }
}

pub mod api_error;
pub mod catalog;
pub mod key;

/// Highest block index any assembler will materialize. Wire indices are
/// attacker-controlled `u64`s; without a bound, `blocks.resize(index + 1, …)`
/// is a one-field OOM (T2-9). Far above any real response.
pub const MAX_BLOCK_INDEX: usize = 512;

/// Wire-format folding, implemented per provider: turn one SSE `data:`
/// payload into events, and produce the terminal `Completed` at end-of-stream.
pub trait SseAssembler {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError>;
    fn finish(self) -> Result<StreamEvent, ProviderError>;
}

/// Drive an SSE byte stream through the line parser and an assembler.
/// Shared by every HTTP provider; no HTTP client dependency (the only
/// non-`std`/`futures` dep is `tokio::time`, which is wasm-supported).
///
/// INVARIANT (T1-4): a gap longer than `idle` between chunks ends the stream
/// with `ProviderError::Transport`. Enforced by
/// `drive_sse_tests::a_silent_stream_times_out_instead_of_hanging`.
pub fn drive_sse<B, E, A>(
    bytes: B,
    mut assembler: A,
    idle: std::time::Duration,
) -> impl futures_util::Stream<Item = Result<StreamEvent, ProviderError>>
where
    B: futures_util::Stream<Item = Result<bytes::Bytes, E>>,
    E: std::fmt::Display,
    A: SseAssembler,
{
    async_stream::stream! {
        let mut parser = SseParser::default();
        futures_util::pin_mut!(bytes);
        use futures_util::StreamExt;
        loop {
            let next = match tokio::time::timeout(idle, bytes.next()).await {
                Ok(next) => next,
                Err(_) => {
                    yield Err(ProviderError::Transport(format!(
                        "stream stalled: no data for {}s. The connection is likely dead \
                         (a proxy dropped it without closing); retry the request.",
                        idle.as_secs()
                    )));
                    return;
                }
            };
            let Some(chunk) = next else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield Err(ProviderError::Transport(format!("stream interrupted: {e}")));
                    return;
                }
            };
            let payloads = match parser.feed(&chunk) {
                Ok(payloads) => payloads,
                Err(e) => { yield Err(e); return; }
            };
            for data in payloads {
                match assembler.handle(&data) {
                    Ok(events) => for ev in events { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        match parser.finish() {
            Ok(payloads) => {
                for data in payloads {
                    match assembler.handle(&data) {
                        Ok(events) => for ev in events { yield Ok(ev); },
                        Err(e) => { yield Err(e); return; }
                    }
                }
            }
            Err(e) => { yield Err(e); return; }
        }
        yield assembler.finish();
    }
}

#[cfg(test)]
mod drive_sse_tests {
    use super::*;
    use futures_util::StreamExt;
    use std::time::Duration;

    struct NeverEnds;
    impl SseAssembler for NeverEnds {
        fn handle(&mut self, _: &str) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(vec![])
        }
        fn finish(self) -> Result<StreamEvent, ProviderError> {
            Err(ProviderError::Parse("unreachable".into()))
        }
    }

    /// INVARIANT (T1-4): a stream that goes silent is abandoned, never awaited
    /// forever. The real-world shape is a load balancer dropping the
    /// connection without a FIN — the bytes stream neither yields nor ends.
    #[tokio::test(start_paused = true)]
    async fn a_silent_stream_times_out_instead_of_hanging() {
        let stalled = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
        let s = drive_sse(stalled, NeverEnds, Duration::from_secs(5));
        futures_util::pin_mut!(s);
        let first = s.next().await.expect("an event, not a hang");
        match first {
            Err(ProviderError::Transport(m)) => {
                assert!(m.contains("stalled"), "{m}");
                assert!(m.contains("retry"), "errors are prompts: {m}");
            }
            other => panic!("expected a transport timeout, got {other:?}"),
        }
        assert!(
            s.next().await.is_none(),
            "the stream must end after the timeout"
        );
    }

    /// A stream that keeps delivering inside the idle window is never cut,
    /// however long it runs in total. This is why the bound is per-chunk and
    /// not a request-level `.timeout()`.
    #[tokio::test(start_paused = true)]
    async fn a_slow_but_live_stream_is_not_cut() {
        let chunks = futures_util::stream::unfold(0u32, |n| async move {
            if n == 6 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(4)).await;
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b":keepalive\n\n")),
                n + 1,
            ))
        });
        let s = drive_sse(chunks, NeverEnds, Duration::from_secs(5));
        futures_util::pin_mut!(s);
        let evs: Vec<_> = s.collect().await;
        // 24s of wall time, 5s idle bound, no timeout: only the assembler's
        // own end-of-stream error comes back.
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], Err(ProviderError::Parse(_))), "{evs:?}");
    }
}
