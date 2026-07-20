//! L2 — the provider seam (system-design §L2).
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
use std::sync::Mutex;

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
    pub system: String,
    pub items: Vec<Item>,
    pub tools: Vec<ToolDef>,
    /// Adaptive thinking on models that support it.
    pub thinking: bool,
    /// M0 static cache placement: system block + latest user block
    /// (explicit-cache providers; system-design §L2 cache policy).
    pub cache_static: bool,
}

/// The unified, channel-tagged, block-structured event enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    Started,
    BlockStart { index: usize, kind: String },
    TextDelta { index: usize, text: String },
    ThinkingDelta { index: usize, text: String },
    ToolInputDelta { index: usize, json: String },
    BlockEnd { index: usize },
    Retrying { attempt: u32, reason: String },
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
    fn stream(&self, req: SamplingRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>>;
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
        Self { scripts: Mutex::new(scripts.into()), requests: Mutex::new(Vec::new()) }
    }

    pub fn requests(&self) -> Vec<SamplingRequest> {
        self.requests.lock().expect("requests mutex").clone()
    }

    /// Convenience: a one-sample script that answers with plain text.
    pub fn text_reply(text: &str) -> Vec<Result<StreamEvent, ProviderError>> {
        vec![
            Ok(StreamEvent::Started),
            Ok(StreamEvent::BlockStart { index: 0, kind: "text".into() }),
            Ok(StreamEvent::TextDelta { index: 0, text: text.into() }),
            Ok(StreamEvent::BlockEnd { index: 0 }),
            Ok(StreamEvent::Completed {
                stop: StopReason::EndTurn,
                usage: TokenUsage { input_tokens: 10, output_tokens: 5, ..Default::default() },
                blocks: vec![serde_json::json!({"type": "text", "text": text})],
            }),
        ]
    }

    /// Convenience: a sample that calls one tool.
    pub fn tool_call(id: &str, name: &str, input: Value) -> Vec<Result<StreamEvent, ProviderError>> {
        let block = serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input});
        vec![
            Ok(StreamEvent::Started),
            Ok(StreamEvent::BlockStart { index: 0, kind: "tool_use".into() }),
            Ok(StreamEvent::BlockEnd { index: 0 }),
            Ok(StreamEvent::Completed {
                stop: StopReason::ToolUse,
                usage: TokenUsage { input_tokens: 10, output_tokens: 8, ..Default::default() },
                blocks: vec![block],
            }),
        ]
    }
}

impl Provider for ScriptedProvider {
    fn stream(&self, req: SamplingRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        self.requests.lock().expect("requests mutex").push(req);
        let script = self
            .scripts
            .lock()
            .expect("scripted provider mutex")
            .pop_front()
            .unwrap_or_else(|| {
                vec![Err(ProviderError::Transport("scripted provider exhausted".into()))]
            });
        Box::pin(futures_util::stream::iter(script))
    }
}

/// SSE line parsing shared by HTTP providers: turns raw byte chunks into
/// complete `data:` payload strings. Chunks can split mid-line (and mid-UTF-8
/// code point), so bytes are buffered, not lossily decoded per chunk.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    /// Feed a chunk; returns complete `data:` payloads (`[DONE]` filtered).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
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
        }
        out
    }
}

/// Pure-data retry classification (RELIABILITY.md; corpus 06 — never regex
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
            ProviderError::Http { status, retry_after, .. } if *status == 429 || *status >= 500 => {
                Decision::Retry { after_secs: retry_after.unwrap_or(1u64 << (attempt - 1)) }
            }
            ProviderError::Transport(_) => Decision::Retry { after_secs: 1u64 << (attempt - 1) },
            _ => Decision::Fatal,
        }
    }

    /// Availability-class errors are the only ones that justify falling back
    /// to another model (never auth/billing/parse — corpus 12).
    pub fn is_availability(err: &ProviderError) -> bool {
        matches!(
            err,
            ProviderError::Http { status, .. } if *status == 429 || *status >= 500
        ) || matches!(err, ProviderError::Transport(_))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn classify_rules() {
            let overload = ProviderError::Http { status: 529, message: String::new(), retry_after: Some(7) };
            assert_eq!(classify(&overload, 1), Decision::Retry { after_secs: 7 });
            assert_eq!(classify(&overload, MAX_ATTEMPTS), Decision::Fatal);
            let auth = ProviderError::Auth("bad".into());
            assert_eq!(classify(&auth, 1), Decision::Fatal);
            assert!(!is_availability(&auth));
            let transport = ProviderError::Transport("reset".into());
            assert_eq!(classify(&transport, 2), Decision::Retry { after_secs: 2 });
            assert!(is_availability(&transport));
            let bad_req = ProviderError::Http { status: 400, message: String::new(), retry_after: None };
            assert_eq!(classify(&bad_req, 1), Decision::Fatal);
        }
    }
}

/// The named cross-provider canonicalization stage (system-design §L2
/// `transform_messages`, Pi corpus 08 Q4). Canonical assistant blocks are
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
            for data in parser.feed(&chunk) {
                match assembler.handle(&data) {
                    Ok(events) => for ev in events { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        yield assembler.finish();
    }
}
