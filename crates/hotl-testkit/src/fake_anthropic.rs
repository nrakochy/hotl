//! A scripted Anthropic SSE server on loopback (0074 T2): replays frames with
//! per-frame delays, routes a request by its body, carries the 5m/1h cache
//! usage fields, and records every request body. Hand-rolled HTTP/1.1 on
//! `tokio::net::TcpListener`, like the provider crates' own doubles — no HTTP
//! crate, no network beyond `127.0.0.1`, no credentials.
//!
//! The wire is built by hand, not by round-tripping the parser's types: a
//! fake that shares the parser's types cannot catch the parser misreading
//! the wire. The real parser is the oracle in this module's tests only.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One SSE frame: `event: <event>\ndata: <json>\n\n`, written after `delay`.
#[derive(Clone, Debug)]
pub struct Frame {
    pub event: &'static str,
    pub data: Value,
    pub delay: Duration,
}

/// What `message_start`/`message_delta` report. Zeros when unspecified; the
/// flat `cache_creation_input_tokens` is always the two buckets' sum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
}

#[derive(Clone, Debug)]
enum Block {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
        /// `Some((chunk_bytes, per_chunk_delay))` streams the input JSON in
        /// pieces — the 0062/184 shape; `None` sends it in one delta.
        chunked: Option<(usize, Duration)>,
    },
}

/// One assistant message the fake will stream. Build with [`text`],
/// [`thinking`], [`tool_use`], [`tool_use_streamed`] or [`batch`].
#[derive(Clone, Debug)]
pub struct Message {
    blocks: Vec<Block>,
    usage: Usage,
    /// Delay before the first frame (a slow model, a child that takes 3 s).
    first_delay: Duration,
    /// Delay before every frame.
    pace: Duration,
}

impl Message {
    fn new(block: Block) -> Self {
        Self {
            blocks: vec![block],
            usage: Usage::default(),
            first_delay: Duration::ZERO,
            pace: Duration::ZERO,
        }
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Wait this long before the first frame.
    pub fn after(mut self, delay: Duration) -> Self {
        self.first_delay = delay;
        self
    }

    /// Wait this long before every frame.
    pub fn paced(mut self, delay: Duration) -> Self {
        self.pace = delay;
        self
    }

    /// Deterministic ids and one delta per block: transcripts stay stable.
    pub fn frames(&self) -> Vec<Frame> {
        let mut out = Vec::new();
        let mut push = |event: &'static str, data: Value, delay: Duration| {
            out.push(Frame { event, data, delay });
        };
        let stop = if self
            .blocks
            .iter()
            .any(|b| matches!(b, Block::ToolUse { .. }))
        {
            "tool_use"
        } else {
            "end_turn"
        };
        push(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_smoke",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-smoke",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.usage.input,
                        "cache_read_input_tokens": self.usage.cache_read,
                        "cache_creation_input_tokens": self.usage.cache_5m + self.usage.cache_1h,
                        "cache_creation": {
                            "ephemeral_5m_input_tokens": self.usage.cache_5m,
                            "ephemeral_1h_input_tokens": self.usage.cache_1h
                        },
                        "output_tokens": 0
                    }
                }
            }),
            self.first_delay + self.pace,
        );
        for (index, block) in self.blocks.iter().enumerate() {
            match block {
                Block::Text(t) => {
                    push(
                        "content_block_start",
                        json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
                        self.pace,
                    );
                    push(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": t}}),
                        self.pace,
                    );
                }
                Block::Thinking(t) => {
                    push(
                        "content_block_start",
                        json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": ""}}),
                        self.pace,
                    );
                    push(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": t}}),
                        self.pace,
                    );
                    push(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": "sig_smoke"}}),
                        self.pace,
                    );
                }
                Block::ToolUse {
                    id,
                    name,
                    input,
                    chunked,
                } => {
                    push(
                        "content_block_start",
                        json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
                        self.pace,
                    );
                    let text = input.to_string();
                    match chunked {
                        Some((bytes, delay)) => {
                            let mut rest = text.as_str();
                            while !rest.is_empty() {
                                let mut cut = (*bytes).min(rest.len());
                                while !rest.is_char_boundary(cut) {
                                    cut -= 1;
                                }
                                let (head, tail) = rest.split_at(cut);
                                push(
                                    "content_block_delta",
                                    json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": head}}),
                                    *delay + self.pace,
                                );
                                rest = tail;
                            }
                        }
                        None => push(
                            "content_block_delta",
                            json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": text}}),
                            self.pace,
                        ),
                    }
                }
            }
            push(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
                self.pace,
            );
        }
        push(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null}, "usage": {"output_tokens": self.usage.output}}),
            self.pace,
        );
        push("message_stop", json!({"type": "message_stop"}), self.pace);
        out
    }
}

pub fn text(s: &str) -> Message {
    Message::new(Block::Text(s.into()))
}

pub fn thinking(s: &str) -> Message {
    Message::new(Block::Thinking(s.into()))
}

pub fn tool_use(id: &str, name: &str, input: Value) -> Message {
    Message::new(Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
        chunked: None,
    })
}

/// The input JSON arrives in `chunk_bytes` pieces, `per_chunk` apart: a 12 KB
/// input at 1 KB per second is a 12 s write.
pub fn tool_use_streamed(
    id: &str,
    name: &str,
    input: Value,
    chunk_bytes: usize,
    per_chunk: Duration,
) -> Message {
    Message::new(Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
        chunked: Some((chunk_bytes.max(1), per_chunk)),
    })
}

/// Several blocks in one message (a parallel tool batch). Usage and delays
/// come from the first.
pub fn batch(messages: Vec<Message>) -> Message {
    let mut iter = messages.into_iter();
    let mut first = iter.next().expect("a batch has at least one message");
    for m in iter {
        first.blocks.extend(m.blocks);
    }
    first
}

/// What one request gets back.
#[derive(Clone, Debug)]
pub enum Reply {
    Stream(Message),
    /// An Anthropic error envelope with this status, for retry paths.
    HttpError {
        status: u16,
        kind: String,
    },
}

impl From<Message> for Reply {
    fn from(m: Message) -> Self {
        Reply::Stream(m)
    }
}

pub fn http_error(status: u16, kind: &str) -> Reply {
    Reply::HttpError {
        status,
        kind: kind.into(),
    }
}

/// How a route picks its request.
#[derive(Clone, Debug)]
pub enum Match {
    BodyContains(String),
}

/// Routes first (consumed on first match, so a parent's follow-up that quotes
/// a child's text falls through), then the queue by ordinal, then `default`.
#[derive(Clone, Debug)]
pub struct Script {
    pub routes: Vec<(Match, Reply)>,
    pub queue: VecDeque<Reply>,
    pub default: Reply,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            routes: Vec::new(),
            queue: VecDeque::new(),
            default: text("ok").into(),
        }
    }
}

impl Script {
    pub fn queue(replies: Vec<Reply>) -> Self {
        Self {
            queue: replies.into(),
            ..Self::default()
        }
    }

    pub fn route(mut self, needle: &str, reply: impl Into<Reply>) -> Self {
        self.routes
            .push((Match::BodyContains(needle.into()), reply.into()));
        self
    }

    pub fn then(mut self, reply: impl Into<Reply>) -> Self {
        self.queue.push_back(reply.into());
        self
    }

    fn pick(&mut self, body: &str) -> Reply {
        if let Some(i) = self.routes.iter().position(|(m, _)| match m {
            Match::BodyContains(s) => body.contains(s.as_str()),
        }) {
            return self.routes.remove(i).1;
        }
        self.queue
            .pop_front()
            .unwrap_or_else(|| self.default.clone())
    }
}

struct State {
    script: Script,
    bodies: Vec<String>,
}

/// The running server. Dropping it stops the accept loop.
pub struct FakeAnthropic {
    base: String,
    state: Arc<Mutex<State>>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for FakeAnthropic {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl FakeAnthropic {
    /// Bind `127.0.0.1:0` and serve on the current runtime.
    pub async fn start(script: Script) -> FakeAnthropic {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let state = Arc::new(Mutex::new(State {
            script,
            bodies: Vec::new(),
        }));
        let st = Arc::clone(&state);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_one(sock, Arc::clone(&st)));
            }
        });
        FakeAnthropic {
            base,
            state,
            accept,
        }
    }

    /// `http://127.0.0.1:PORT` — what `[provider] base_url` takes.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Every raw request body, in arrival order.
    pub fn bodies(&self) -> Vec<String> {
        self.state.lock().expect("fake mutex").bodies.clone()
    }

    /// The bodies, parsed.
    pub fn requests(&self) -> Vec<Value> {
        self.bodies()
            .iter()
            .filter_map(|b| serde_json::from_str(b).ok())
            .collect()
    }

    pub fn request_count(&self) -> usize {
        self.state.lock().expect("fake mutex").bodies.len()
    }
}

async fn serve_one(mut sock: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
    // Read to the header break, take content-length, read exactly that many.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let body_at = loop {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..body_at]).to_string();
    // The provider warms its connection with a bare GET (`Warmable::arm`);
    // only a POST is a sample — anything else gets an empty 204 and is never
    // recorded, so it cannot eat the queue's next reply.
    if !head.starts_with("POST ") {
        let _ = sock
            .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
            .await;
        let _ = sock.shutdown().await;
        return;
    }
    let len: usize = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split_once(':'))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < body_at + len {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_at..]).to_string();
    let reply = {
        let mut st = state.lock().expect("fake mutex");
        st.bodies.push(body.clone());
        st.script.pick(&body)
    };
    match reply {
        Reply::HttpError { status, kind } => {
            let payload = json!({"type": "error", "error": {"type": kind, "message": format!("fake: {kind}")}}).to_string();
            let head = format!(
                "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                payload.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(payload.as_bytes()).await;
        }
        Reply::Stream(message) => {
            // No content-length: the body ends when the socket closes, so each
            // frame can be written and flushed after its own delay.
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                .await;
            for frame in message.frames() {
                if !frame.delay.is_zero() {
                    tokio::time::sleep(frame.delay).await;
                }
                let line = format!("event: {}\ndata: {}\n\n", frame.event, frame.data);
                if sock.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
            }
        }
    }
    let _ = sock.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use hotl_provider::{Provider, SamplingRequest, StreamEvent};
    use hotl_provider_anthropic::AnthropicProvider;
    use hotl_types::{Item, StopReason};

    fn provider(base: &str) -> AnthropicProvider {
        AnthropicProvider::new(Arc::new(hotl_provider::key::StaticKey(Some("x".into()))))
            .with_base_url(base)
    }

    fn req(text: &str) -> SamplingRequest {
        SamplingRequest {
            model: "claude-smoke".into(),
            max_tokens: 64,
            system: "sys".into(),
            items: hotl_provider::arc_items(vec![Item::User {
                text: text.into(),
                synthetic: None,
                images: Vec::new(),
            }]),
            ephemeral_tail: Arc::new(Vec::new()),
            tools: Arc::from(Vec::new()),
            thinking: false,
            effort: None,
            cache: hotl_provider::CachePolicy::Off,
            turn_context: None,
            cache_key: None,
            output_format: None,
        }
    }

    async fn completed(base: &str, text: &str) -> (StopReason, hotl_types::TokenUsage, Vec<Value>) {
        let events: Vec<_> = provider(base).stream(req(text)).collect().await;
        for e in &events {
            assert!(e.is_ok(), "{events:?}");
        }
        events
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Completed {
                    stop,
                    usage,
                    blocks,
                }) => Some((stop, usage, blocks)),
                _ => None,
            })
            .expect("a Completed event")
    }

    #[tokio::test]
    async fn fake_frames_parse_through_the_real_sse_assembler() {
        let fake = FakeAnthropic::start(Script::queue(vec![
            text("hello there").into(),
            tool_use("t1", "read", json!({"path": "a.rs"})).into(),
            thinking("hmm").into(),
            batch(vec![
                tool_use("t1", "spawn", json!({"task": "A"})),
                tool_use("t2", "spawn", json!({"task": "B"})),
            ])
            .into(),
            tool_use_streamed(
                "t9",
                "write",
                json!({"path": "big.txt", "content": "x".repeat(3000)}),
                1024,
                Duration::ZERO,
            )
            .into(),
        ]))
        .await;
        let (stop, _, blocks) = completed(fake.base_url(), "one").await;
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello there");

        let (stop, _, blocks) = completed(fake.base_url(), "two").await;
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "t1");
        assert_eq!(blocks[0]["input"]["path"], "a.rs");

        let (_, _, blocks) = completed(fake.base_url(), "three").await;
        assert_eq!(blocks[0]["type"], "thinking");

        let (stop, _, blocks) = completed(fake.base_url(), "four").await;
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert_eq!(blocks[1]["id"], "t2");

        let (_, _, blocks) = completed(fake.base_url(), "five").await;
        assert_eq!(
            blocks[0]["input"]["content"].as_str().map(str::len),
            Some(3000),
            "chunks reassemble"
        );
    }

    #[tokio::test]
    async fn fake_routes_a_child_request_by_body() {
        let fake = FakeAnthropic::start(
            Script::queue(vec![text("from the queue").into()]).route("child-A", text("routed")),
        )
        .await;
        // The child's own request carries its task text: routed, and the
        // route is consumed.
        let (_, _, b) = completed(fake.base_url(), "task: child-A").await;
        assert_eq!(b[0]["text"], "routed");
        // The parent's follow-up quotes the child's text: falls through.
        let (_, _, b) = completed(fake.base_url(), "results: child-A said routed").await;
        assert_eq!(b[0]["text"], "from the queue");
        // Both exhausted: the default.
        let (_, _, b) = completed(fake.base_url(), "anything").await;
        assert_eq!(b[0]["text"], "ok");
    }

    #[tokio::test]
    async fn fake_paces_frames_by_delay() {
        let fake = FakeAnthropic::start(Script::queue(vec![text("slow")
            .paced(Duration::from_millis(60))
            .into()]))
        .await;
        let started = std::time::Instant::now();
        let (_, _, b) = completed(fake.base_url(), "x").await;
        let took = started.elapsed();
        assert_eq!(b[0]["text"], "slow");
        // Six frames at 60 ms: at least 300 ms, and well under the 2 s a slow
        // box would still pass.
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
    }

    #[tokio::test]
    async fn fake_usage_carries_both_cache_ttls() {
        let fake = FakeAnthropic::start(Script::queue(vec![text("u")
            .with_usage(Usage {
                input: 20,
                output: 9,
                cache_read: 300,
                cache_5m: 150,
                cache_1h: 300,
            })
            .into()]))
        .await;
        let (_, usage, _) = completed(fake.base_url(), "x").await;
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_creation_input_tokens, 450, "{usage:?}");
        let frames = text("u")
            .with_usage(Usage {
                cache_5m: 150,
                cache_1h: 300,
                ..Default::default()
            })
            .frames();
        let start = &frames[0].data["message"]["usage"];
        assert_eq!(start["cache_creation"]["ephemeral_5m_input_tokens"], 150);
        assert_eq!(start["cache_creation"]["ephemeral_1h_input_tokens"], 300);
    }

    #[tokio::test]
    async fn fake_records_every_body_in_order() {
        let fake = FakeAnthropic::start(Script::default()).await;
        completed(fake.base_url(), "first").await;
        completed(fake.base_url(), "second").await;
        assert_eq!(fake.request_count(), 2);
        let reqs = fake.requests();
        assert_eq!(reqs[0]["model"], "claude-smoke");
        assert!(fake.bodies()[0].contains("first") && fake.bodies()[1].contains("second"));
    }

    #[tokio::test]
    async fn fake_http_error_drives_the_retry_path() {
        let fake = FakeAnthropic::start(Script::queue(vec![
            http_error(529, "overloaded_error"),
            text("after").into(),
        ]))
        .await;
        let events: Vec<_> = provider(fake.base_url()).stream(req("x")).collect().await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                Ok(StreamEvent::Retrying {
                    status: Some(529),
                    ..
                })
            )),
            "{events:?}"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, Ok(StreamEvent::Completed { .. }))));
        assert_eq!(fake.request_count(), 2);
    }
}
