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

impl ProviderError {
    /// M0 dumb retry cap: 429 / 5xx / transport are retryable, nothing else.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::Http { status, .. } => *status == 429 || *status >= 500,
            ProviderError::Transport(_) => true,
            _ => false,
        }
    }
}

pub trait Provider: Send + Sync {
    fn stream(&self, req: SamplingRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>>;
}

/// The honest "second impl" (D9): a scripted provider driving the real engine
/// in tests. Each `stream()` call pops the next script.
pub struct ScriptedProvider {
    scripts: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
}

impl ScriptedProvider {
    pub fn new(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Self {
        Self { scripts: Mutex::new(scripts.into()) }
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
    fn stream(&self, _req: SamplingRequest) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
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
