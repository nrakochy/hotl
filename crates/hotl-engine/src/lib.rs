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
use std::time::Duration;

use hotl_platform::Clock;
use hotl_provider::Provider;
use hotl_store::SessionLog;
use hotl_tools::{
    rules::{PermissionMode, Rules},
    Registry,
};
use hotl_types::{EntryPayload, Item, Todo, TokenUsage};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Re-exported so `hotl_engine::QuestionAnswer` resolves alongside
/// `EngineEvent::Question` — the type physically lives in hotl-types (shared
/// with hotl-tools's `QuestionSink`) to avoid a hotl-tools → hotl-engine
/// dependency cycle; see `question_sink`'s doc comment.
pub use hotl_types::QuestionAnswer;

/// Re-exported so `hotl_engine::NotificationKind` names the `Notification`
/// hook's kind without reaching into the `hooks` module — the type is
/// defined in `hooks.rs` (next to the trait method it parameterizes), not
/// here, to keep the event vocabulary and its dispatcher together.
pub use hooks::NotificationKind;

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
    /// A structured `ask_user` question (tier-1 gap #4) — NOT a permission
    /// gate: the reply is a plain-text tool result, never an authorization.
    /// Committed durably (`PendingQuestion`) before this event is sent; a
    /// dropped `reply` (headless/no-human) resolves to `QuestionAnswer::NoHuman`.
    Question {
        id: String,
        question: hotl_types::Question,
        reply: oneshot::Sender<hotl_types::QuestionAnswer>,
    },
    TurnDone {
        outcome: Outcome,
        usage: TokenUsage,
    },
    /// The `todo_write` checklist changed (a full-state replace committed).
    /// Ephemeral-context companion to the durable `Todos` entry: the surface
    /// (console strip, `hotl watch`) renders progress from this, never from
    /// parsing model text.
    TodosChanged {
        items: Vec<Todo>,
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
            Self::Question { question, .. } => write!(f, "Question({})", question.header),
            Self::TurnDone { outcome, .. } => write!(f, "TurnDone({outcome:?})"),
            Self::TodosChanged { items } => write!(f, "TodosChanged(n={})", items.len()),
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
    /// Set the session's display name (durable: appended to the log).
    Rename(String),
    /// Set the session's effective permission mode (durable: appended to the
    /// log as `ModeSet`; takes effect immediately — no `Rules` reallocation).
    SetMode(PermissionMode),
    /// Full-state replace of the `todo_write` checklist (durable: appended
    /// to the log as `Todos`, last-wins on replay — same shape as
    /// `Rename`/`SetMode`). The actor is the list's sole owner; the tool
    /// only ever forwards a validated `Vec<Todo>` here.
    SetTodos(Vec<Todo>),
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
    /// The todo checklist a resumed session starts with (the replayed
    /// session's last durable `Todos` entry — see `hotl_store::Replayed`).
    /// Empty for a fresh session. Seeds the actor's live `todos`, not
    /// `initial_items`: it never rode the projection, so it must not
    /// re-enter through it, and seeding here (vs. a post-spawn `SetTodos`)
    /// means resume never appends a duplicate `Todos` log entry.
    pub initial_todos: Vec<Todo>,
    pub config: EngineConfig,
}

pub struct SessionHandle {
    cmd: mpsc::Sender<SessionCmd>,
    pub events: mpsc::Receiver<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
    /// The session-scoped `notify` drain (Finding 1 fix) — the same instance
    /// the actor (and any `question_sink`) tracks detached `Notification`
    /// hook tasks in.
    notifications: hooks::NotificationDrain,
    /// The actor task itself. Kept (rather than discarded, as before) so a
    /// one-shot CLI exit path can wait for the actor to fully shut down —
    /// including its now-synchronous `SessionEnd` hook call (Finding 1) —
    /// instead of just dropping the handle and hoping the actor gets another
    /// scheduler turn before the runtime goes away.
    actor: tokio::task::JoinHandle<()>,
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
    /// Name the session durably (a `rename` log entry; last one wins).
    pub async fn rename(&self, name: String) {
        let _ = self.cmd.send(SessionCmd::Rename(name)).await;
    }
    /// Set the session's effective permission mode durably (a `mode_set` log
    /// entry; last one wins). Takes effect immediately: the running actor
    /// flips an atomic, it never reallocates `Rules`.
    pub async fn set_mode(&self, mode: PermissionMode) {
        let _ = self.cmd.send(SessionCmd::SetMode(mode)).await;
    }
    /// Full-state replace of the todo checklist (a durable `todos` log
    /// entry). Exposed mainly for tests that pre-seed a list; the real
    /// entry point is the `todo_write` tool's sink.
    pub async fn set_todos(&self, items: Vec<Todo>) {
        let _ = self.cmd.send(SessionCmd::SetTodos(items)).await;
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

    /// Bounded wait for every detached `Notification` hook task still in
    /// flight (Finding 1's `notify` fix): the one-shot CLI's `block_on` drops
    /// its `current_thread` runtime the instant its driving future resolves,
    /// which would otherwise silently kill a hook task mid-subprocess. A
    /// one-shot exit path should call this (or, more commonly,
    /// [`SessionHandle::finish`]) before returning; the long-lived
    /// TUI/interactive path never needs to — its runtime stays alive on its
    /// own, so an in-flight notification finishes naturally.
    pub async fn drain_notifications(&self, grace: Duration) {
        self.notifications.drain(grace).await;
    }

    /// The one-shot CLI's exit-time helper (Finding 1, both halves):
    /// consumes the handle, first draining in-flight `Notification` hook
    /// tasks (bounded by `grace`), then dropping this handle's strong
    /// command-channel sender and waiting — again bounded by `grace` — for
    /// the actor to fully shut down, which now runs its `SessionEnd` hook
    /// synchronously rather than as a detached task racing this same exit.
    /// Total worst case is `2 * grace`, never unbounded: a hung hook can
    /// delay the process's exit, but never wedge it.
    ///
    /// Call this (not a bare `drop(handle)`) right before a one-shot CLI
    /// function returns. The long-lived TUI/interactive/`hotl serve` paths
    /// must NOT call this — their runtime stays alive on its own, so both
    /// the notification and session-end hooks get to run naturally without
    /// this explicit wait.
    pub async fn finish(self, grace: Duration) {
        self.notifications.drain(grace).await;
        let SessionHandle { cmd, actor, .. } = self;
        drop(cmd);
        let _ = tokio::time::timeout(grace, actor).await;
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

/// A fresh, not-yet-consumed command channel for a session that doesn't
/// exist yet. Split out from [`spawn_session`] so a caller can build a tool
/// (`todo_write`) whose sink already holds a live sender to *this* session's
/// actor before the actor exists — the registry (and the deps built from
/// it) has to be assembled before `spawn_session` runs, which is otherwise a
/// chicken-and-egg with a command channel `spawn_session` creates internally.
pub fn session_channel() -> (mpsc::Sender<SessionCmd>, mpsc::Receiver<SessionCmd>) {
    mpsc::channel(64)
}

/// A fresh, not-yet-consumed event channel for a session that doesn't exist
/// yet — the events-side twin of [`session_channel`]. Split out so a caller
/// can build a tool (`ask_user`) whose sink already holds a live sender to
/// *this* session's own events stream before the actor exists, the same
/// chicken-and-egg [`session_channel`] solves for `SessionCmd`.
pub fn event_channel() -> (mpsc::Sender<EngineEvent>, mpsc::Receiver<EngineEvent>) {
    mpsc::channel(256)
}

pub fn spawn_session(deps: SessionDeps) -> SessionHandle {
    let (cmd_tx, cmd_rx) = session_channel();
    spawn_session_with(deps, cmd_tx, cmd_rx)
}

/// Spawn against a pre-created command channel (see [`session_channel`]);
/// builds its own event channel.
pub fn spawn_session_with(
    deps: SessionDeps,
    cmd_tx: mpsc::Sender<SessionCmd>,
    cmd_rx: mpsc::Receiver<SessionCmd>,
) -> SessionHandle {
    let (event_tx, event_rx) = event_channel();
    spawn_session_with_channels(
        deps,
        cmd_tx,
        cmd_rx,
        event_tx,
        event_rx,
        hooks::NotificationDrain::new(),
    )
}

/// Spawn against pre-created command *and* event channels (see
/// [`session_channel`]/[`event_channel`]) — what a caller needs when a
/// session-scoped tool's sink (`ask_user`) must hold live senders to both
/// before the actor exists.
pub fn spawn_session_with_channels(
    deps: SessionDeps,
    cmd_tx: mpsc::Sender<SessionCmd>,
    cmd_rx: mpsc::Receiver<SessionCmd>,
    event_tx: mpsc::Sender<EngineEvent>,
    event_rx: mpsc::Receiver<EngineEvent>,
    notifications: hooks::NotificationDrain,
) -> SessionHandle {
    let current_turn = Arc::new(Mutex::new(CancellationToken::new()));
    // The actor gets only a weak sender: strong senders are the handle and
    // any in-flight turn task, so dropping the handle lets the command
    // channel close and the actor task exit instead of leaking.
    let actor = tokio::spawn(actor::run(
        deps,
        cmd_rx,
        cmd_tx.downgrade(),
        event_tx,
        current_turn.clone(),
        notifications.clone(),
    ));
    SessionHandle {
        cmd: cmd_tx,
        events: event_rx,
        current_turn,
        notifications,
        actor,
    }
}

/// The production [`hotl_tools::ask::QuestionSink`] for `ask_user` (tier-1
/// gap #4): mirrors `Turn::ask` almost line-for-line, but runs from inside a
/// tool rather than `Turn` itself, so it reaches the actor through channels
/// instead of `self.propose`/`self.events` directly. Durably commits
/// `PendingQuestion` *before* surfacing (so a process that dies mid-question
/// leaves a dangling record replay can warn about, exactly like
/// `PendingAsk`), emits [`EngineEvent::Question`] carrying a fresh reply
/// channel, races the human's reply against the call's own cancellation
/// token (the same token `Turn::ask` races — an in-flight `ask_user` must
/// never outlive a turn the user already cancelled), then commits
/// `QuestionResolved`.
///
/// Captures only *weak* senders: this sink ends up owned by the tool
/// registry, which `SharedDeps` — and so the actor — holds for the whole
/// session. A strong sender captured here would be exactly the reference
/// cycle that made an early cut of `TodoWriteTool`'s sink leak the actor
/// task (`cmd_rx.recv()` never returns `None` while a strong sender lives
/// inside the very state the actor holds forever); see
/// `spawn_session_with_todos` for the sibling fix. An upgrade failure (the
/// handle/actor already gone) resolves to `NoHuman` — there is nobody left
/// to answer.
///
/// `hooks`/`notifications` (Finding 2 fix): this is the dominant "agent
/// needs input" surface — the exact signal `hotl watch` exists to catch —
/// but until now only `Turn::ask` (the permission-ask surface) fired
/// `Notification::Blocked`. The blocker cited when this was first built
/// (hooks unavailable at registry-build time) doesn't hold: `scaffold()`
/// loads hooks and completes before `spawn_session_with_todos`/this sink are
/// built, so the caller always has a `hooks` handle in scope — it just
/// wasn't threaded through. `notifications` must be the *same* drain the
/// session's actor was built with (Finding 1) so the CLI's exit-time drain
/// call also covers a `Blocked` notification fired from here.
pub fn question_sink(
    cmd_tx: mpsc::WeakSender<SessionCmd>,
    events_tx: mpsc::WeakSender<EngineEvent>,
    hooks_handle: Option<Arc<dyn hooks::Hooks>>,
    notifications: hooks::NotificationDrain,
) -> hotl_tools::ask::QuestionSink {
    std::sync::Arc::new(move |question, cancel| {
        let cmd_tx = cmd_tx.clone();
        let events_tx = events_tx.clone();
        let hooks_handle = hooks_handle.clone();
        let notifications = notifications.clone();
        Box::pin(async move {
            let id = hotl_types::new_ulid();
            propose_via(
                &cmd_tx,
                vec![EntryPayload::PendingQuestion {
                    id: id.clone(),
                    question: question.clone(),
                }],
            )
            .await;
            // Notification (Finding 2): the agent is blocked on a human at
            // the ask_user surface, mirroring `Turn::ask` — fire-and-forget,
            // right before the question actually surfaces.
            if let Some(h) = &hooks_handle {
                crate::hooks::notify(
                    h,
                    &notifications,
                    crate::hooks::NotificationKind::Blocked,
                    question.header.clone(),
                );
            }
            let answer = match events_tx.upgrade() {
                None => hotl_types::QuestionAnswer::NoHuman,
                Some(events) => {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if events
                        .send(EngineEvent::Question {
                            id: id.clone(),
                            question,
                            reply: reply_tx,
                        })
                        .await
                        .is_err()
                    {
                        hotl_types::QuestionAnswer::NoHuman
                    } else {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => hotl_types::QuestionAnswer::NoHuman,
                            reply = reply_rx => reply.unwrap_or(hotl_types::QuestionAnswer::NoHuman),
                        }
                    }
                }
            };
            propose_via(
                &cmd_tx,
                vec![EntryPayload::QuestionResolved {
                    id,
                    answer: hotl_tools::ask::format_answer(&answer),
                }],
            )
            .await;
            answer
        })
    })
}

/// Durable-append helper for [`question_sink`]: best-effort, like
/// `Turn::propose` — a sealed/gone log never blocks the question itself.
async fn propose_via(cmd_tx: &mpsc::WeakSender<SessionCmd>, entries: Vec<EntryPayload>) {
    let Some(tx) = cmd_tx.upgrade() else { return };
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx
        .send(SessionCmd::Propose {
            entries,
            reply: reply_tx,
        })
        .await
        .is_ok()
    {
        let _ = reply_rx.await;
    }
}
