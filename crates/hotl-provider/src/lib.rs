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
    pub items: Arc<Vec<Item>>,
    pub tools: Arc<[ToolDef]>,
    /// Adaptive thinking on models that support it.
    pub thinking: bool,
    /// M0 static cache placement: system block + latest user block
    /// (explicit-cache providers).
    pub cache_static: bool,
    /// MOIM (M2): ephemeral per-turn context, sent as a trailing user block
    /// after the cache marker. Never persisted — it exists only on the wire.
    pub turn_context: Option<String>,
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

#[derive(Debug, thiserror::Error)]
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
}

/// The honest "second impl" (D9): a scripted provider driving the real engine
/// in tests. Each `stream()` call pops the next script.
pub struct ScriptedProvider {
    scripts: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
    /// Every request the engine made, for test assertions on what the model saw.
    requests: Mutex<Vec<SamplingRequest>>,
}

impl ScriptedProvider {
    pub fn new(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
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
        Box::pin(futures_util::stream::iter(script))
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

/// SSE line parsing shared by HTTP providers: turns raw byte chunks into
/// complete `data:` payload strings. Chunks can split mid-line (and mid-UTF-8
/// code point), so bytes are buffered, not lossily decoded per chunk.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

/// A stream that never sends a newline must not buffer without bound.
const SSE_MAX_BUFFER: usize = 1024 * 1024;

impl SseParser {
    /// Feed a chunk; returns complete `data:` payloads (`[DONE]` filtered).
    /// Errors when a single line exceeds [`SSE_MAX_BUFFER`].
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<String>, ProviderError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[start..start + pos]);
            let line = line.trim_end_matches('\r');
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start();
                if !data.is_empty() && data != "[DONE]" {
                    out.push(data.to_string());
                }
            }
            start += pos + 1;
        }
        self.buf.drain(..start);
        if self.buf.len() > SSE_MAX_BUFFER {
            return Err(ProviderError::Parse(format!(
                "SSE line exceeded {SSE_MAX_BUFFER} bytes without a newline"
            )));
        }
        Ok(out)
    }
}

/// Pure-data retry classification (RELIABILITY.md — never regex
/// on prose). Both HTTP providers consult this; budgets reset per sample.
pub mod retry {
    use super::ProviderError;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Decision {
        /// Wait this many seconds, then retry the request.
        Retry { after_secs: u64 },
        /// Not recoverable by retrying (auth, parse, client errors).
        Fatal,
    }

    pub const MAX_ATTEMPTS: u32 = 3;

    /// `attempt` is 1-based (the attempt that just failed).
    pub fn classify(err: &ProviderError, attempt: u32) -> Decision {
        if attempt >= MAX_ATTEMPTS {
            return Decision::Fatal;
        }
        match err {
            ProviderError::Http {
                status,
                retry_after,
                ..
            } if *status == 429 || *status >= 500 => Decision::Retry {
                after_secs: retry_after.unwrap_or(1u64 << (attempt - 1)),
            },
            ProviderError::Transport(_) => Decision::Retry {
                after_secs: 1u64 << (attempt - 1),
            },
            _ => Decision::Fatal,
        }
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

        #[test]
        fn classify_rules() {
            let overload = ProviderError::Http {
                status: 529,
                message: String::new(),
                retry_after: Some(7),
            };
            assert_eq!(classify(&overload, 1), Decision::Retry { after_secs: 7 });
            assert_eq!(classify(&overload, MAX_ATTEMPTS), Decision::Fatal);
            let auth = ProviderError::Auth("bad".into());
            assert_eq!(classify(&auth, 1), Decision::Fatal);
            assert!(!is_availability(&auth));
            let transport = ProviderError::Transport("reset".into());
            assert_eq!(classify(&transport, 2), Decision::Retry { after_secs: 2 });
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

pub mod key;

/// Wire-format folding, implemented per provider: turn one SSE `data:`
/// payload into events, and produce the terminal `Completed` at end-of-stream.
pub trait SseAssembler {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError>;
    fn finish(self) -> Result<StreamEvent, ProviderError>;
}

/// Drive an SSE byte stream through the line parser and an assembler.
/// Shared by every HTTP provider; wasm-clean (no HTTP client dependency).
pub fn drive_sse<B, E, A>(
    bytes: B,
    mut assembler: A,
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
        while let Some(chunk) = bytes.next().await {
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
        yield assembler.finish();
    }
}
