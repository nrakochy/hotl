//! L3 — the turn engine, M1: actor + turn tasks (commit-protocol.md).
//!
//! One **session actor** per session is the sole committer to the log and the
//! owner of the projection ([`actor`]); **turn tasks** read actor-granted
//! snapshots at sample boundaries and *propose* entries ([`turn`]). Steers
//! admitted mid-turn are woven into the next sample (the conflict table's
//! rebase row); interrupts travel out-of-band via a shared token; permission
//! asks are events carrying a oneshot reply.

mod actor;
mod turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hotl_platform::Clock;
use hotl_provider::Provider;
use hotl_store::SessionLog;
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, TokenUsage};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    pub thinking: bool,
    pub cache_static: bool,
    /// Availability-only fallback models (≤3 total — RELIABILITY.md).
    pub fallback_models: Vec<String>,
    /// Consecutive failures of one tool before the turn stops.
    pub tool_failure_budget: u32,
    /// Model context window in tokens; compaction triggers at 80% (M2).
    pub context_window: u64,
    /// Housekeeping model (compaction summarize); defaults to `model`.
    pub fast_model: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4-8".into(),
            max_tokens: 32_000,
            max_turns: 25,
            thinking: true,
            cache_static: true,
            fallback_models: Vec::new(),
            tool_failure_budget: 5,
            context_window: 200_000,
            fast_model: None,
        }
    }
}

/// How a turn task ended: with a user-facing outcome, or asking the actor
/// to compact and respawn a continuation (M2 mid-turn = terminate → compact
/// → respawn, per commit-protocol).
#[derive(Debug)]
pub enum TurnEnd {
    Outcome(Outcome),
    Compact,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done { text: String },
    Cancelled,
    TurnLimit,
    Refused,
    DoomLoop { pattern: String },
    ToolFailureBudget { tool: String },
    Error { message: String },
}

/// Everything the surface renders. `Ask` carries the reply channel — the
/// surface (or an allow-rule upstream) is the human on the loop.
pub enum EngineEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart { name: String, summary: String },
    ToolDone { name: String, ok: bool },
    ToolDenied { name: String },
    ToolAutoAllowed { name: String, rule: String },
    Retrying { attempt: u32, reason: String },
    FallbackModel { model: String },
    PromptQueued,
    /// Context was compacted (digest + verbatim tail); `degraded` means the
    /// summarize call failed and the floor placeholder was used (Sec #10).
    Compacted { degraded: bool },
    Ask { summary: String, protected_why: Option<String>, reply: oneshot::Sender<bool> },
    TurnDone { outcome: Outcome, usage: TokenUsage },
}

impl std::fmt::Debug for EngineEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TextDelta(t) => write!(f, "TextDelta({t:?})"),
            Self::ThinkingDelta(_) => write!(f, "ThinkingDelta"),
            Self::ToolStart { name, .. } => write!(f, "ToolStart({name})"),
            Self::ToolDone { name, ok } => write!(f, "ToolDone({name},{ok})"),
            Self::ToolDenied { name } => write!(f, "ToolDenied({name})"),
            Self::ToolAutoAllowed { name, rule } => write!(f, "ToolAutoAllowed({name},{rule})"),
            Self::Retrying { attempt, .. } => write!(f, "Retrying({attempt})"),
            Self::FallbackModel { model } => write!(f, "FallbackModel({model})"),
            Self::PromptQueued => write!(f, "PromptQueued"),
            Self::Compacted { degraded } => write!(f, "Compacted({degraded})"),
            Self::Ask { summary, .. } => write!(f, "Ask({summary})"),
            Self::TurnDone { outcome, .. } => write!(f, "TurnDone({outcome:?})"),
        }
    }
}

pub enum SessionCmd {
    /// A user prompt. Starts a turn, or queues (one-at-a-time promotion).
    Prompt(String),
    /// Mid-turn guidance: admitted durably now, woven into the next sample.
    Steer(String),
    /// Turn task → actor: sample-boundary snapshot refresh.
    Snapshot { reply: oneshot::Sender<Arc<Vec<Item>>> },
    /// Turn task → actor: commit these entries (durable-ack before reply).
    Propose { entries: Vec<EntryPayload>, reply: oneshot::Sender<bool> },
    /// Turn task → actor: the turn is over (or needs a compaction respawn).
    TurnFinished { end: TurnEnd, usage: TokenUsage },
}

pub struct SessionDeps {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub rules: Arc<Rules>,
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub log: SessionLog,
    pub system: String,
    /// Working directory for subdir instruction hints (M2).
    pub cwd: PathBuf,
    pub initial_items: Vec<Item>,
    pub config: EngineConfig,
}

pub struct SessionHandle {
    cmd: mpsc::Sender<SessionCmd>,
    pub events: mpsc::Receiver<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
}

impl SessionHandle {
    pub async fn prompt(&self, text: String) {
        let _ = self.cmd.send(SessionCmd::Prompt(text)).await;
    }
    pub async fn steer(&self, text: String) {
        let _ = self.cmd.send(SessionCmd::Steer(text)).await;
    }
    /// Out-of-band interrupt of the in-flight turn (never queued behind data).
    pub fn interrupt(&self) {
        self.current_turn.lock().expect("turn token mutex").cancel();
    }
}

pub fn spawn_session(deps: SessionDeps) -> SessionHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let current_turn = Arc::new(Mutex::new(CancellationToken::new()));
    tokio::spawn(actor::run(deps, cmd_rx, cmd_tx.clone(), event_tx, current_turn.clone()));
    SessionHandle { cmd: cmd_tx, events: event_rx, current_turn }
}
