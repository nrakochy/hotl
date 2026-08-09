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
mod ledger;
mod turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hotl_platform::Clock;
use hotl_provider::{CacheTtl, Provider};
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

/// Re-exported so `hotl_engine::LedgerSummary` resolves alongside
/// `EngineEvent::LedgerReport` — the loop-overhead instrument (§S1) lives in
/// its own module ([`ledger`]) since it is self-contained (no dependency on
/// the rest of the engine's types) and independently unit-tested.
pub use ledger::{LedgerSummary, Phase, PhaseDeltaSummary};

/// Re-exported so `hotl_engine::ProjectionHead` names what
/// [`SessionHandle::head`] hands out — the epoch-fenced published projection
/// (commit-protocol.md §Read invariant). It lives in [`actor`], next to the
/// only thing that may publish it.
pub use actor::ProjectionHead;

/// Re-exported alongside [`ProjectionHead`]: what a read of it yields, split
/// into the durable projection and the ephemeral per-sample tail. Out-of-crate
/// readers (`fork`'s history seed) name it to say which half they take.
pub use actor::Snapshot;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub model: String,
    pub max_tokens: u32,
    /// Model samples one prompt may spend before the turn is cut short with
    /// [`Outcome::TurnLimit`]. Every tool round-trip costs one, so this is a
    /// step budget for the agent loop, not a count of conversational turns.
    ///
    /// **Negative = unlimited**: the turn runs until the model stops on its
    /// own, the context fills, a tool budget trips, or the user cancels. That
    /// is a deliberate opt-in — the bound is what keeps an unattended
    /// (`Auto`/`DontAsk`) run from looping on the owner's money, so removing
    /// it should be a choice, never a default.
    pub max_turns: i64,
    pub thinking: bool,
    pub cache_static: bool,
    /// The lifetime `compose_request` asks explicit-cache breakpoints for
    /// when `cache_static` is set (`CachePolicy::Static { prefix_ttl }` —
    /// consumed by the Anthropic serializer's prefix and rolling-anchor
    /// markers; the latest marker always renders plain regardless). Default
    /// `FiveMinutes`; long-lived human-supervised surfaces (`hotl tui`,
    /// `hotl acp`, `hotl bg`/attach) raise it to `OneHour` after `scaffold()`
    /// returns, and sub-agent children pin it back to `FiveMinutes`
    /// explicitly in `HotlChildBuilder::spawn_child`.
    pub cache_ttl: CacheTtl,
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
    /// Which [`AckMode`] turn-originated proposals use where the protocol
    /// allows pipelining (commit-protocol.md §Pipelined commits). Production
    /// is `Pipelined`; `Sync` exists so a golden scenario can drive the same
    /// session both ways and compare normalized transcripts — the revision's
    /// own counter assertion. Same discipline as
    /// `hotl_store::SessionLog::set_sync_noop`: a runtime seam, hidden from
    /// the public API, never a cargo feature.
    #[doc(hidden)]
    pub ack_mode: AckMode,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4-8".into(),
            max_tokens: 32_000,
            // Roomy enough that ordinary agentic work (long edit/test/fix
            // chains) finishes inside it — the cap is a runaway backstop, not
            // a work ceiling. Sub-agent call sites set their own, tighter.
            max_turns: 100,
            thinking: true,
            cache_static: true,
            cache_ttl: CacheTtl::FiveMinutes,
            fallback_models: Vec::new(),
            tool_failure_budget: 5,
            context_window: 200_000,
            fast_model: None,
            compaction_reset: false,
            show_context_pct: true,
            evict_threshold_tokens: 20_000,
            ack_mode: AckMode::Pipelined,
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
    /// precompute one — `None` falls back to the inline summarize. `cont`
    /// carries the per-turn counters the respawn must not reset (boxed so the
    /// variant stays small).
    Compact {
        spec: Option<SpecDigest>,
        cont: Box<TurnContinuation>,
    },
}

/// The per-turn state a compaction respawn must NOT reset (T2-2). A fold is
/// "no new user item, same logical turn" — so every counter that bounds that
/// turn has to cross it. Reconstructing a `Turn` from `Default` here is what let
/// `max_turns` be defeated by the very scenario it exists for.
/// INVARIANT: every per-turn safety counter survives a compaction respawn.
/// Enforced by `max_turns_is_enforced_across_a_compaction` and
/// `three_folds_with_progress_do_not_exhaust_the_streak`.
#[derive(Debug, Default)]
pub struct TurnContinuation {
    /// Steps already spent against [`EngineConfig::max_turns`].
    pub(crate) spent: i64,
    /// Fallback-model position: a continuation does not silently revert to the
    /// primary model that just failed.
    pub(crate) model_idx: usize,
    /// The doom detector's trailing signature window.
    pub(crate) call_sigs: std::collections::VecDeque<crate::turn::CallSig>,
    /// Per-tool consecutive failures (the tool-failure budget).
    pub(crate) consecutive_failures: std::collections::HashMap<String, u32>,
    /// The shared per-prompt "reminder and continue" budget.
    pub(crate) turn_extensions: u32,
    /// Completed samples since the last fold — the compaction streak's
    /// "intervening completed sample" (T2-3). Read by `actor::try_compact`;
    /// a fresh continuation restarts the count at zero.
    pub(crate) samples_since_compact: u32,
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
    /// The human approved *and* lifted the credential read-deny for this one
    /// command (plan 0022). Never reachable headless or from a sub-agent, and
    /// scoped to the single `Tool::run` future so it cannot outlive the call.
    AllowWithSecretReads,
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
    /// Loop-overhead instrument (§S1), flushed once when the turn task ends.
    /// UI/telemetry only — this NEVER becomes a session-log entry, so it
    /// cannot perturb golden-transcript normalization.
    LedgerReport(LedgerSummary),
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
            Self::LedgerReport(s) => write!(f, "LedgerReport(samples={})", s.sample_count),
        }
    }
}

/// One entry a turn task proposes to the actor, already serialized and
/// masked (commit-protocol.md §Proposal payloads): the actor's disk-write
/// path ([`hotl_store::SessionLog::append_prepared`]) never touches
/// `EntryPayload` again. `item` carries the typed value for entries that
/// also live in the model-visible projection — the actor still needs it to
/// update `SessionCmd::Snapshot`'s answer, and keeping it here is not a
/// second serialization: it is the exact value the turn already built in
/// memory to produce `payload`, never re-parsed from `payload`'s bytes
/// (that would just move T3-16's per-entry cost back onto the actor rather
/// than delete it). `None` for entries that never enter the projection
/// (`Usage`, `PendingAsk`/`AskResolved`).
///
/// Fields are private: `payload` and `item` are two independently-built
/// views of the same logical entry, and nothing about the types alone
/// guarantees they agree. [`PreparedEntry::new`] is the only constructor —
/// it debug-asserts that `item`'s presence matches `payload.kind()`, so a
/// future call site that passes a mismatched pair (wrong variable, copied
/// from a different entry) fails loudly in tests/dev builds instead of
/// silently diverging the projection from the log.
pub struct PreparedEntry {
    payload: hotl_store::PreparedPayload,
    item: Option<Item>,
}

impl PreparedEntry {
    pub fn new(payload: hotl_store::PreparedPayload, item: Option<Item>) -> Self {
        debug_assert_eq!(
            item.is_some(),
            matches!(payload.kind(), hotl_store::EntryKind::Item),
            "PreparedEntry::new: item's presence must match payload.kind() == EntryKind::Item"
        );
        Self { payload, item }
    }

    pub fn payload(&self) -> &hotl_store::PreparedPayload {
        &self.payload
    }

    pub fn item(&self) -> Option<&Item> {
        self.item.as_ref()
    }

    /// Consume the entry: the actor's commit loop needs to move `payload`
    /// into `SessionLog::append_prepared` and, separately, `item` (if any)
    /// into the live projection.
    pub fn into_parts(self) -> (hotl_store::PreparedPayload, Option<Item>) {
        (self.payload, self.item)
    }
}

/// Whether the sample that produced a proposal had **closed** by the time
/// the proposal was made — declared by the proposer, because only the turn
/// knows. The actor never stores it as state and never routes on it; it
/// exists so the held-steer release can *check* the argument it rests on
/// instead of resting on a comment (commit-protocol.md §Read invariant, and
/// the 72a6f1b held-steer rule).
///
/// The argument, stated: a steer held while a turn is live may land the
/// moment one of that turn's commits settles, because a turn commits nothing
/// between granting itself a snapshot and the `Completed` group that closes
/// the sample — so every ack the actor handles is genuinely between samples.
/// That is true of every proposal site today and it is exactly what
/// [`SampleStage::InSample`] exists to catch when it stops being true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleStage {
    /// The sample that produced this commit had already closed (or none was
    /// running). A held steer may land behind it: the model's reply is
    /// already durable, so nothing can precede an assistant item that could
    /// not have seen it.
    AtBoundary,
    /// The commit lands **while its own sample is still streaming**. No such
    /// site exists today; §Commit granularity's intra-sample `BlockEnd`
    /// pipelining would create the first, and on that day a steer released
    /// behind one would land ahead of the assistant item the model is still
    /// producing — the exact inversion 72a6f1b fixed. Declaring this is what
    /// makes that a loud failure rather than a silent regression.
    InSample,
}

/// A batch of entries a turn task asks the actor to commit
/// (commit-protocol.md §Vocabulary), in the two shapes the protocol names.
/// Both answer with exactly one [`CommitTicket`] in [`AckMode::Pipelined`];
/// they differ in what reaches the writer.
pub enum EntryProposal {
    Single(PreparedEntry),
    /// Entries that are **one causal event** (§Causal groups): the actor
    /// chains them parent→child inside the group and sends one writer
    /// message, which does one `write_all`, one `sync_data` and resolves one
    /// ticket. The projection applies the whole group or none of it.
    Group(Vec<PreparedEntry>),
}

impl EntryProposal {
    /// The shape `entries` calls for. A turn only ever proposes several
    /// entries *together* when they are one causal event — the `Completed`
    /// pair, or a tool-results batch with the subdir instructions that batch
    /// uncovered — so the multi-entry case is always a `Group`.
    pub fn of(mut entries: Vec<PreparedEntry>) -> Self {
        if entries.len() == 1 {
            Self::Single(entries.pop().expect("just checked len == 1"))
        } else {
            Self::Group(entries)
        }
    }

    pub(crate) fn entries(&self) -> &[PreparedEntry] {
        match self {
            Self::Single(entry) => std::slice::from_ref(entry),
            Self::Group(entries) => entries,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }
}

/// Whether the *proposer* waits for durability (commit-protocol.md
/// §Vocabulary). Orthogonal to [`hotl_store::AckTier`], which is how durable
/// the write must be before the writer acks: canon is `Durable` in both
/// modes, and only who waits changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AckMode {
    /// The proposer awaits the durable ack inline.
    Sync,
    /// The actor validates, mints, assigns `seq`, forwards to the writer and
    /// answers immediately with a [`CommitTicket`]; durability is settled at
    /// the turn's next barrier (§Pipelined commits).
    #[default]
    Pipelined,
}

/// What the writer acked with, as a proposer sees it — the shipped
/// `hotl_store::Ack`, renamed at this boundary to match commit-protocol.md
/// §Vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitAck {
    pub offset: u64,
}

/// Why a pipelined commit will never be durable. Exactly two variants
/// (commit-protocol.md §Vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitFailed {
    /// The log is read-only for good; nothing further can be recorded.
    LogSealed,
    /// A compaction or branch move superseded the turn. The bytes already
    /// forwarded are canon and will land — what is discarded is the turn's
    /// claim on them, never the log (§conflict table, Abort).
    Aborted,
}

/// Handed back the moment the actor forwards a proposal to the writer in
/// [`AckMode::Pipelined`] (commit-protocol.md §Vocabulary). `id` and `seq`
/// are carried **eagerly** — the actor mints the ulid and assigns `seq` at
/// validation, before the write — so a proposer knows its own identity and
/// its own commit order without waiting for durability. Only durability
/// waits, and `ack` is the one place a commit failure is ever reported.
#[derive(Debug)]
pub struct CommitTicket {
    pub id: String,
    pub seq: u64,
    pub ack: oneshot::Receiver<Result<CommitAck, CommitFailed>>,
}

/// What [`SessionCmd::ProposePrepared`] answers with — plain `bool` has no
/// room for the rules-epoch guard's distinction (commit-protocol.md
/// §Proposal payloads): a stale epoch means "rebuild under the current
/// rules and resend", a genuinely different repair than "the log is sealed,
/// stop trying".
#[derive(Debug)]
pub enum ProposeReply {
    Committed,
    /// The actor refused the append: the log is sealed.
    Sealed,
    /// `PreparedPayload::rules_epoch` predates the actor's current masking
    /// rules epoch. Nothing in this proposal was committed.
    StaleEpoch,
    /// [`AckMode::Pipelined`] only: forwarded to the writer, durability
    /// outstanding. One ticket per proposal, bearing the proposal's last
    /// entry's id and seq — the shape a `Group` will keep unchanged (S2c).
    Ticket(CommitTicket),
}

// `BumpRulesEpoch` (below) is a real, data-free variant a test sends over
// the wire, not a non-exhaustiveness marker — clippy's heuristic can't tell
// those apart for a `#[doc(hidden)]` unit variant in last position.
#[allow(clippy::manual_non_exhaustive)]
pub enum SessionCmd {
    /// A user prompt. Starts a turn, or queues (one-at-a-time promotion).
    /// `images` are the prompt's attachments, already validated and
    /// base64-encoded at the wire entry point (`hotl::images::parse_images`).
    Prompt {
        text: String,
        images: Vec<hotl_types::UserImage>,
    },
    /// A prompt whose committed item carries a provenance tag (T2: schema
    /// contract + validation-retry feedback ride in as tagged user items).
    /// Engine-internal injections never carry images.
    PromptTagged {
        text: String,
        synthetic: hotl_types::SyntheticReason,
    },
    /// Continue an interrupted turn (M4/#8): sample against the current
    /// projection with no new user item — used on resume when the last item
    /// is a user/tool turn the model never answered. No-op if already running.
    Continue,
    /// Mid-turn guidance: admitted durably now, woven into the next sample.
    Steer {
        text: String,
        images: Vec<hotl_types::UserImage>,
    },
    /// Set the session's display name (durable: appended to the log).
    Rename(String),
    /// Set the session's effective permission mode (durable: appended to the
    /// log as `ModeSet`; takes effect immediately — no `Rules` reallocation).
    SetMode(PermissionMode),
    /// Toggle plan mode, the second permission axis (durable: appended to the
    /// log as `PlanSet`; takes effect immediately, same shape as `SetMode`).
    SetPlan(bool),
    /// Full-state replace of the `todo_write` checklist (durable: appended
    /// to the log as `Todos`, last-wins on replay — same shape as
    /// `Rename`/`SetMode`). The actor is the list's sole owner; the tool
    /// only ever forwards a validated `Vec<Todo>` here.
    SetTodos(Vec<Todo>),
    /// Pre-actor proposal path (durable-ack before reply): the ONLY caller
    /// left is [`question_sink`]'s `PendingQuestion`/`QuestionResolved`
    /// entries, built and sent before the actor (and its masker) exist yet
    /// — see `question_sink`'s doc comment on why it can't reach
    /// `SharedDeps`. Those entries are always human-sized (a question
    /// header/prompt/options), so they stay on the actor-serializing inline
    /// path, the same carve-out commit-protocol.md §Proposal payloads grants
    /// steer admissions/compaction digests/todo snapshots. A turn-task
    /// proposal uses [`SessionCmd::ProposePrepared`] instead — see that
    /// variant's doc comment for why this one can't be reused for that.
    Propose {
        entries: Vec<EntryPayload>,
        reply: oneshot::Sender<bool>,
    },
    /// Turn task → actor: commit already-prepared entries
    /// (commit-protocol.md §Proposal payloads). This is the type-level
    /// enforcement point requirement 4 of task 8 asks for: a turn-originated
    /// proposal can only reach the log through this channel, so a future
    /// call site cannot reintroduce actor-side serialization for these kinds
    /// — the entries carry pre-serialized, pre-masked `PreparedPayload`
    /// bytes, never a raw `EntryPayload`.
    ProposePrepared {
        proposal: EntryProposal,
        /// The proposer's declaration of whether its sample had closed —
        /// see [`SampleStage`]. Read once, by the held-steer release's
        /// assertion, and dropped.
        stage: SampleStage,
        /// Whether the proposer waits for durability (commit-protocol.md
        /// §Pipelined commits). `Pipelined` answers with a
        /// [`ProposeReply::Ticket`] the moment the entries are forwarded.
        mode: AckMode,
        reply: oneshot::Sender<ProposeReply>,
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
    /// Test-only: bump the actor's masking-rules epoch by one
    /// (commit-protocol.md §Proposal payloads' `rules_epoch` guard). Nothing
    /// in production sends this — the epoch is constant today — but an
    /// integration test needs a way to force a real
    /// reject-stale→re-mask→retry round trip through the actor's actual
    /// command loop, not just a hand-called commit function. `#[doc(hidden)]`
    /// rather than `#[cfg(test)]`: this crate's tests live in a separate
    /// compilation unit (`hotl-engine/tests/*.rs`) that only sees `pub` API,
    /// the same reason `hotl_store::SessionLog::inject_fault` is shaped this
    /// way.
    #[doc(hidden)]
    BumpRulesEpoch,
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
    /// The read side of the actor's published head — see
    /// [`SessionHandle::head`].
    head: tokio::sync::watch::Receiver<Arc<ProjectionHead>>,
    /// The actor task itself. Kept (rather than discarded, as before) so a
    /// one-shot CLI exit path can wait for the actor to fully shut down —
    /// including its now-synchronous `SessionEnd` hook call (Finding 1) —
    /// instead of just dropping the handle and hoping the actor gets another
    /// scheduler turn before the runtime goes away.
    actor: tokio::task::JoinHandle<()>,
}

impl SessionHandle {
    /// A read-only view of the actor's published projection head
    /// (commit-protocol.md §Read invariant). Only a `watch::Receiver` is ever
    /// handed out: the `Sender` never leaves the actor, so this grants a
    /// reader, never a second publisher. Used by `fork`, which seeds a child
    /// session from this one's history.
    pub fn head(&self) -> tokio::sync::watch::Receiver<Arc<ProjectionHead>> {
        self.head.clone()
    }

    pub async fn prompt(&self, text: String) {
        self.prompt_with(text, Vec::new()).await;
    }
    /// A prompt carrying attached images (already validated and
    /// base64-encoded at the wire entry point).
    pub async fn prompt_with(&self, text: String, images: Vec<hotl_types::UserImage>) {
        let _ = self.cmd.send(SessionCmd::Prompt { text, images }).await;
    }
    /// A prompt whose committed user item carries a provenance tag (T2).
    pub async fn prompt_tagged(&self, text: String, synthetic: hotl_types::SyntheticReason) {
        let _ = self
            .cmd
            .send(SessionCmd::PromptTagged { text, synthetic })
            .await;
    }
    pub async fn steer(&self, text: String) {
        self.steer_with(text, Vec::new()).await;
    }
    /// A steer carrying attached images — same plumbing as [`Self::steer`].
    pub async fn steer_with(&self, text: String, images: Vec<hotl_types::UserImage>) {
        let _ = self.cmd.send(SessionCmd::Steer { text, images }).await;
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
    /// Toggle plan mode durably (a `plan_set` log entry; last one wins).
    /// Immediate, atomic-backed, same as [`Self::set_mode`].
    pub async fn set_plan(&self, plan: bool) {
        let _ = self.cmd.send(SessionCmd::SetPlan(plan)).await;
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
    // The head's read side is created here rather than inside the actor so
    // `SessionHandle::head` can hand it out immediately: the actor takes the
    // `Sender` and never gives it up.
    let (head_tx, head_rx) = actor::head_channel();
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
        head_tx,
    ));
    SessionHandle {
        cmd: cmd_tx,
        events: event_rx,
        current_turn,
        notifications,
        head: head_rx,
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
    // §S1 HookRouter gate: resolved once here (sink-construction time, never
    // per question) — the same handle-first, snapshot-fallback shape
    // `SharedDeps::hook_mask` uses, so a live handle's mid-session
    // narrowing (e.g. a three-strike eviction) is visible to `hook_gate!`
    // below immediately, not just at the next session.
    let hook_mask: Arc<std::sync::atomic::AtomicU8> = hooks_handle
        .as_ref()
        .and_then(|h| h.mask_handle())
        .unwrap_or_else(|| {
            Arc::new(std::sync::atomic::AtomicU8::new(
                hooks_handle
                    .as_ref()
                    .map_or(hooks::EventMask::NONE, |h| h.event_mask())
                    .bits(),
            ))
        });
    std::sync::Arc::new(move |question, cancel| {
        let hook_mask = Arc::clone(&hook_mask);
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
            crate::hooks::hook_gate!(
                hooks_handle,
                crate::hooks::mask_of(&hook_mask),
                crate::hooks::EventMask::NOTIFICATION,
                |h| {
                    crate::hooks::notify(
                        h,
                        &notifications,
                        crate::hooks::NotificationKind::Blocked,
                        question.header.clone(),
                    );
                },
                else {}
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// IMPORTANT 3 (task 8 review): `PreparedEntry::new`'s debug_assert
    /// catches a mismatched pair — the bug class the reviewer flagged
    /// (bytes and item as two unchecked sources of truth).
    #[test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "drives a debug_assert, which release builds compile out"
    )]
    #[should_panic(expected = "item's presence must match payload.kind()")]
    fn prepared_entry_new_rejects_a_mismatched_item_and_kind() {
        let masker = hotl_store::Masker::empty();
        let payload = hotl_store::prepare_payload(
            &EntryPayload::Usage {
                usage: TokenUsage::default(),
            },
            &masker,
            0,
        )
        .unwrap();
        // `item` is `Some`, but `payload` was built from `Usage`, not `Item`.
        let _ = PreparedEntry::new(
            payload,
            Some(Item::User {
                text: "x".into(),
                synthetic: None,
                images: Vec::new(),
            }),
        );
    }

    #[test]
    fn prepared_entry_new_accepts_a_matching_item_and_kind() {
        let masker = hotl_store::Masker::empty();
        let item = Item::User {
            text: "x".into(),
            synthetic: None,
            images: Vec::new(),
        };
        let payload =
            hotl_store::prepare_payload(&EntryPayload::Item { item: item.clone() }, &masker, 0)
                .unwrap();
        let entry = PreparedEntry::new(payload, Some(item));
        assert!(entry.item().is_some());
    }
}
