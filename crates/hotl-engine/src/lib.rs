//! L3 — the turn engine, M1: actor + turn tasks (commit-protocol.md).
//!
//! One **session actor** per session is the sole committer to the log and the
//! owner of the projection ([`actor`]); **turn tasks** read actor-granted
//! snapshots at sample boundaries and *propose* entries ([`turn`]). Steers
//! admitted mid-turn are woven into the next sample (the conflict table's
//! rebase row); interrupts travel out-of-band via a shared token; permission
//! asks are events carrying a oneshot reply.

mod actor;
pub mod clearing;
mod expect;
pub mod hooks;
mod ledger;
mod nudge;
pub mod plan_state;
mod turn;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hotl_platform::Clock;
use hotl_provider::{CacheTtl, Effort, EffortSchedule, Provider};
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

/// Where a session writes its plan artifact (0056 T2). Injected, never
/// derived: the engine has no opinion about XDG, and testkit sessions must
/// never touch the real data dir.
#[derive(Debug, Clone)]
pub struct PlanFiles {
    /// The project id the plan is filed under (`hotl_store::project::id`).
    pub project: String,
    /// `<data>/plans/<project>` — holds `current.json` and `current.md`.
    pub dir: PathBuf,
    /// Optional in-repo mirror of `current.md` (`[plan] repo_dir`).
    pub repo_mirror: Option<PathBuf>,
}

/// Re-exported alongside [`ProjectionHead`]: what a read of it yields, split
/// into the durable projection and the ephemeral per-sample tail. Out-of-crate
/// readers (`fork`'s history seed) name it to say which half they take.
pub use actor::Snapshot;

/// Re-exported so the cap is one number the tests and the docs both name:
/// mid-stream re-samples one sample may spend (0050 T3).
pub use turn::STREAM_RETRY_MAX;

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
    /// Reasoning depth for every sample this session takes. `None` = the
    /// provider's own default; the dialect decides the wire spelling.
    pub effort: Option<Effort>,
    /// Per-phase rungs applied at turn start (0059 T1). `None` = one depth for
    /// the whole session. A phase with no rung inherits [`Self::effort`], and
    /// an explicit `/effort` pins the session and stops the schedule.
    pub effort_schedule: Option<EffortSchedule>,
    /// Extra command prefixes that count as verification when the schedule
    /// derives its phase, on top of the built-in table
    /// (`[behavior] verify_commands`).
    pub verify_commands: Vec<String>,
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
    /// Tool calls this **session** may run before a batch is refused
    /// (`[behavior] max_tool_calls`). `0` = no cap. Checked once per batch, so
    /// one wide batch may cross it by its own width — a runaway backstop, not
    /// an accounting ledger.
    pub max_tool_calls: u64,
    /// USD this **session** may spend before a sample is refused pre-flight
    /// (`[behavior] max_cost_usd`). `0.0` = no cap, and an uncatalogued model
    /// has no price, so no cap applies there either (warned once at startup).
    pub max_cost_usd: f64,
    /// Model context window in tokens; compaction triggers at 80% (M2).
    pub context_window: u64,
    /// Housekeeping model (compaction summarize); defaults to `model`.
    pub fast_model: Option<String>,
    /// The one-off-call role (0059 T2): compaction digests and goal
    /// evaluations. Resolved by [`EngineConfig::utility`] — this field is the
    /// explicit setting only, so `/cost` can still tell a configured role from
    /// an inherited fallback.
    pub utility_model: Option<String>,
    /// Reset-mode compaction (M4/#9): the continuation gets the preserved
    /// prefix + digest only, no verbatim tail — a fresh slate rather than a
    /// summarized-then-refilling window. Default false = M2 in-place behavior.
    pub compaction_reset: bool,
    /// Include `context_used%` in the MOIM turn-context block (M4/#9).
    /// Default false: broadcasting fullness every sample induces "context
    /// anxiety" — premature wrap-up (Anthropic long-horizon finding). Opt in
    /// via `[context] show_used_pct = true`.
    pub show_context_pct: bool,
    /// Evict a successful tool result larger than this (estimated tokens) to a
    /// masked blob, leaving a head preview + read pointer (T4). `0` disables.
    pub evict_threshold_tokens: u64,
    /// How this model's tokenizer trades characters for tokens (0057 T6,
    /// tracker #63). Every estimate in the engine — the compaction trigger,
    /// the clear trigger, the spill threshold and `/context` — reads this one
    /// ruler, so the number a user sees and the number that folds their
    /// history can never disagree.
    pub token_profile: hotl_context::TokenProfile,
    /// Per-tool spill thresholds that override [`Self::evict_threshold_tokens`]
    /// (0057 T2). A `bash` or `grep` result is a haystack the model wanted one
    /// needle out of, so it spills far earlier than a `read` — which is a
    /// file the model asked for in full.
    pub evict_overrides: Vec<(String, u64)>,
    /// How many of the newest user turns keep their tool results verbatim
    /// when the context ladder clears (0057). `0` disables clearing.
    pub keep_results_turns: usize,
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

impl EngineConfig {
    /// The model a one-off call takes: `utility_model` → `fast_model` → the
    /// session model. One resolver, so compaction and goal evaluation can
    /// never drift onto different models.
    pub fn utility(&self) -> String {
        self.utility_model
            .clone()
            .or_else(|| self.fast_model.clone())
            .unwrap_or_else(|| self.model.clone())
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4-8".into(),
            // Anthropic recommends ≥64K output headroom at high/xhigh effort,
            // and adaptive thinking shares this cap.
            max_tokens: 64_000,
            // Roomy enough that ordinary agentic work (long edit/test/fix
            // chains) finishes inside it — the cap is a runaway backstop, not
            // a work ceiling. Sub-agent call sites set their own, tighter.
            max_turns: 100,
            thinking: true,
            effort: None,
            effort_schedule: None,
            verify_commands: Vec::new(),
            cache_static: true,
            cache_ttl: CacheTtl::FiveMinutes,
            fallback_models: Vec::new(),
            tool_failure_budget: 5,
            max_tool_calls: 0,
            max_cost_usd: 0.0,
            context_window: 200_000,
            fast_model: None,
            utility_model: None,
            compaction_reset: false,
            show_context_pct: false,
            evict_threshold_tokens: 20_000,
            token_profile: hotl_context::TokenProfile::CONSERVATIVE,
            evict_overrides: vec![("bash".into(), 6_000), ("grep".into(), 6_000)],
            keep_results_turns: 4,
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
    /// The ladder's cheap rung below `Compact` (0057): clear these old tool
    /// results to stubs, then respawn the same logical turn. Same
    /// terminate → re-point → respawn shape as a fold, and the same `cont`.
    Clear {
        ids: Vec<String>,
        cont: Box<TurnContinuation>,
        /// What a speculative digest this clear abandons already cost (0051
        /// decision 6). Clearing drops below the speculate threshold, so the
        /// digest will never be folded — but it was billed, and an unreported
        /// spend is the one direction this codebase treats as unacceptable.
        spec_usage: TokenUsage,
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
    /// When the logical turn started (0059 T3). `None` on a fresh turn — the
    /// respawn reads the clock itself — `Some` across a fold, so the MOIM's
    /// `elapsed_s` keeps counting through a compaction.
    pub(crate) started_ms: Option<u64>,
    /// Fallback-model position: a continuation does not silently revert to the
    /// primary model that just failed.
    pub(crate) model_idx: usize,
    /// The doom detector's trailing signature window.
    pub(crate) call_sigs: std::collections::VecDeque<crate::turn::CallSig>,
    /// The stagnation detectors' per-turn memo (0059 T4). Carried, so a fold
    /// does not let an already-named pattern be named again.
    pub(crate) nudges: crate::nudge::Detectors,
    /// Flagged decisions already notified this prompt (0037 D5). Carried so a
    /// mid-prompt compaction doesn't repeat every notice; a NEW prompt starts
    /// from `default()`, so nothing stays buried across a long session.
    pub(crate) flagged: std::collections::HashSet<crate::turn::FlagKey>,
    /// Per-tool consecutive failures (the tool-failure budget).
    pub(crate) consecutive_failures: std::collections::HashMap<String, u32>,
    /// The shared per-prompt "reminder and continue" budget.
    pub(crate) turn_extensions: u32,
    /// Tool results that missed a stated `expect` so far this prompt (0050 T5).
    pub(crate) mispredictions: u32,
    /// Whether this prompt already spent its one clearing pass (0057). Once
    /// per prompt is the whole budget: `clearing::candidates` counts back
    /// from the newest *user* turns, and a prompt adds none, so a second pass
    /// could only ever return the same (now stubbed) set.
    pub(crate) cleared: bool,
    /// Truncation-recovery continues already spent (MAX_TOKENS_CONTINUE_MAX).
    pub(crate) max_tokens_continues: u32,
    /// Completed samples since the last fold — the compaction streak's
    /// "intervening completed sample" (T2-3). Read by `actor::try_compact`;
    /// a fresh continuation restarts the count at zero.
    pub(crate) samples_since_compact: u32,
    /// Calls that executed this logical turn (0051 G1) — carried, because a
    /// fold does not undo the work already done.
    pub(crate) tools_ran: u32,
    /// Denials in a row, and in total, this logical turn (0059 T5). Carried:
    /// a fold does not un-deny anything.
    pub(crate) denials_consecutive: u32,
    pub(crate) denials_total: u32,
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
    /// What the speculative summarize cost. Carried so an adopted digest is
    /// as honest about its spend as the inline fold (0051 decision 6).
    pub usage: TokenUsage,
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
    Done {
        text: String,
    },
    Cancelled,
    TurnLimit,
    Refused,
    DoomLoop {
        pattern: String,
    },
    ToolFailureBudget {
        tool: String,
    },
    /// Denials piling up (0059 T5): three in a row, or twenty in one turn.
    /// A denial is a human saying no; a spiral is the agent not hearing it,
    /// and burning samples asking again is what this ends.
    DenialSpiral {
        consecutive: u32,
        total: u32,
    },
    /// A session-level budget stopped the turn before it spent more.
    /// `kind` is `"tool_calls"` or `"cost_usd"`; `used`/`cap` are in that
    /// kind's own unit.
    Budget {
        kind: String,
        used: f64,
        cap: f64,
    },
    Error {
        message: String,
    },
}

/// Denials in a row before the turn ends. Three: two is a human changing
/// their mind about one call, three is a pattern nothing downstream fixes.
pub const DENIAL_SPIRAL_CONSECUTIVE: u32 = 3;
/// Denials in one turn, however scattered, before the turn ends.
pub const DENIAL_SPIRAL_TOTAL: u32 = 20;

/// The `[behavior]` key that raises a budget of this kind.
pub fn budget_key(kind: &str) -> &'static str {
    match kind {
        "cost_usd" => "max_cost_usd",
        _ => "max_tool_calls",
    }
}

/// The one refusal sentence every budget shares: the invariant, the target,
/// and the next action (core belief 9).
pub fn budget_refusal(kind: &str, used: f64, cap: f64) -> String {
    let n = |v: f64| match kind {
        "cost_usd" => format!("${v:.2}"),
        _ => format!("{v:.0}"),
    };
    format!(
        "Session budget reached: {} of {} ({kind}). Raise [behavior] {} or \
         start a new session.",
        n(used),
        n(cap),
        budget_key(kind),
    )
}

/// Everything the surface renders. `Ask` carries the reply channel — the
/// surface (or an allow-rule upstream) is the human on the loop.
pub enum EngineEvent {
    TextDelta(String),
    ThinkingDelta(String),
    /// Every tool lifecycle event carries the provider's `tool_use` id (0037):
    /// N concurrent same-name calls are indistinguishable by name alone, and a
    /// surface that settles "the newest `read` card" strands the other N−1.
    ToolStart {
        id: String,
        name: String,
        summary: String,
    },
    ToolDone {
        id: String,
        name: String,
        ok: bool,
    },
    ToolDenied {
        id: String,
        name: String,
    },
    ToolAutoAllowed {
        id: String,
        name: String,
        rule: String,
    },
    /// Bypass-mode floor event (0036): the call was allowed (`denied: false`)
    /// or refused (`denied: true`) without a human, and the surface must show
    /// it prominently — this is the notification that replaces the ask.
    /// Coalesced per turn (0037): one notice per distinct
    /// (tool, class, path, denied) decision — the floor still evaluates and
    /// resolves every call.
    ToolFlagged {
        id: String,
        name: String,
        summary: String,
        why: String,
        denied: bool,
    },
    /// A sub-agent's tool activity forwarded on the parent stream (0039 D1):
    /// `parent_id` is the spawn call's own `tool_use` id, `ok: None` = start,
    /// `Some(ok)` = done. A denied child arrives already settled
    /// (`Some(false)`) — it never got a start.
    ChildTool {
        parent_id: String,
        id: String,
        name: String,
        summary: String,
        ok: Option<bool>,
        /// Child token total on the done frame (input + output + cache_read +
        /// cache_creation); `None` on start and for forwarded tool calls.
        tokens: Option<u64>,
    },
    /// A sub-agent's own text and thinking, forwarded live on the parent
    /// stream (0058 T2). Never a parent log entry — the child logs its own
    /// words in its own session; this exists so a human can watch a child
    /// work instead of staring at a stalled card.
    ChildText {
        parent_id: String,
        text: String,
    },
    /// One turn's delegation summary (0058 T2), emitted at turn end when the
    /// turn spawned anything at all.
    Delegation {
        subagent_runs: u32,
        /// Paths more than one sibling touched — duplicated work.
        duplicate_work_paths: u32,
        /// Children that reported `completed` while the caller's own
        /// `validate_cmd` said otherwise.
        false_completions: u32,
    },
    Retrying {
        attempt: u32,
        reason: String,
        /// The re-sample threw away text the surface had already rendered
        /// (0050 T3), so the surface must un-render it or the answer appears
        /// twice — once half-written, once whole.
        discarded_partial: bool,
    },
    FallbackModel {
        model: String,
    },
    PromptQueued,
    /// A session spend budget crossed one of its thresholds (0059 T5).
    /// Fires once per threshold per session; `pct` is 50, 80 or 100.
    BudgetNotice {
        pct: u8,
        used_usd: f64,
        cap_usd: f64,
    },
    /// Context was compacted (digest + verbatim tail); `degraded` means the
    /// summarize call failed and the floor placeholder was used.
    Compacted {
        degraded: bool,
    },
    /// Old tool results were cleared to stubs (0057) — the rung below
    /// `Compacted`, and the one a long exploration should hit first.
    Cleared {
        count: usize,
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
    /// A subprocess tried to reach a host outside `[network].allow`, and no
    /// human-approved call in flight had shown that host (plan 0026).
    ///
    /// A **grant**, unlike `Question`, and a socket rather than a tool, unlike
    /// `Ask` — hence its own variant rather than a sixth `AskReply` case that
    /// would be unreachable at every tool-call site that matches `AskReply`
    /// exhaustively. A dropped `reply` resolves to `NoAnswer`, which refuses
    /// the connection and records nothing.
    EgressAsk {
        host: String,
        reply: oneshot::Sender<hotl_tools::net::EgressDecision>,
    },
    TurnDone {
        outcome: Outcome,
        usage: TokenUsage,
        /// Tool results that missed a stated `expect` (0050 T5), cumulative
        /// over the whole prompt — compaction respawns and goal-loop
        /// continuations fold into this one number, like `usage`.
        mispredictions: u32,
    },
    /// The `todo_write` checklist changed (a full-state replace committed).
    /// Ephemeral-context companion to the durable `Todos` entry: the surface
    /// (console strip, `hotl watch`) renders progress from this, never from
    /// parsing model text.
    TodosChanged {
        items: Vec<Todo>,
    },
    /// The session's goal changed (`/goal`, 0034): set (`Some`) or
    /// resolved/cleared (`None`). Same shape as `TodosChanged` — the surface
    /// renders goal state from this, never from parsing text. Also covers
    /// engine-initiated clears (met/impossible verdicts).
    GoalChanged {
        condition: Option<String>,
    },
    /// The model handed the user a plan to approve or revise (`present_plan`,
    /// 0056 T3). The nodes are already durable — this is the surface's cue to
    /// show them and offer the two answers, not a request the engine waits on.
    PlanPresented {
        summary: String,
        nodes: Vec<Todo>,
        /// The artifact the plan was written to, when this session files one.
        path: Option<String>,
    },
    /// The goal evaluator's per-turn judgment (0034). `turns` counts
    /// evaluated turns since the goal was set (in-memory; resets on resume).
    /// `usage` is the loop's cumulative spend since then, the evaluator's own
    /// calls included — the surface reads spend from here, never from local
    /// sums (0051 G4).
    GoalVerdict {
        verdict: GoalVerdictKind,
        reason: String,
        turns: u32,
        usage: TokenUsage,
        /// What the harness observed each plan node's `validate_cmd` do
        /// (0056 T4), one rendered line each. Additive and often empty — a
        /// plan whose steps name no commands has nothing to report.
        evidence: Vec<String>,
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
            Self::ToolDone { name, ok, .. } => write!(f, "ToolDone({name},{ok})"),
            Self::ToolDenied { name, .. } => write!(f, "ToolDenied({name})"),
            Self::ToolAutoAllowed { name, rule, .. } => {
                write!(f, "ToolAutoAllowed({name},{rule})")
            }
            Self::ToolFlagged { name, denied, .. } => {
                write!(f, "ToolFlagged({name},denied={denied})")
            }
            Self::ChildTool { name, ok, .. } => {
                let phase = if ok.is_some() { "done" } else { "start" };
                write!(f, "ChildTool({name},{phase})")
            }
            Self::ChildText { text, .. } => write!(f, "ChildText(n={})", text.len()),
            Self::Delegation { subagent_runs, .. } => write!(f, "Delegation({subagent_runs})"),
            Self::Retrying { attempt, .. } => write!(f, "Retrying({attempt})"),
            Self::FallbackModel { model } => write!(f, "FallbackModel({model})"),
            Self::PromptQueued => write!(f, "PromptQueued"),
            Self::BudgetNotice { pct, .. } => write!(f, "BudgetNotice({pct}%)"),
            Self::Compacted { degraded } => write!(f, "Compacted({degraded})"),
            Self::Cleared { count } => write!(f, "Cleared({count})"),
            Self::Ask { summary, .. } => write!(f, "Ask({summary})"),
            Self::Question { question, .. } => write!(f, "Question({})", question.header),
            Self::EgressAsk { host, .. } => write!(f, "EgressAsk({host})"),
            Self::TurnDone { outcome, .. } => write!(f, "TurnDone({outcome:?})"),
            Self::TodosChanged { items } => write!(f, "TodosChanged(n={})", items.len()),
            Self::PlanPresented { nodes, .. } => write!(f, "PlanPresented(n={})", nodes.len()),
            Self::GoalChanged { condition } => {
                write!(f, "GoalChanged(set={})", condition.is_some())
            }
            Self::GoalVerdict { verdict, turns, .. } => {
                write!(f, "GoalVerdict({verdict:?},turns={turns})")
            }
            Self::LedgerReport(s) => write!(f, "LedgerReport(samples={})", s.sample_count),
        }
    }
}

/// What the goal evaluator concluded, engine-side vocabulary: the parser
/// never produces `EvalFailed` — it marks an evaluation that failed, timed
/// out, or was cancelled, which fails open (goal kept, turn ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalVerdictKind {
    NotYet,
    Met,
    Impossible,
    EvalFailed,
    /// The loop paused: `GOAL_STALL_TURNS` consecutive not-yet verdicts with
    /// no tool executed. The goal STAYS SET — the next user prompt re-arms it
    /// with a fresh idle budget (0051 G1).
    Stalled,
    /// The goal is tombstoned: the turn ended in an error only the owner can
    /// clear (auth, 401/402/403/404, context exhausted). Resume must never
    /// re-arm the loop against a dead credential (0051 G6).
    Errored,
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

/// 0033 Task 10: a prompt admitted through the ticketed path — the fast
/// admission's handoff from the actor to the turn it spawns. The ticket is
/// the prompt entry's (its ack resolves during TLS + TTFB instead of before
/// request-building starts); `predicted` is the snapshot the head will
/// publish when that ack lands: head items + the prompt item. Adoption,
/// mismatch and crash semantics are identical to mid-turn speculation.
pub(crate) struct Admission {
    pub(crate) ticket: CommitTicket,
    pub(crate) predicted: actor::Snapshot,
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
    /// Set the session's reasoning depth (durable: appended to the log as
    /// `EffortSet`; takes effect on the next request). `None` = the provider's
    /// own default, which must round-trip so a user can clear the setting.
    SetEffort(Option<Effort>),
    /// Full-state replace of the `todo_write` checklist (durable: appended
    /// to the log as `Todos`, last-wins on replay — same shape as
    /// `Rename`/`SetMode`). The actor is the list's sole owner; the tool
    /// only ever forwards a validated `Vec<Todo>` here.
    /// `present_plan` (0056 T3): the same durable write `SetTodos` makes,
    /// plus the summary and the `PlanPresented` event the surfaces turn into
    /// an approve/revise card. A separate command rather than a flag on
    /// `SetTodos`: only this one hands the turn back to the human.
    PresentPlan { summary: String, nodes: Vec<Todo> },
    SetTodos {
        todos: Vec<Todo>,
        /// New decisions to append. Their `when_ms` arrives zero — the actor
        /// stamps it from the session clock, so the model cannot backdate one.
        decisions: Vec<hotl_types::Decision>,
    },
    /// Set (`Some`) or clear (`None`) the session's goal (durable: appended
    /// to the log as `GoalSet`, last-wins on replay). Clearing an *active*
    /// goal appends the tombstone (outcome `"cleared"`); clearing when none
    /// is active is a silent no-op — no entry, no event.
    SetGoal(Option<String>),
    /// A read-only breakdown of what currently fills the context window
    /// (plan 0028). The ONLY `SessionCmd` that neither appends to the log nor
    /// advances the projection: it reads the actor's own head plus two
    /// `SharedDeps` fields and replies. Safe mid-turn for exactly that reason.
    /// A dropped `reply` (the client hung up) is not an error.
    ContextBreakdown {
        reply: oneshot::Sender<hotl_types::ContextBreakdown>,
    },
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
    TurnFinished {
        end: TurnEnd,
        usage: TokenUsage,
        /// Mispredicted tool results this turn (0050 T5), summed into the
        /// goal loop's carry exactly like `usage`.
        mispredictions: u32,
        /// Calls that executed this logical turn, denials excluded — the
        /// goal gate's "did anything happen?" (0051 G1).
        tools_ran: u32,
        /// The error outcome, if any, is one the owner must fix (auth,
        /// 401/402/403/404) rather than one a retry could clear (0051 G6).
        unrecoverable: bool,
    },
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
    /// The decisions log a resumed session starts with (0056 T2) — same
    /// seed-not-replay shape as `initial_todos`.
    pub initial_decisions: Vec<hotl_types::Decision>,
    /// Where this session's plan artifact is filed, when it is filed at all.
    /// `None` in tests and anywhere without a data dir; the engine never
    /// derives an XDG path itself.
    pub plan_files: Option<PlanFiles>,
    /// The goal a resumed session starts with (the replayed chain's last
    /// `GoalSet`, tombstones applied — see `hotl_store::Replayed::goal`).
    /// `None` for a fresh session. A seed, never a post-spawn `SetGoal`, so
    /// resume appends no duplicate `GoalSet` entry; the turn counter starts
    /// at zero (in-memory by design).
    pub initial_goal: Option<String>,
    pub config: EngineConfig,
    /// The process-wide Layer-B budget, cloned — never a fresh
    /// `SessionConcurrency::default()` outside tests, which would hand this
    /// session a second, independent set of semaphores.
    pub concurrency: hotl_tools::concurrency::SessionConcurrency,
}

/// See [`SessionHandle::turn_cancel`]. Reads the cell, never writes it — only
/// the actor and `interrupt` may swap or cancel the token.
#[derive(Clone)]
pub struct TurnCancel(Arc<Mutex<CancellationToken>>);

impl TurnCancel {
    /// The token the current turn is racing, right now.
    pub fn token(&self) -> CancellationToken {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
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
    /// Set the session's reasoning depth durably (an `effort_set` log entry;
    /// last one wins). Immediate, atomic-backed, same as [`Self::set_mode`];
    /// `None` restores the provider's own default.
    pub async fn set_effort(&self, effort: Option<Effort>) {
        let _ = self.cmd.send(SessionCmd::SetEffort(effort)).await;
    }
    /// Full-state replace of the todo checklist (a durable `todos` log
    /// entry). Exposed mainly for tests that pre-seed a list; the real
    /// entry point is the `todo_write` tool's sink.
    pub async fn set_todos(&self, items: Vec<Todo>) {
        self.set_plan_nodes(items, Vec::new()).await;
    }
    /// Hand the user a plan (`present_plan`, 0056 T3).
    pub async fn present_plan(&self, summary: String, nodes: Vec<Todo>) {
        let _ = self
            .cmd
            .send(SessionCmd::PresentPlan { summary, nodes })
            .await;
    }
    /// The full-state replace plus decisions to append (0056 T2).
    pub async fn set_plan_nodes(&self, todos: Vec<Todo>, decisions: Vec<hotl_types::Decision>) {
        let _ = self
            .cmd
            .send(SessionCmd::SetTodos { todos, decisions })
            .await;
    }
    /// Set or clear the session's goal (a durable `goal_set` log entry;
    /// last one wins, and the tombstone means an achieved/cleared goal never
    /// survives resume). `None` clears.
    pub async fn set_goal(&self, condition: Option<String>) {
        let _ = self.cmd.send(SessionCmd::SetGoal(condition)).await;
    }
    /// What currently fills the context window, by source (`/context`).
    /// `None` if the actor is gone. Appends nothing and publishes nothing, so
    /// unlike every other command here it is safe to call mid-turn.
    pub async fn context_breakdown(&self) -> Option<hotl_types::ContextBreakdown> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(SessionCmd::ContextBreakdown { reply: tx })
            .await
            .ok()?;
        rx.await.ok()
    }
    /// Continue an interrupted turn on resume (M4/#8).
    pub async fn continue_turn(&self) {
        let _ = self.cmd.send(SessionCmd::Continue).await;
    }
    /// A live view of whatever token the *current* turn is racing — the same
    /// cell [`SessionHandle::interrupt`] cancels. Handed to the plan-0026
    /// egress sink, which is built once per session but must not hold a
    /// connection open across a turn the user already interrupted; a plain
    /// `CancellationToken` clone would freeze on the turn that happened to be
    /// current at session start.
    pub fn turn_cancel(&self) -> TurnCancel {
        TurnCancel(self.current_turn.clone())
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
pub fn needs_continuation<I: std::borrow::Borrow<Item>>(items: &[I]) -> bool {
    matches!(
        items.last().map(std::borrow::Borrow::borrow),
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

/// The production [`hotl_tools::net::EgressAskSink`] (plan 0026): the proxy's
/// bridge to a human when a subprocess reaches a host `[network].allow` does
/// not cover.
///
/// Three things happen here, in order:
///
/// 1. **The shown-hosts rule.** If a human-approved call still in flight had
///    this host on screen, allow it with no event and no prompt. A human who
///    approves `curl https://docs.example.com` has read the host; asking again
///    is the noise that trains people to reflex-key the gate. `Verdict::Auto`
///    deliberately does not count — an allow-rule is not a human, and a
///    rule-approved `curl` reaching an unlisted host is precisely the case the
///    ask exists for.
/// 2. Otherwise emit [`EngineEvent::EgressAsk`] and await the answer, racing
///    the session's cancellation token.
/// 3. Log the decision either way. A suppressed ask is still a decision
///    (0026 decision 14) — without the line the audit trail shows a connection
///    nobody appears to have approved.
///
/// Every failure resolves to `NoAnswer`: a gone session, a send failure, a
/// dropped reply, a cancelled turn. `NoAnswer` refuses the connection and is
/// never written to the session table.
///
/// Only *weak* senders are captured, for the same reason [`question_sink`]
/// captures weak ones: this sink is installed process-wide and outlives the
/// session, so a strong sender here would keep the actor alive forever.
///
/// The sink is built once per session but resolves `cancel` per ask, so an
/// interrupt lands on the turn that is actually running (see
/// [`SessionHandle::turn_cancel`]). Cancellation here is the engine-side
/// guard; the proxy's independent one is its own deadline.
pub fn egress_ask_sink(
    events_tx: mpsc::WeakSender<EngineEvent>,
    cancel: TurnCancel,
) -> hotl_tools::net::EgressAskSink {
    use hotl_tools::net::EgressDecision;
    Arc::new(move |ask: hotl_tools::net::EgressAsk| {
        let events_tx = events_tx.clone();
        let cancel = cancel.token();
        Box::pin(async move {
            let host = ask.host;
            if hotl_tools::net::host_was_shown(&host) {
                eprintln!("egress: allowed \"{host}\" — shown in an approved call");
                return EgressDecision::Allow;
            }
            let Some(events) = events_tx.upgrade() else {
                eprintln!("egress: denied \"{host}\" — no answer");
                return EgressDecision::NoAnswer;
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if events
                .send(EngineEvent::EgressAsk {
                    host: host.clone(),
                    reply: reply_tx,
                })
                .await
                .is_err()
            {
                eprintln!("egress: denied \"{host}\" — no answer");
                return EgressDecision::NoAnswer;
            }
            let decision = tokio::select! {
                biased;
                _ = cancel.cancelled() => EgressDecision::NoAnswer,
                reply = reply_rx => reply.unwrap_or(EgressDecision::NoAnswer),
            };
            match decision {
                EgressDecision::Allow => eprintln!("egress: allowed \"{host}\" — human"),
                EgressDecision::Deny => eprintln!("egress: denied \"{host}\" — human"),
                EgressDecision::NoAnswer => eprintln!("egress: denied \"{host}\" — no answer"),
            }
            decision
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
