//! L3 — the turn engine, M1: actor + turn tasks (commit-protocol.md).
//!
//! One **session actor** per session is the sole committer to the log and the
//! owner of the projection ([`actor`]); **turn tasks** read actor-granted
//! snapshots at sample boundaries and *propose* entries ([`turn`]). Steers
//! admitted mid-turn are woven into the next sample (the conflict table's
//! rebase row); interrupts travel out-of-band via a shared token; permission
//! asks are events carrying a oneshot reply.

mod actor;
pub mod hooks;
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
    /// Reset-mode compaction (M4/#9): the continuation gets the preserved
    /// prefix + digest only, no verbatim tail — a fresh slate rather than a
    /// summarized-then-refilling window. Default false = M2 in-place behavior.
    pub compaction_reset: bool,
    /// Include `context_used%` in the MOIM turn-context block (M4/#9).
    /// Default true = M2 behavior; false to avoid inducing context anxiety.
    pub show_context_pct: bool,
    /// Evict a successful tool result larger than this (estimated tokens) to a
    /// masked blob, leaving a head preview + read pointer (T4). `0` disables.
    pub evict_threshold_tokens: u64,
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
            compaction_reset: false,
            show_context_pct: true,
            evict_threshold_tokens: 20_000,
        }
    }
}

/// How a turn task ended: with a user-facing outcome, or asking the actor
/// to compact and respawn a continuation (M2 mid-turn = terminate → compact
/// → respawn, per commit-protocol).
#[derive(Debug)]
pub enum TurnEnd {
    Outcome(Outcome),
    /// Compact, folding with the speculative digest when the turn managed to
    /// precompute one — `None` falls back to the inline summarize.
    Compact {
        spec: Option<SpecDigest>,
    },
}

/// A compaction digest computed speculatively *during* the turn, overlapping
/// the summarize call with the turn's own samples. Indices refer to the
/// projection the digest was planned against; the projection only appends
/// between folds, so they stay valid until the fold that consumes them.
#[derive(Debug)]
pub struct SpecDigest {
    pub prefix_end: usize,
    pub kept_from: usize,
    pub text: String,
}

/// A human's answer to a permission ask. Widened from a
/// bare `bool` so a denial can carry the reason to the model as tool-result
/// feedback — a steer fused with a "no". §2b (M4) extends this with
/// `AllowEdited`/`Respond`; callers should treat it as non-exhaustive.
#[derive(Debug, Clone, PartialEq)]
pub enum AskReply {
    Allow,
    Deny {
        message: Option<String>,
    },
    /// The human approved but rewrote the tool input (§2b).
    AllowEdited {
        input: serde_json::Value,
    },
    /// The human answered *as* the tool — skip execution, use this as the
    /// tool result (§2b).
    Respond {
        content: String,
    },
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
    ToolStart {
        name: String,
        summary: String,
    },
    ToolDone {
        name: String,
        ok: bool,
    },
    ToolDenied {
        name: String,
    },
    ToolAutoAllowed {
        name: String,
        rule: String,
    },
    Retrying {
        attempt: u32,
        reason: String,
    },
    FallbackModel {
        model: String,
    },
    PromptQueued,
    /// Context was compacted (digest + verbatim tail); `degraded` means the
    /// summarize call failed and the floor placeholder was used.
    Compacted {
        degraded: bool,
    },
    Ask {
        summary: String,
        protected_why: Option<String>,
        reply: oneshot::Sender<AskReply>,
    },
    TurnDone {
        outcome: Outcome,
        usage: TokenUsage,
    },
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
    /// A prompt whose committed item carries a provenance tag (T2: schema
    /// contract + validation-retry feedback ride in as tagged user items).
    PromptTagged {
        text: String,
        synthetic: hotl_types::SyntheticReason,
    },
    /// Continue an interrupted turn (M4/#8): sample against the current
    /// projection with no new user item — used on resume when the last item
    /// is a user/tool turn the model never answered. No-op if already running.
    Continue,
    /// Mid-turn guidance: admitted durably now, woven into the next sample.
    Steer(String),
    /// Turn task → actor: sample-boundary snapshot refresh.
    Snapshot {
        reply: oneshot::Sender<Arc<Vec<Item>>>,
    },
    /// Turn task → actor: commit these entries (durable-ack before reply).
    Propose {
        entries: Vec<EntryPayload>,
        reply: oneshot::Sender<bool>,
    },
    /// Turn task → actor: write an oversized tool result to a masked blob
    /// (T4 — the actor owns the log, the turn never touches it directly).
    /// Replies `Ok(path)` on success; on write failure the content is handed
    /// back in `Err` so eviction never loses data.
    WriteBlob {
        tool_use_id: String,
        content: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Turn task → actor: the turn is over (or needs a compaction respawn).
    TurnFinished { end: TurnEnd, usage: TokenUsage },
}

/// Workspace snapshots around mutating tool batches (M3b shadow-git).
/// Implementations run the actual snapshot off-thread; a slow or absent
/// snapshotter must never wedge the turn.
pub trait Snapshotter: Send + Sync {
    fn snapshot(&self, label: String) -> futures_util::future::BoxFuture<'static, ()>;
}

pub struct SessionDeps {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub rules: Arc<Rules>,
    /// Gates bash allow-rules: true only while the kernel write floor is
    /// enforced *and* any configured egress restriction is kernel-backed.
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub log: SessionLog,
    pub system: String,
    /// Working directory for subdir instruction hints (M2).
    pub cwd: PathBuf,
    /// Shadow snapshots (M3b); None = run without undo support.
    pub snapshots: Option<Arc<dyn Snapshotter>>,
    /// Extension hooks (M5); None = no hooks.
    pub hooks: Option<Arc<dyn hooks::Hooks>>,
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
    /// A prompt whose committed user item carries a provenance tag (T2).
    pub async fn prompt_tagged(&self, text: String, synthetic: hotl_types::SyntheticReason) {
        let _ = self
            .cmd
            .send(SessionCmd::PromptTagged { text, synthetic })
            .await;
    }
    pub async fn steer(&self, text: String) {
        let _ = self.cmd.send(SessionCmd::Steer(text)).await;
    }
    /// Continue an interrupted turn on resume (M4/#8).
    pub async fn continue_turn(&self) {
        let _ = self.cmd.send(SessionCmd::Continue).await;
    }
    /// Out-of-band interrupt of the in-flight turn (never queued behind data).
    pub fn interrupt(&self) {
        // A poisoned lock is fine: the token has no invariants to protect.
        self.current_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }
}

/// Whether a projection ends on the model's turn to speak (M4/#8): the last
/// item is a user prompt or a batch of tool results the model never answered
/// — i.e. an interrupted turn worth continuing on resume. A projection ending
/// in an assistant item (or holding only instructions) is complete.
pub fn needs_continuation(items: &[Item]) -> bool {
    matches!(
        items.last(),
        Some(Item::User { .. } | Item::ToolResults { .. })
    )
}

pub fn spawn_session(deps: SessionDeps) -> SessionHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let current_turn = Arc::new(Mutex::new(CancellationToken::new()));
    // The actor gets only a weak sender: strong senders are the handle and
    // any in-flight turn task, so dropping the handle lets the command
    // channel close and the actor task exit instead of leaking.
    tokio::spawn(actor::run(
        deps,
        cmd_rx,
        cmd_tx.downgrade(),
        event_tx,
        current_turn.clone(),
    ));
    SessionHandle {
        cmd: cmd_tx,
        events: event_rx,
        current_turn,
    }
}
