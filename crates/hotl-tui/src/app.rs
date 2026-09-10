//! The Elm core: `State` × `Msg` → mutations + `Cmd` effects. Pure — elapsed
//! time is tick counts (8/sec), never wall-clock, so every transition is
//! golden-testable.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hotl_tools::ask::{Question, QuestionOption};
use hotl_types::{ContextKind, ContextRow};
use serde_json::Value;

use crate::complete::{self, Completion};
use crate::paste;
use crate::select;
use crate::vim::{Editor, EditorEvent};

/// What the agent is doing right now. `ticks` count time *in this phase*
/// (8/sec); `WaitingAsk` deliberately has none — the loop is halted on you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Sampling {
        ticks: u64,
    },
    Streaming {
        ticks: u64,
        chars: u64,
    },
    Tool {
        name: String,
        ticks: u64,
    },
    WaitingAsk {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
        input: String,
        denying: bool,
        /// The proposed change, when the server sent one. Empty for every ask
        /// today — see `hotl::diffgen::for_tool`'s unimplemented-invariant
        /// marker (RQ-2): the engine's ask carries no tool input to diff.
        diff: Vec<DiffLine>,
    },
    /// A structured `ask_user` question (tier-1 gap #4) — NOT a permission
    /// ask: answering it never authorizes a tool, it only supplies text the
    /// model reads. `input` is the free-text buffer; once the human starts
    /// typing, digit keys stop selecting and become ordinary characters.
    WaitingQuestion {
        req_id: u64,
        header: String,
        prompt: String,
        options: Vec<QuestionOption>,
        input: String,
        /// The option under the `↑`/`↓` cursor (0049 T6, LD5); `Enter` with
        /// nothing typed picks it, exactly as its digit would.
        selected: usize,
    },
    /// An egress ask (plan 0026): a subprocess reached a host that was not in
    /// `[network].allow` and was not on screen when the human approved the
    /// command that opened the connection.
    ///
    /// Rendered deliberately unlike `WaitingAsk` — different heading, the host
    /// on its own line, and the "was not in the approved command" line. A
    /// human who just approved `npm install` would otherwise read a second
    /// modal as a duplicate and reflex-key it, which is the failure mode the
    /// whole feature is trying to avoid.
    WaitingEgress {
        req_id: u64,
        host: String,
    },
}

/// One row of a proposed change, as it arrives on the wire. The generator
/// lives in the runtime crate (`hotl::diffgen` — `write`'s "before" is a file
/// read); this crate only renders what it is handed, so the core stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    Ctx,
    Add,
    Del,
    /// The `[+N more lines]` trailer; never file content.
    Trailer,
}

impl DiffOp {
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "ctx" => DiffOp::Ctx,
            "add" => DiffOp::Add,
            "del" => DiffOp::Del,
            "trailer" => DiffOp::Trailer,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

/// Append-only text with a revision that bumps on every mutation, so the
/// render memo can fingerprint it in O(1) as (seed, rev, len) instead of
/// hashing the whole content per frame. Privacy of `text` is what keeps the
/// old content-hash guarantee — no mutation path can skip the bump. `seed`
/// hashes the construction-time content once: a *different* item landing at
/// the same transcript index can never fingerprint equal to the old one.
#[derive(Debug, Clone, Eq)]
pub struct Streamed {
    text: String,
    seed: u64,
    rev: u64,
}

impl Streamed {
    pub fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
        self.rev += 1;
    }
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn rev(&self) -> u64 {
        self.rev
    }
    /// Cut back to `len` bytes (a re-sample discarding partial text, 0050
    /// T3). Bumps `rev` so every render cache treats it as new content.
    pub fn truncate(&mut self, len: usize) {
        if len < self.text.len() && self.text.is_char_boundary(len) {
            self.text.truncate(len);
            self.rev += 1;
        }
    }
    pub fn seed(&self) -> u64 {
        self.seed
    }
}

/// Content equality — `seed`/`rev` are cache hints, not identity.
impl PartialEq for Streamed {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl PartialEq<&str> for Streamed {
    fn eq(&self, other: &&str) -> bool {
        self.text == **other
    }
}

impl PartialEq<str> for Streamed {
    fn eq(&self, other: &str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for Streamed {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

impl std::ops::Deref for Streamed {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Streamed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.text.fmt(f)
    }
}

impl From<String> for Streamed {
    fn from(text: String) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        Self {
            seed: h.finish(),
            text,
            rev: 0,
        }
    }
}

impl From<&str> for Streamed {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    User {
        text: Streamed,
    },
    /// `queued=true` → pinned chip until the engine admits it (`prompt_queued`).
    Steer {
        text: Streamed,
        queued: bool,
    },
    /// Grows via `text_delta`.
    Assistant {
        text: Streamed,
    },
    /// Model reasoning, when the provider returns it. Billed on every turn
    /// (`EngineConfig.thinking` defaults true) and, before this, never shown
    /// — T3-15. Collapsed to `view::THINKING_COLLAPSED_LINES` unless
    /// `State.thinking_expanded`. Its own variant rather than part of
    /// `Assistant`: the spine marker and style differ, and the transcript is
    /// what `Scroll::At` indexes, so conflating them would make thinking
    /// un-skippable. Empty deltas create no item — until R3 sends
    /// `thinking.display: "summarized"` the text really is empty.
    Thinking {
        text: Streamed,
    },
    Tool {
        /// The engine's `tool_use` id (0037). Cards settle by id, never by
        /// name: N concurrent same-name calls would otherwise all land their
        /// `tool_done`s on the newest card and strand the other N−1.
        id: String,
        name: String,
        summary: String,
        status: ToolStatus,
        ticks: u64,
        /// Every call this card absorbed (0039 D5), the anchor included —
        /// INVARIANT: `id == calls[0].id`. Per-id settle state is what makes
        /// the merged status (D4) computable out of order.
        calls: Vec<ToolCall>,
        /// A spawn card's forwarded child calls (0039), rendered inside this
        /// item's block — item-indexed scroll never sees them.
        children: Vec<ChildCall>,
        /// The child's own words (0058 T2), tail-capped at
        /// [`CHILD_TEXT_CAP`]. Drill-in only, so outside the fingerprint
        /// invariant for the same reason the tick stamps are.
        child_text: String,
        /// The newest output line a running tool reported (0061 T25). `None`
        /// is exactly today's card: a tool with no sink, a settled one, or an
        /// older peer.
        progress: Option<ToolProgress>,
    },
    /// Retrying / fallback / compacted / controlled stops.
    Notice {
        text: Streamed,
    },
    /// A turn that failed outright (provider/transport error, sealed log, panic).
    /// Its own variant, not a `Notice`: an error must not read as muted chatter.
    Error {
        text: Streamed,
    },
    /// A multi-line block a command produced — `/context`. Raw numbers,
    /// never formatted strings: `view.rs` owns column alignment, so these
    /// tests can assert tokens instead of whitespace.
    Report(ContextReport),
    /// `/workflows` (0044): every run of the `workflow` tool this process has
    /// started, one row each. Parsed ad hoc from the payload like `ContextRow`.
    WorkflowsReport(Vec<WorkflowRun>),
    /// A plan the model handed over (`present_plan`, 0056 T3). Its own item
    /// rather than a `Notice`: it carries the two answers, and a plan the
    /// human is meant to act on must not read as muted chatter.
    Plan(PresentedPlan),
    /// One line closing a turn (0061 T9): how long it took, how many tool
    /// calls it made, and the wall time it finished at. The clock comes from
    /// the runtime — the core has none — so `finished_at` is `None` under an
    /// older peer and in tests.
    TurnSummary {
        secs: u64,
        calls: usize,
        finished_at: Option<String>,
    },
}

/// What approving a plan sends. One sentence, and it says *how* to work the
/// plan — an approval that only said "go" would leave the model to guess
/// whether to batch the steps.
pub const PLAN_APPROVED_PROMPT: &str =
    "Approved. Implement the plan, one step at a time, verifying each.";

/// The plan card's content, plus whether it is still the live one — an
/// answered card stays in the transcript as a record, without the keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PresentedPlan {
    pub summary: String,
    /// `(content, verify-or-acceptance, dependencies)` per step, resolved on
    /// the way in so the view renders rather than reasons.
    pub steps: Vec<PlanStep>,
    pub path: Option<String>,
    /// Cleared the moment the human approves, revises, or sends anything.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanStep {
    pub content: String,
    /// The command that proves it, else the acceptance sentence.
    pub proof: Option<String>,
    pub after: Vec<String>,
}

/// One `workflow` run as `/workflows` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowRun {
    pub name: String,
    /// `running` | `done` | `failed` | `cancelled`, verbatim from the wire.
    pub status: String,
    pub tokens: u64,
    pub elapsed_ms: u64,
    pub phases: Vec<WorkflowPhase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowPhase {
    pub title: String,
    /// Agents settled (done or failed) / started so far, and how many failed.
    pub settled: usize,
    pub started: usize,
    pub failed: usize,
}

/// How much of a child's own prose a spawn card keeps. The tail, not the
/// head: what a child is saying now is what a human watching it wants.
pub const CHILD_TEXT_CAP: usize = 4000;

/// The two tool cards that own a row of the agent band (0039/0044): a
/// running `spawn` or `workflow` call, whose forwarded children the drill-in
/// shows. Three sites test this; one helper keeps them agreeing.
pub fn is_agent_card(name: &str) -> bool {
    matches!(name, "spawn" | "workflow")
}

/// A `/context` answer, ready to render. Everything a row needs is here; the
/// view resolves labels, colors and widths.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextReport {
    pub model: String,
    pub window: u64,
    /// The provider's exact figure for the last turn; `None` before the first
    /// turn, and then the report simply omits that line.
    pub reported: Option<u64>,
    /// The sum of ALL rows, including the zeros `rows` drops.
    pub estimated: u64,
    /// Canonical order, zero rows already dropped.
    pub rows: Vec<(ContextKind, u64)>,
    /// `window - max(estimated, reported)`. Taking the max is what keeps the
    /// estimator's overcount bias pointing the safe way: `/context` may
    /// understate your remaining room, never overstate it.
    pub free: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    /// Approved, but waiting on the subprocess budget (0061 T24). `ahead` is
    /// how many callers were queued in front. No clock: nothing has started.
    Queued {
        ahead: usize,
    },
    Running,
    Done,
    Failed,
    Denied,
    AutoAllowed {
        rule: String,
    },
}

/// A running tool's newest output line and the counts behind it (0061 T25).
/// One `Option` rather than three loose fields, so "no progress" is one check
/// and cannot half-exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProgress {
    pub tail: String,
    pub lines: u64,
    /// Not rendered; kept because the frame carries it and a later surface
    /// (the inspector) will want it.
    pub bytes: u64,
    /// The card's own tick when this landed — what `quiet Ns` counts from.
    pub at_ticks: u64,
}

/// A backoff the surface is counting down (0061 T22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retry {
    pub attempt: u64,
    /// The ladder's ceiling; `None` from a peer that does not send one.
    pub max: Option<u64>,
    /// `HTTP 429`, or the first words of the reason when there was no status.
    pub status: String,
    /// The sleep the provider said it would take; `None` from an older peer,
    /// which simply gets no countdown.
    pub delay_ticks: Option<u64>,
    pub ticks: u64,
}

/// One call absorbed into a tool card (0039 D5). `ok: None` = outstanding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub ok: Option<bool>,
    /// What the model received (0061 T1). `None` = an older peer that sends
    /// no counts, which renders as no result row rather than `0 lines`.
    pub lines: Option<u64>,
    pub bytes: Option<u64>,
}

/// A sub-agent's tool call, nested on its spawn card (0039).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildCall {
    pub id: String,
    pub name: String,
    pub summary: String,
    /// `None` = running; `Some(ok)` = settled.
    pub ok: Option<bool>,
    /// Parent-card tick stamps for the drill-in's per-call durations.
    /// Deliberately outside the fingerprint invariant: `item_block` never
    /// reads them (the drill-in renders outside the cache, D7).
    pub started_at: u64,
    pub settled_at: Option<u64>,
    /// Token total from the done frame (0044) — drill-in only, so outside the
    /// fingerprint invariant for the same reason as the stamps above.
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Follow,
    At(usize),
}

/// A row of the agent band (0043): `main` or one spawn card, by anchor id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BandRow {
    Main,
    Spawn(String),
}

/// Running totals across every turn in this session. Per-turn usage is
/// overwritten by design (the strip shows one line); these are what a human
/// actually budgets against.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SessionUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Cache writes, cumulative. The signal a write-heavy, read-poor session
    /// shows (GPT-5.6 bills writes at 1.25×; Anthropic likewise) — 0046.
    pub cache_written: u64,
    /// Last completed turn's split, for the /cost hit-rate line — a cold
    /// cache mid-session is the biggest latency bug we can have and the
    /// cumulative counter hides it. Overwritten, never accumulated.
    pub last_input: u64,
    pub last_cache_read: u64,
    /// Accumulated only across turns that reported a price. `None` means no
    /// turn ever did — the UI must then show nothing rather than `$0.00`.
    pub cost_usd: Option<f64>,
}

impl SessionUsage {
    /// Fold one turn's usage payload in. Absent keys count as zero, which is
    /// what `TokenUsage`'s own `#[serde(default)]` fields already mean.
    pub fn add(&mut self, usage: &Value) {
        let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        self.input += n("input_tokens");
        self.output += n("output_tokens");
        self.cache_read += n("cache_read_input_tokens");
        self.cache_written += n("cache_creation_input_tokens");
        self.last_input = n("input_tokens") + n("cache_creation_input_tokens");
        self.last_cache_read = n("cache_read_input_tokens");
        if let Some(c) = usage.get("cost_usd").and_then(Value::as_f64) {
            *self.cost_usd.get_or_insert(0.0) += c;
        }
    }
}

/// The library's context window, used until the handshake reports the real
/// one (`initialize`'s `contextWindow`). Only a fallback: an older server
/// that does not report it still leaves the client rendering something
/// honest rather than dividing by zero.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

#[derive(Debug)]
pub struct State {
    pub phase: Phase,
    /// Whole-turn clock for the activity animation. Unlike `Phase::*.ticks`
    /// (which reset on every sub-phase change), this advances on every tick the
    /// turn is actually moving — thinking, writing, *and* tool sub-phases alike
    /// — and pauses while a prompt is blocked on you. Reset to 0 at every turn
    /// end (→ `Idle`), so each new working turn restarts the animation's cycle
    /// from the beginning. This is what drives the travel→look→reverse cycle in
    /// `anim`; the per-phase `ticks` still drive the "· 3s ·" elapsed readouts.
    pub work_ticks: u64,
    pub transcript: Vec<TranscriptItem>,
    pub scroll: Scroll,
    pub editor: Editor,
    pub vim_mode: bool,
    pub model: String,
    /// Set on the prompt result (real usage; streaming shows `chars/4`).
    pub usage_line: Option<String>,
    /// Running count of `tool_flagged` notices (0036) — bypass-mode calls
    /// allowed or refused with a ⚑ instead of an ask. Never clears
    /// mid-session, so an unattended run's flags survive scrollback.
    pub flag_count: u64,
    /// Running totals across every turn, the basis of `usage_line`.
    pub session_usage: SessionUsage,
    pub help_open: bool,
    /// Row offset into an ask/question/help body taller than the frame;
    /// reset whenever a modal opens. The view clamps the top.
    pub modal_scroll: usize,
    /// A draft the user entered before the session opened (0033 Task 8b):
    /// `pre_open_input` sets it instead of submitting — there is no session
    /// to send to yet — and `fire_queued_submit` replays it through the
    /// normal path the moment the session opens.
    pub queued_submit: bool,
    /// First Esc sent a cancel; suppresses duplicate notices until the result.
    pub interrupt_sent: bool,
    /// Turns the user detached from (second Esc) whose prompt results are
    /// still in flight. Everything a detached turn emits is absorbed until
    /// its result arrives and decrements this — the phase belongs to the
    /// user now, and nothing the dead turn says may take it back.
    pub detached_turns: u32,
    /// `tool_auto_allowed` arrives before its `tool_start`; each rule parks
    /// here, keyed by call id, until its card exists — a parallel chunk
    /// resolves every gate before any call starts, so several can be pending
    /// at once (0037).
    pub pending_auto_rules: std::collections::HashMap<String, String>,
    /// Display name (badge + titles); seeded from the open handshake,
    /// updated by `/rename`.
    pub session_name: Option<String>,
    /// Effective permission mode (`ask` | `bypass` | `dontask`).
    /// Seeded from the open handshake and corrected by every `mode_changed`
    /// notification, so it is what the engine enforces rather than what the
    /// user asked for. `/mode` updates it optimistically; the notification is
    /// what makes an engine coercion visible.
    pub mode: String,
    /// Plan mode, the axis orthogonal to `mode`: file edits always ask.
    /// Same seed-then-correct shape, via `plan_changed`. No coercion exists
    /// for it, so the optimistic update is always the one that sticks.
    pub plan: bool,
    /// Reasoning depth. `None` = the provider's own default, which is a real
    /// setting and not merely "unknown" — `/effort default` restores it. Same
    /// seed-then-correct shape as `plan`, via `effort_changed`.
    pub effort: Option<String>,
    /// The session's resolved starting effort from the open handshake
    /// (0030 Task 8), display-only: a bare `/effort` with nothing explicitly
    /// set reports this instead of the lie "default". Never sent anywhere —
    /// the engine already holds the same resolved value.
    pub default_effort: Option<String>,
    /// Session spend against `[behavior] max_cost_usd` (0059 T5), from the
    /// last `budget_notice`. `None` = no cap, or none crossed yet.
    pub budget: Option<(f64, f64)>,
    /// The per-phase effort schedule (0059 T1), pre-rendered by the server.
    /// Reported by `/effort` and `/status` so a rung that moves between turns
    /// reads as configuration, not drift.
    pub effort_schedule: Option<String>,
    /// Model context window in tokens, from the handshake. What the context
    /// gauge divides by; `DEFAULT_CONTEXT_WINDOW` until a server reports one.
    pub context_window: u64,
    /// What the last turn actually cost the provider — the exact figure a
    /// `/context` report shows beside its estimate. `on_prompt_result`
    /// computes it for the strip's `% ctx` segment and used to discard it.
    /// `None` until the first turn completes.
    pub live_context: Option<u64>,
    /// The engine's at-open context estimate, from the open reply (0040):
    /// what the strip's `% ctx` shows before any turn completes — a resumed
    /// session inherits real fullness, and a fresh session's seed items
    /// already occupy tokens. Superseded by `usage_line` the moment
    /// `on_prompt_result` builds one, and never shown as `/context`'s
    /// `reported` row — that row is provider-reported truth, this is an
    /// estimate (§5.7 in miniature).
    pub open_context: Option<u64>,
    /// Length of the open assistant bubble at the start of the current
    /// sample — where a mid-stream re-sample (0050 T3) truncates back to, so
    /// a half-written answer is not left above the whole one. Marked on
    /// every non-delta frame, which is the only sample boundary the wire
    /// makes visible.
    pub stream_mark: usize,
    /// Every loadable skill name, from the `initialize` result. `/<name>`
    /// resolves against this, so an unknown slash stays an unknown
    /// command instead of becoming a wasted turn.
    pub skills: Vec<String>,
    /// Every saved workflow recipe name (0044), from the same handshake.
    /// `/<name>` falls through to this after the built-ins and skills.
    pub workflows: Vec<String>,
    /// The rosters as advertised, kept so either `set_*` can rebuild the
    /// completion table without the other's input.
    skill_roster: Vec<(String, String)>,
    workflow_roster: Vec<(String, String)>,
    /// The skill the human requested via `/<name>`, held until the turn ends.
    /// If no successful `skill` load names it by then, the model silently
    /// skipped it — the one skill failure the per-load cards cannot show.
    pub pending_skill: Option<String>,
    /// Transcript spacing, from `[settings] density`. Drives the blank line
    /// between turns and the left-gutter width the role spine lives in.
    pub density: hotl_theme::Density,
    /// Prose wrap width from `[settings] measure`; `usize::MAX` = full width.
    pub measure: usize,
    /// The `todo_write` checklist, from `todos_changed` updates. Empty means
    /// either no list yet or the model cleared it — both render as nothing.
    pub todos: Vec<hotl_tools::todo::Todo>,
    /// The active `/goal` condition. Seeded from the open handshake (resume
    /// restores it) and corrected by every `goal_changed` — which also
    /// carries the engine's own clears (met/impossible verdicts).
    pub goal: Option<String>,
    /// Ticks since the goal was set/restored — the strip's elapsed readout.
    /// Ticks arrive only while a turn runs, so the clock pauses at idle.
    pub goal_ticks: u64,
    /// Turns the evaluator has judged, from `goal_verdict`.
    pub goal_turns: u64,
    /// The latest evaluator reason, shown by bare `/goal` so the human can
    /// see what the loop thinks is still missing without scrolling.
    pub goal_last_reason: Option<String>,
    /// What the harness observed each plan step's `validate_cmd` do, from the
    /// newest `goal_verdict` (0056 T4). Rendered by bare `/goal`.
    pub goal_evidence: Vec<String>,
    /// The loop's cumulative spend, read from `goal_verdict`'s `usage` —
    /// never summed locally, because the evaluator's own calls are in it.
    pub goal_usage: SessionUsage,
    /// Set by a `stalled` verdict, cleared by the next one: the goal is armed
    /// but resting, which the status line has to say out loud.
    pub goal_stalled: bool,
    /// The last goal that resolved this session: condition, outcome word,
    /// turns. Bare `/goal` shows it when nothing is active.
    pub goal_resolved: Option<(String, String, u64)>,
    /// The condition an engine-initiated `goal_changed(None)` just cleared,
    /// held until the verdict that explains it lands — the clear always
    /// arrives first, and by then `goal` no longer knows what resolved.
    pub goal_resolving: Option<String>,
    /// Every completable `/` command: the built-ins, plus one row per skill
    /// name the handshake advertised. Built once at startup.
    pub commands: Vec<complete::Command>,
    /// The open completion popup, or `None`. Derived from the editor buffer
    /// after every keystroke — never a mode that can outlive what is typed.
    pub completion: Option<Completion>,
    /// Esc closed the popup; suppresses it until the buffer stops being a
    /// `/` command, so the next fresh slash opens it again.
    pub dismissed: bool,
    /// `Ctrl-T`: show model reasoning in full rather than collapsed.
    /// Reasoning is context for a decision, not the decision — collapsed is
    /// the default posture.
    pub thinking_expanded: bool,
    /// `Ctrl-O`: show every tool card instead of folding settled runs into
    /// one rollup line. The twin of `thinking_expanded` for work (0061 T6).
    pub tools_expanded: bool,
    /// Ticks since the last frame the wire delivered (0061 T19). The core has
    /// no clock, so this *is* the clock: it counts only while a turn runs,
    /// and a `quiet Ns` past `anim::QUIET_AFTER` is the one honest thing a
    /// surface can say when nothing has arrived.
    pub since_frame: u64,
    /// A provider backoff in flight (0061 T22). Nothing is computing during
    /// one, so the countdown *is* the liveness; cleared by the first frame
    /// that proves the re-send landed.
    pub retry: Option<Retry>,
    /// A model-backed fold is running (0061 T23, tracker #35). The actor is
    /// blocked for a hook and a model call — up to two minutes with nothing
    /// else on screen.
    pub compacting: bool,
    /// Compacted pastes riding the current draft (`paste::Attachment`),
    /// keyed positionally to their `[Image #N]` / `[Pasted text #N …]`
    /// tokens. Lives here rather than in `Editor` so `$EDITOR` round-trips
    /// and history recall (both replace the buffer via `set_text`) cannot
    /// orphan valid tokens. Cleared on every submit; a mangled token's
    /// entry is silently dropped at expansion (the orphan rule).
    pub attachments: Vec<paste::Attachment>,
    /// The live mouse drag, in *screen* cell coordinates rather than transcript
    /// offsets. Transient: it survives only until the next real user action
    /// (see the clearing rule at the top of `update`).
    pub selection: Option<select::Selection>,
    /// Lines copied by the last drag, shown in the hint until the next action
    /// clears it. There is no timer to expire it — the runtime's ticker is
    /// armed only while a turn runs, so an idle console would keep a timed
    /// notice forever.
    pub copy_notice: Option<usize>,
    /// The agent whose stream fills the region above the strip (0039): a
    /// spawn card's anchor id, `None` = main. Survives back-to-main and
    /// typing so you can watch a child while composing; a dangling id
    /// renders main.
    pub selected_agent: Option<String>,
    /// Line offset into the child stream; `None` follows the tail (D7).
    pub agent_scroll: Option<usize>,
    /// The highlighted agent-band row (0043 D3); `None` = disengaged. Id-based
    /// so a row settling above it never shifts the highlight onto another agent.
    pub band_cursor: Option<BandRow>,
}

impl State {
    pub fn new(vim_mode: bool, model: String) -> Self {
        State {
            phase: Phase::Idle,
            work_ticks: 0,
            transcript: Vec::new(),
            scroll: Scroll::Follow,
            editor: Editor::new(vim_mode),
            vim_mode,
            model,
            usage_line: None,
            flag_count: 0,
            session_usage: SessionUsage::default(),
            help_open: false,
            modal_scroll: 0,
            queued_submit: false,
            interrupt_sent: false,
            detached_turns: 0,
            pending_auto_rules: std::collections::HashMap::new(),
            session_name: None,
            mode: "ask".into(),
            plan: false,
            effort: None,
            default_effort: None,
            budget: None,
            effort_schedule: None,
            context_window: DEFAULT_CONTEXT_WINDOW,
            live_context: None,
            open_context: None,
            stream_mark: 0,
            skills: Vec::new(),
            workflows: Vec::new(),
            skill_roster: Vec::new(),
            workflow_roster: Vec::new(),
            pending_skill: None,
            density: hotl_theme::Density::default(),
            measure: 110,
            todos: Vec::new(),
            goal: None,
            goal_ticks: 0,
            goal_turns: 0,
            goal_last_reason: None,
            goal_evidence: Vec::new(),
            goal_usage: SessionUsage::default(),
            goal_stalled: false,
            goal_resolved: None,
            goal_resolving: None,
            commands: complete::builtins(),
            completion: None,
            dismissed: false,
            thinking_expanded: false,
            tools_expanded: false,
            since_frame: 0,
            retry: None,
            compacting: false,
            attachments: Vec::new(),
            selection: None,
            copy_notice: None,
            selected_agent: None,
            agent_scroll: None,
            band_cursor: None,
        }
    }

    /// The agent band's spawn rows (0043 D1), transcript order: running while
    /// the turn is live, plus the shown and the highlighted spawn, pinned
    /// until left.
    pub fn band_spawns(&self) -> Vec<&TranscriptItem> {
        let live = self.phase != Phase::Idle;
        let pinned = |id: &str| {
            self.selected_agent.as_deref() == Some(id)
                || matches!(&self.band_cursor, Some(BandRow::Spawn(c)) if c == id)
        };
        self.transcript
            .iter()
            .filter(|i| {
                matches!(i, TranscriptItem::Tool { id, name, status, .. }
                    if is_agent_card(name)
                        && ((live && matches!(status, ToolStatus::Running | ToolStatus::AutoAllowed { .. }))
                            || pinned(id)))
            })
            .collect()
    }

    /// Seed the loadable-skill roster and the completion table from it.
    ///
    /// One path for both the open handshake and `/reload`, so `skills` (what
    /// `/<name>` dispatch resolves against) and `commands` (what the popup
    /// offers) can never disagree about which skills exist.
    pub fn set_skills(&mut self, skills: Vec<(String, String)>) {
        self.skills = skills.iter().map(|(name, _)| name.clone()).collect();
        self.skill_roster = skills;
        self.rebuild_commands();
    }

    /// The saved-workflow roster (0044), same contract as `set_skills`. A
    /// recipe named like a built-in or a skill is hidden from completion —
    /// dispatch precedence (builtin > skill > workflow) would never reach it.
    pub fn set_workflows(&mut self, workflows: Vec<(String, String)>) {
        self.workflows = workflows.iter().map(|(name, _)| name.clone()).collect();
        self.workflow_roster = workflows;
        self.rebuild_commands();
    }

    fn rebuild_commands(&mut self) {
        self.commands = complete::builtins();
        let row = |(name, description): &(String, String)| complete::Command {
            name: name.clone(),
            description: description.clone(),
            builtin: false,
        };
        self.commands.extend(self.skill_roster.iter().map(row));
        let shadowed: Vec<String> = self.commands.iter().map(|c| c.name.clone()).collect();
        self.commands.extend(
            self.workflow_roster
                .iter()
                .filter(|(name, _)| !shadowed.contains(name))
                .map(row),
        );
    }

    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        State::new(true, "test-model".into())
    }
}

#[derive(Debug, PartialEq)]
pub enum Msg {
    /// The `update` object from a `session/update` notification.
    Update(Value),
    PermissionRequest {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
        /// The proposed change, when the server sent one — empty for every
        /// ask until the engine's ask carries tool input (RQ-2).
        diff: Vec<DiffLine>,
    },
    QuestionRequest {
        req_id: u64,
        question: Question,
    },
    EgressRequest {
        req_id: u64,
        host: String,
    },
    PromptResult {
        outcome_kind: String,
        outcome_text: Option<String>,
        usage: Value,
        /// Local `HH:MM`, stamped by the runtime (0061 T9). `None` off unix
        /// and in tests, where the summary simply omits the clock.
        finished_at: Option<String>,
    },
    /// The server refused a steer — image validation, most often. The
    /// transcript's pinned "queued" chip must not outlive this.
    SteerRejected {
        why: String,
    },
    Key(KeyEvent),
    /// Bracketed-paste payload. Literal text, never keys — see `Msg::Key`.
    /// A multi-line paste used to arrive as one `Enter` per line and submit
    /// one turn per line.
    Paste(String),
    /// Transcript scroll from a key or the mouse wheel. Vim's `j`/`k` reach
    /// the same `scroll::apply` via `EditorEvent::Scroll*`.
    Scroll(crate::scroll::Intent),
    Tick,
    /// `$EDITOR` result; `None` = unchanged/aborted.
    /// `Ok(None)` = unchanged or aborted; `Err` = the editor never ran, which
    /// is a different thing and has to be said out loud rather than look like
    /// a no-op.
    EditorDone(Result<Option<String>, String>),
    /// Left button pressed: anchor a new selection at this cell.
    SelectStart {
        col: u16,
        row: u16,
    },
    /// Left button dragged: move the selection head. One per cell crossed.
    SelectExtend {
        col: u16,
        row: u16,
    },
    /// Left button released: copy, unless the drag never left its anchor.
    SelectEnd,
    /// The runtime finished a copy and reports how much reached the clipboard.
    /// `0` means the region held nothing worth copying.
    Copied {
        lines: usize,
    },
    /// The runtime re-read the client-side half of `config.toml`
    /// (`Cmd::ReloadSettings`). Theme, mouse and copy-on-select live in the
    /// runtime's own locals; these three live in `State`.
    SettingsReloaded {
        vim_mode: bool,
        density: hotl_theme::Density,
        measure: usize,
        warnings: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// The wire-bound draft: paste tokens already expanded, `[Image #N]`
    /// tokens inline, image paths riding with `data: None` until the
    /// runtime seam reads and encodes the files.
    SendPrompt(paste::PromptPayload),
    SendSteer(paste::PromptPayload),
    /// Send `session/rename` (fire-and-forget; the ack is noise).
    Rename(String),
    /// Send `session/set_mode` (fire-and-forget; the ack is noise). Payload
    /// is the mode name (`"ask" | "bypass" | "dontask"`) — already validated
    /// by `slash_command` before this is emitted.
    SetMode(String),
    /// Send `session/set_plan` (fire-and-forget). The other permission axis.
    SetPlan(bool),
    /// Send `session/set_effort` (fire-and-forget). `None` = back to the
    /// provider's default, sent as the wire word `"default"`.
    SetEffort(Option<String>),
    /// Send `session/set_goal` (fire-and-forget; the ack is noise — the
    /// engine's `goal_changed` broadcast is what corrects the optimism).
    /// `None` clears.
    SetGoal(Option<String>),
    Cancel,
    ReplyPermission {
        req_id: u64,
        allow: bool,
        /// Plan 0022: approved *and* the credential read-deny lifted for this
        /// one command. Only ever set from the `s` key on a bash ask.
        secret_reads: bool,
        message: Option<String>,
    },
    /// Answer a `session/request_question`. Exactly one of `selected`
    /// (a single label — v1 is single-select even when `multi` was set) or
    /// `free_text` is populated.
    /// Answer a `session/request_egress`. Two answers only, both scoped to
    /// this session: hotl does not write `config.toml`, so a permanent grant
    /// stays a deliberate edit (plan 0026 decision 9).
    ReplyEgress {
        req_id: u64,
        allow: bool,
    },
    ReplyQuestion {
        req_id: u64,
        selected: Vec<String>,
        free_text: Option<String>,
    },
    /// Send `session/reload_config` (fire-and-forget; the engine broadcasts
    /// `config_reloaded`, which is what the client actually acts on).
    ReloadConfig,
    /// Send `session/context` (fire-and-forget; the ack is noise — the engine
    /// broadcasts `context_report`, which is what the client acts on).
    RequestContext,
    /// Send `session/workflows` (0044; same fire-and-forget shape — the
    /// engine broadcasts `workflows_report`).
    RequestWorkflows,
    /// Re-read the client-side half of `config.toml` — theme, density, vim
    /// mode, mouse. The runtime owns this one: `hotl-tui` never touches the
    /// filesystem.
    ReloadSettings,
    OpenEditor(String),
    SetTitle(String),
    /// Append a submitted prompt to the on-disk history file (the runtime
    /// owns the file; the core just names what to persist).
    AppendHistory(String),
    /// Copy this screen region to the clipboard. The core names the region;
    /// the runtime resolves it against the rendered buffer and writes OSC 52,
    /// then reports back as `Msg::Copied`.
    CopySelection(select::Selection),
    Quit,
}

/// Terminal-tab title: `hotl` / `hotl · <name>`, plus a state suffix.
fn title(state: &State, suffix: &str) -> String {
    match &state.session_name {
        Some(n) => format!("hotl · {n}{suffix}"),
        None => format!("hotl{suffix}"),
    }
}

/// Did this message come off the wire? Anything that did is proof the engine
/// is alive, whatever it said (0061 T19).
fn from_wire(msg: &Msg) -> bool {
    matches!(
        msg,
        Msg::Update(_)
            | Msg::PermissionRequest { .. }
            | Msg::QuestionRequest { .. }
            | Msg::EgressRequest { .. }
            | Msg::PromptResult { .. }
            | Msg::SteerRejected { .. }
    )
}

pub fn update(state: &mut State, msg: Msg) -> Vec<Cmd> {
    if from_wire(&msg) {
        state.since_frame = 0;
    }
    // A selection is a region of the *screen*, so any deliberate user action
    // retires it — but not the two message kinds that arrive on their own
    // schedule. Excluding `Update` is what lets a drag work mid-turn, and it
    // is safe to exclude: the highlight is painted at fixed cells and the copy
    // scrapes the live buffer, so the two agree even as text moves underneath.
    //
    // INVARIANT: a live drag survives arriving stream tokens. Enforced by
    // `streaming_updates_do_not_clear_a_live_drag`.
    if !matches!(
        &msg,
        Msg::SelectStart { .. }
            | Msg::SelectExtend { .. }
            | Msg::SelectEnd
            | Msg::Copied { .. }
            | Msg::Tick
            | Msg::Update(_)
    ) {
        state.selection = None;
        state.copy_notice = None;
    }
    // A detached turn (second Esc) is dead to the UI but alive on the wire
    // until its prompt result arrives. The wire is FIFO, so everything it
    // emits lands before that result: absorb it all here — except durable
    // session state — so nothing a dead turn says can reclaim the phase the
    // user took back. Its asks go unanswered on purpose: their reply channels
    // die with the cancelled turn and the server prunes them.
    if state.detached_turns > 0 {
        match &msg {
            Msg::Update(v) => {
                let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
                // The reload pair joins the durable-state exceptions: a
                // `/reload` issued after an esc-esc detach replaces the session
                // outright, and swallowing that would leave the badge, the
                // model and the skill roster describing an engine that is gone.
                if !matches!(
                    kind,
                    "mode_changed"
                        | "effort_changed"
                        | "todos_changed"
                        | "goal_changed"
                        | "goal_verdict"
                        // A plan presented while detached must be on screen
                        // when you come back: it is waiting on you.
                        | "plan_presented"
                        | "config_reloaded"
                        | "config_reload_failed"
                ) {
                    return Vec::new();
                }
            }
            Msg::PermissionRequest { .. }
            | Msg::QuestionRequest { .. }
            | Msg::EgressRequest { .. } => return Vec::new(),
            Msg::PromptResult { usage, .. } => {
                state.detached_turns -= 1;
                state.session_usage.add(usage);
                return Vec::new();
            }
            _ => {}
        }
    }
    match msg {
        Msg::Update(v) => on_update(state, &v),
        Msg::PermissionRequest {
            req_id,
            summary,
            protected_why,
            diff,
        } => {
            state.phase = Phase::WaitingAsk {
                req_id,
                summary,
                protected_why,
                input: String::new(),
                denying: false,
                diff,
            };
            // The ask owns the keyboard and the screen now. A popup or a live
            // reverse-i-search left over from mid-typing would steal the first
            // Esc, draw a stale menu under the "waiting on you" card, and
            // advertise keys `on_ask_key` ignores (tracker #13).
            state.completion = None;
            state.editor.clear_search();
            state.modal_scroll = 0;
            vec![Cmd::SetTitle(title(state, " — waiting on you"))]
        }
        Msg::QuestionRequest { req_id, question } => {
            state.phase = Phase::WaitingQuestion {
                req_id,
                header: question.header,
                prompt: question.prompt,
                options: question.options,
                input: String::new(),
                selected: 0,
            };
            // Same reasoning as the ask arm above (tracker #13).
            state.completion = None;
            state.editor.clear_search();
            state.modal_scroll = 0;
            vec![Cmd::SetTitle(title(state, " — waiting on you"))]
        }
        Msg::EgressRequest { req_id, host } => {
            state.phase = Phase::WaitingEgress { req_id, host };
            // Same reasoning as the ask arm above (tracker #13).
            state.completion = None;
            state.editor.clear_search();
            state.modal_scroll = 0;
            vec![Cmd::SetTitle(title(state, " — waiting on you"))]
        }
        Msg::PromptResult {
            outcome_kind,
            outcome_text,
            usage,
            finished_at,
        } => on_prompt_result(state, &outcome_kind, outcome_text, &usage, finished_at),
        Msg::SteerRejected { why } => {
            clear_newest_queued_steer(state);
            notice(state, format!("steer rejected: {why}"));
            Vec::new()
        }
        Msg::Key(key) => on_key(state, key),
        Msg::Paste(text) => {
            // A dropped image path or a 3+-line paste compacts to a token;
            // the content parks in the side table until submit. Numbering is
            // per-kind and per-draft. Everything else inserts literally —
            // `insert_text`'s never-submits invariant carries over either
            // way (tokens contain no newline).
            match paste::classify(&text) {
                paste::PasteKind::Image { path, media_type } => {
                    let n = 1 + state
                        .attachments
                        .iter()
                        .filter(|a| matches!(a, paste::Attachment::Image { .. }))
                        .count();
                    state.editor.insert_text(&paste::image_marker(n));
                    state
                        .attachments
                        .push(paste::Attachment::Image { path, media_type });
                }
                paste::PasteKind::Text { text, lines } => {
                    let n = 1 + state
                        .attachments
                        .iter()
                        .filter(|a| matches!(a, paste::Attachment::Paste { .. }))
                        .count();
                    state.editor.insert_text(&paste::paste_marker(n, lines));
                    state
                        .attachments
                        .push(paste::Attachment::Paste { text, lines });
                }
                paste::PasteKind::Literal => state.editor.insert_text(&text),
            }
            // INVARIANT: the editor's live-token set matches `State.attachments`.
            // Enforced by `backspace_swallows_a_token_only_while_its_attachment_lives`.
            state
                .editor
                .set_live_tokens(paste::live_tokens(&state.attachments));
            refresh(state);
            Vec::new()
        }
        Msg::Scroll(intent) => {
            scroll_route(state, intent);
            Vec::new()
        }
        Msg::Tick => {
            on_tick(state);
            Vec::new()
        }
        Msg::EditorDone(content) => {
            match content {
                Ok(Some(text)) => {
                    state.editor.set_text(text.trim_end_matches('\n'));
                    refresh(state);
                }
                Ok(None) => {}
                Err(why) => notice(state, why),
            }
            Vec::new()
        }
        Msg::SelectStart { col, row } => {
            state.selection = Some(select::Selection::new(col, row));
            state.copy_notice = None;
            Vec::new()
        }
        Msg::SelectExtend { col, row } => {
            if let Some(sel) = &mut state.selection {
                sel.head = (col, row);
            }
            Vec::new()
        }
        // The highlight deliberately stays up after the copy — it is the only
        // confirmation of *what* was copied. The clearing rule above retires it
        // on the next action.
        Msg::SelectEnd => match state.selection {
            Some(sel) if !sel.is_empty() => vec![Cmd::CopySelection(sel)],
            _ => {
                state.selection = None;
                Vec::new()
            }
        },
        Msg::Copied { lines } => {
            state.copy_notice = (lines > 0).then_some(lines);
            Vec::new()
        }
        // The runtime already applied the settings it owns (theme, mouse,
        // copy-on-select); these two live here. `vim_mode` also has to reach
        // the editor, which holds its own copy.
        Msg::SettingsReloaded {
            vim_mode,
            density,
            measure,
            warnings,
        } => {
            state.vim_mode = vim_mode;
            state.editor.set_vim_mode(vim_mode);
            state.density = density;
            state.measure = measure;
            for w in warnings {
                notice(state, w);
            }
            Vec::new()
        }
    }
}

/// A card's merge identity (0039 D3): the first ` · `-separated summary
/// segment — for `read`, the (already elided) path with the page details
/// stripped. Deterministic: both sides of the comparison are post-elision.
fn merge_key(summary: &str) -> &str {
    summary.split(" · ").next().unwrap_or(summary)
}

fn on_update(state: &mut State, v: &Value) -> Vec<Cmd> {
    let text_of = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    // `retrying` is excluded: it is the frame that *reads* the mark, and the
    // re-sample it announces resumes from the same place.
    if !matches!(kind, "text_delta" | "thinking_delta" | "retrying") {
        state.stream_mark = open_assistant_len(state);
    }
    match kind {
        "text_delta" => {
            append_assistant(state, &text_of("text"));
            state.retry = None;
            enter_streaming(state);
        }
        "tool_start" => {
            state.retry = None;
            let id = text_of("id");
            let status = match state.pending_auto_rules.remove(&id) {
                Some(rule) => ToolStatus::AutoAllowed { rule },
                None => ToolStatus::Running,
            };
            let name = text_of("name");
            // A queued card promotes in place (0061 T24), never at the tail:
            // it is the same call, and moving it would reorder the transcript
            // around whatever landed while it waited. A promoted card does not
            // merge — a queued run of same-key reads is N cards, not `×N`.
            if let Some(TranscriptItem::Tool {
                status: prev,
                ticks,
                ..
            }) = state.transcript.iter_mut().rev().find(|i| {
                matches!(i, TranscriptItem::Tool { id: card, status, .. }
                    if *card == id && matches!(status, ToolStatus::Queued { .. }))
            }) {
                *prev = status;
                *ticks = 0;
                state.phase = Phase::Tool { name, ticks: 0 };
                return Vec::new();
            }
            let summary = text_of("summary");
            // 0039 D3: a consecutive same-key call absorbs into the previous
            // card instead of stacking an identical row — adjacency alone
            // enforces the turn boundary. The running flavor becomes the
            // newest start's (two different auto-allow rules on one merged
            // card show the newest — cosmetic, D4). Ticks keep accumulating:
            // on_tick ticks any Running card in the turn.
            let key = merge_key(&summary);
            if let Some(TranscriptItem::Tool {
                name: prev_name,
                summary: prev_summary,
                status: prev_status,
                calls,
                children,
                ..
            }) = state.transcript.last_mut()
            {
                // Never into/out of an agent card, a card with children, or a
                // settled failure (a retry after failure starts fresh — D3).
                if *prev_name == name
                    && !is_agent_card(&name)
                    && children.is_empty()
                    && merge_key(prev_summary) == key
                    && !matches!(prev_status, ToolStatus::Failed | ToolStatus::Denied)
                {
                    calls.push(ToolCall {
                        id,
                        ok: None,
                        lines: None,
                        bytes: None,
                    });
                    *prev_summary = key.to_string();
                    *prev_status = status;
                    state.phase = Phase::Tool { name, ticks: 0 };
                    return Vec::new();
                }
            }
            state.transcript.push(TranscriptItem::Tool {
                id: id.clone(),
                name: name.clone(),
                summary,
                status,
                ticks: 0,
                calls: vec![ToolCall {
                    id,
                    ok: None,
                    lines: None,
                    bytes: None,
                }],
                children: Vec::new(),
                child_text: String::new(),
                progress: None,
            });
            state.phase = Phase::Tool { name, ticks: 0 };
        }
        // Settles by id, never by name (0037): with N concurrent same-name
        // calls, "the newest matching card" was whichever start happened to
        // land last — N−1 cards stranded as spinners forever. Since 0039 the
        // id lives in `calls` (a merged card holds several), and the card
        // settles only when every absorbed call has (D4: any failed → Failed).
        "tool_done" => {
            let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
            // Absent from an older peer, and absent is not zero: no counts
            // means no result row at all (0061 T1).
            let lines = v.get("lines").and_then(Value::as_u64);
            let bytes = v.get("bytes").and_then(Value::as_u64);
            let id = text_of("id");
            let mut found = false;
            if let Some(TranscriptItem::Tool {
                status,
                calls,
                progress,
                ..
            }) = state.transcript.iter_mut().rev().find(|i| {
                matches!(i, TranscriptItem::Tool { calls, .. }
                        if calls.iter().any(|c| c.id == id && c.ok.is_none()))
            }) {
                found = true;
                if let Some(call) = calls.iter_mut().find(|c| c.id == id && c.ok.is_none()) {
                    call.ok = Some(ok);
                    call.lines = lines;
                    call.bytes = bytes;
                }
                // The tail row gives way to the result row (0061 T25).
                *progress = None;
                if calls.iter().all(|c| c.ok.is_some()) {
                    *status = if calls.iter().any(|c| c.ok == Some(false)) {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Done
                    };
                }
            }
            band_tidy(state);
            if !found {
                // No card: the human answered *as* the tool (§2b Respond
                // skips execution, so no `tool_start` ever ran). The settle
                // is still worth a card — the call happened. An absorbed id
                // can never land here: its outstanding call is findable (D5).
                let id2 = id.clone();
                state.transcript.push(TranscriptItem::Tool {
                    id,
                    name: text_of("name"),
                    summary: text_of("name"),
                    status: if ok {
                        ToolStatus::Done
                    } else {
                        ToolStatus::Failed
                    },
                    ticks: 0,
                    calls: vec![ToolCall {
                        id: id2,
                        ok: Some(ok),
                        lines,
                        bytes,
                    }],
                    children: Vec::new(),
                    child_text: String::new(),
                    progress: None,
                });
            }
            settle_phase(state);
        }
        // A running tool's newest output line (0061 T25). Settles by id like
        // `tool_done`; deliberately no phase change and no `enter_streaming`
        // — it is not a `text_delta`, and the strip already follows the cards.
        "tool_progress" => {
            let id = text_of("id");
            if let Some(TranscriptItem::Tool {
                name,
                status,
                ticks,
                progress,
                ..
            }) = state.transcript.iter_mut().rev().find(|i| {
                matches!(i, TranscriptItem::Tool { calls, .. }
                    if calls.iter().any(|c| c.id == id && c.ok.is_none()))
            }) {
                // An agent card's second row is its own account of itself,
                // and a settled card is a late frame the drain raced (T12's
                // guard covers the engine side; this is belt and braces).
                if !is_agent_card(name)
                    && matches!(status, ToolStatus::Running | ToolStatus::AutoAllowed { .. })
                {
                    *progress = Some(ToolProgress {
                        tail: text_of("tail"),
                        lines: v.get("lines").and_then(Value::as_u64).unwrap_or(0),
                        bytes: v.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                        at_ticks: *ticks,
                    });
                }
            }
        }
        // Approved but waiting on the subprocess budget (0061 T24). Its own
        // card, parked without a clock, promoted in place by its `tool_start`.
        "tool_queued" => {
            let id = text_of("id");
            let ahead = v.get("ahead").and_then(Value::as_u64).unwrap_or(0) as usize;
            state.transcript.push(TranscriptItem::Tool {
                id: id.clone(),
                name: text_of("name"),
                summary: text_of("summary"),
                status: ToolStatus::Queued { ahead },
                ticks: 0,
                calls: vec![ToolCall {
                    id,
                    ok: None,
                    lines: None,
                    bytes: None,
                }],
                children: Vec::new(),
                child_text: String::new(),
                progress: None,
            });
        }
        // Denied tools never get a `tool_start` (the engine returns before
        // running them) — the denial itself is the card, its one call
        // already settled.
        "tool_denied" => {
            let name = text_of("name");
            let id = text_of("id");
            state.transcript.push(TranscriptItem::Tool {
                id: id.clone(),
                name: name.clone(),
                summary: name,
                status: ToolStatus::Denied,
                ticks: 0,
                calls: vec![ToolCall {
                    id,
                    ok: Some(false),
                    lines: None,
                    bytes: None,
                }],
                children: Vec::new(),
                child_text: String::new(),
                progress: None,
            });
            settle_phase(state);
        }
        // A sub-agent's forwarded tool activity (0039): nests under its own
        // spawn card, routed by parent_id. No parent card → drop silently
        // (reattach replays no tool frames — pre-existing gap). Deliberately
        // no `enter_streaming`: child activity must not perturb the parent's
        // phase.
        "child_tool" => {
            let parent_id = text_of("parent_id");
            if let Some(TranscriptItem::Tool {
                ticks, children, ..
            }) = state
                .transcript
                .iter_mut()
                .rev()
                .find(|i| matches!(i, TranscriptItem::Tool { id, .. } if *id == parent_id))
            {
                let id = text_of("id");
                match v.get("ok").and_then(Value::as_bool) {
                    None => children.push(ChildCall {
                        id,
                        name: text_of("name"),
                        summary: text_of("summary"),
                        ok: None,
                        started_at: *ticks,
                        settled_at: None,
                        tokens: None,
                    }),
                    Some(ok) => {
                        let tokens = v.get("tokens").and_then(Value::as_u64);
                        if let Some(c) = children
                            .iter_mut()
                            .rev()
                            .find(|c| c.id == id && c.ok.is_none())
                        {
                            c.ok = Some(ok);
                            c.settled_at = Some(*ticks);
                            c.tokens = tokens;
                        } else {
                            // Settled in one frame — a denied child never
                            // got a start.
                            children.push(ChildCall {
                                id,
                                name: text_of("name"),
                                summary: text_of("summary"),
                                ok: Some(ok),
                                started_at: *ticks,
                                settled_at: Some(*ticks),
                                tokens,
                            });
                        }
                    }
                }
            }
        }
        // A child's own words (0058 T2): appended to its spawn card's tail
        // for the drill-in. Like `child_tool`, no `enter_streaming` — child
        // prose must not perturb the parent's phase — and no parent card
        // means drop.
        "child_text" => {
            let parent_id = text_of("parent_id");
            if let Some(TranscriptItem::Tool { child_text, .. }) = state
                .transcript
                .iter_mut()
                .rev()
                .find(|i| matches!(i, TranscriptItem::Tool { id, .. } if *id == parent_id))
            {
                child_text.push_str(&text_of("text"));
                if child_text.chars().count() > CHILD_TEXT_CAP {
                    let keep = child_text.chars().count() - CHILD_TEXT_CAP;
                    *child_text = child_text.chars().skip(keep).collect();
                }
            }
        }
        // T3-15: thinking is billed on every turn and used to be dropped on
        // the floor (`_ => {}`). Deltas accumulate into one item so a burst
        // of reasoning is one collapsible block, not fifty.
        "thinking_delta" => {
            let delta = text_of("text");
            if !delta.is_empty() {
                match state.transcript.last_mut() {
                    Some(TranscriptItem::Thinking { text }) => text.push_str(&delta),
                    _ => state
                        .transcript
                        .push(TranscriptItem::Thinking { text: delta.into() }),
                }
                // 0061 T21: thinking is not writing. `enter_streaming` here
                // put the strip on `writing · ~0 tok` for the whole reasoning
                // pass, since `Streaming.chars` counts Assistant text only.
                // A `Tool` phase with running cards keeps the strip on the
                // cards (0049 T1b), so only `Streaming` is corrected.
                state.retry = None;
                if matches!(state.phase, Phase::Streaming { .. }) {
                    state.phase = Phase::Sampling { ticks: 0 };
                }
            }
        }
        "tool_auto_allowed" => {
            state
                .pending_auto_rules
                .insert(text_of("id"), text_of("rule"));
        }
        // The bypass floor's notification (0036): the flag *is* the ask's
        // replacement, so it rides a Notice (no new TranscriptItem kind — a
        // new kind breaks two view.rs matches at once) plus a running chip
        // the strip keeps for the whole session.
        "tool_flagged" => {
            state.flag_count += 1;
            let summary = text_of("summary");
            let why = text_of("why");
            let denied = v.get("denied").and_then(Value::as_bool).unwrap_or(false);
            notice(
                state,
                if denied {
                    format!("⚑ refused: {summary} — {why}")
                } else {
                    format!("⚑ allowed with notice: {summary} — {why}")
                },
            );
        }
        "todos_changed" => {
            state.todos = v
                .get("items")
                .cloned()
                .and_then(|items| serde_json::from_value(items).ok())
                .unwrap_or_default();
        }
        // Seed-then-correct like `mode_changed`: covers a change made on
        // another attached surface AND the engine's own clears (a met or
        // impossible verdict). A confirmation of this surface's optimistic
        // update leaves the counters alone.
        "goal_changed" => {
            let goal = v.get("goal").and_then(Value::as_str).map(str::to_string);
            if goal != state.goal {
                state.goal_ticks = 0;
                state.goal_turns = 0;
                state.goal_last_reason = None;
                state.goal_evidence.clear();
                state.goal_usage = SessionUsage::default();
                state.goal_stalled = false;
                // An engine-initiated clear is a resolution whose verdict is
                // still in flight: stash what it cleared, since `goal` is
                // about to stop knowing. A local `/goal clear` already set
                // `goal` to None, so this never fires for one.
                state.goal_resolving = match (&goal, &state.goal) {
                    (None, Some(c)) => Some(c.clone()),
                    _ => None,
                };
            }
            state.goal = goal;
        }
        // The verdict rides as a Notice — no new view arms, and the reason
        // is worth keeping in the transcript. `turns` comes from the event,
        // not local state: on met/impossible the `goal_changed` clear (which
        // resets counters) lands first.
        "goal_verdict" => {
            let turns = v.get("turns").and_then(Value::as_u64).unwrap_or(0);
            state.goal_turns = turns;
            let reason = text_of("reason");
            let verdict = text_of("verdict");
            state.goal_last_reason = (!reason.is_empty()).then(|| reason.clone());
            state.goal_evidence = v
                .get("evidence")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            // Cumulative, evaluator included — replace, never accumulate.
            if let Some(usage) = v.get("usage") {
                state.goal_usage = SessionUsage::default();
                state.goal_usage.add(usage);
            }
            state.goal_stalled = verdict == "stalled";
            if let Some(word) = match verdict.as_str() {
                "met" => Some("achieved"),
                "impossible" => Some("impossible"),
                "error" => Some("failed"),
                _ => None,
            } {
                // The condition the clear took away, or — for a surface that
                // never saw the clear — the one still on screen.
                if let Some(condition) = state.goal_resolving.take().or_else(|| state.goal.clone())
                {
                    state.goal_resolved = Some((condition, word.into(), turns));
                }
            }
            notice(
                state,
                match verdict.as_str() {
                    "not_yet" => format!("◎ goal check (turn {turns}): not yet — {reason}"),
                    "met" => format!("◎ goal achieved after {turns} turn(s) — {reason}"),
                    "impossible" => {
                        format!("◎ goal impossible after {turns} turn(s) — {reason}")
                    }
                    // The goal stays set here: only `goal_changed` clears it.
                    "stalled" => format!(
                        "◎ goal paused after {turns} turn(s) without a tool call — it stays \
                         set; your next prompt re-arms it"
                    ),
                    "error" => format!(
                        "◎ goal cleared after an unrecoverable error — {reason}; run /goal \
                         again to continue"
                    ),
                    _ => "◎ goal check failed — no verdict; the goal stays active".into(),
                },
            );
        }
        "retrying" => {
            let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
            // The re-sample regenerates from the mark, so anything rendered
            // since is about to be said again (0050 T3).
            if v.get("discarded_partial").and_then(Value::as_bool) == Some(true) {
                truncate_open_assistant(state);
            }
            notice(
                state,
                format!("retrying (attempt {attempt}) — {}", text_of("reason")),
            );
            // The countdown the strip runs (0061 T22): the phase kept
            // animating its old text through the whole backoff, which read as
            // work in progress when nothing at all was computing.
            let reason = text_of("reason");
            state.retry = Some(Retry {
                attempt,
                max: v.get("max").and_then(Value::as_u64),
                status: match v.get("status").and_then(Value::as_u64) {
                    Some(code) => format!("HTTP {code}"),
                    None => reason.chars().take(24).collect(),
                },
                delay_ticks: v
                    .get("delay_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| ms * crate::anim::TICK_HZ / 1000),
                ticks: 0,
            });
        }
        "fallback_model" => {
            state.model = text_of("model");
            notice(state, format!("model fallback → {}", state.model));
        }
        // Server-side truth, not a client guess: the badge showed "ask" while
        // the shipped default ran "bypass" (evaluation §5.7). Optimistic
        // /mode updates are corrected here when the engine coerces them
        // (a security-enforced build forces Bypass→Ask).
        // INVARIANT: `state.mode` is what the engine enforces, never what the
        // user asked for. Enforced by `mode_changed_updates_the_badge_state`.
        "mode_changed" => state.mode = text_of("mode"),
        // The other axis. No coercion exists for it, so this only ever
        // confirms the optimistic update — or carries a change another
        // attached surface made.
        "plan_changed" => state.plan = v.get("plan").and_then(Value::as_bool).unwrap_or(false),
        // Null is meaningful here, not missing: it is "the provider's own
        // default", which is exactly what `/effort default` sets.
        "effort_changed" => {
            state.effort = v.get("effort").and_then(Value::as_str).map(str::to_string);
            if state.effort.is_none() {
                // Cleared (here or on another attached surface): the session
                // default no longer describes this session — see `set_effort`.
                state.default_effort = None;
            }
        }
        // `/reload` landed: the engine now runs a scaffold built from the
        // config on disk, and the session was re-opened onto it. Everything
        // here is server-side truth — the client re-seeds rather than guesses,
        // exactly as it does at the open handshake.
        "config_reloaded" => {
            state.model = text_of("model");
            state.mode = text_of("mode");
            state.plan = v.get("plan").and_then(Value::as_bool).unwrap_or(false);
            if let Some(w) = v
                .get("context_window")
                .and_then(Value::as_u64)
                .filter(|&w| w > 0)
            {
                state.context_window = w;
            }
            state.set_skills(crate::client::parse_skills(v));
            state.set_workflows(crate::client::parse_workflows(v));
            for w in v
                .get("warnings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                notice(state, w.to_string());
            }
            notice(
                state,
                format!(
                    "config reloaded — model {} · mode {} · {} skill(s). \
                     [sandbox], [network] and the thread pools are process-wide — restart to change those.",
                    state.model,
                    state.mode,
                    state.skills.len()
                ),
            );
        }
        // The rebuild failed (a typo in config.toml, an unreachable provider).
        // The engine deliberately kept running the old scaffold, and saying so
        // is the whole point: a silent failure here reads as "reloaded".
        "config_reload_failed" => notice(
            state,
            format!(
                "config reload failed: {} — the previous config is still live",
                text_of("reason")
            ),
        ),
        // The answer to `/context`. Arrives as a broadcast, so it can land on
        // a surface that never asked — harmless, it is read-only information
        // about a session both surfaces share.
        "context_report" => push_context_report(state, v),
        "workflows_report" => push_workflows_report(state, v),
        // 0056 T3. Only one plan is ever live: a second `present_plan`
        // supersedes the first, so the old card loses its keys.
        "plan_presented" => {
            for item in state.transcript.iter_mut() {
                if let TranscriptItem::Plan(p) = item {
                    p.live = false;
                }
            }
            let steps = v
                .get("nodes")
                .and_then(Value::as_array)
                .map(|ns| ns.iter().map(plan_step).collect())
                .unwrap_or_default();
            state.transcript.push(TranscriptItem::Plan(PresentedPlan {
                summary: v
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                steps,
                path: v.get("path").and_then(Value::as_str).map(str::to_string),
                live: true,
            }));
        }
        "prompt_queued" => clear_newest_queued_steer(state),
        // The spend meter (0059 T5): the strip shows it from here on, and the
        // threshold itself is worth a line in the transcript.
        "budget_notice" => {
            let num = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            let (used, cap) = (num("used_usd"), num("cap_usd"));
            state.budget = Some((used, cap));
            let pct = v.get("pct").and_then(Value::as_u64).unwrap_or(0);
            notice(
                state,
                format!(
                    "session spend {pct}% of budget (${used:.2} of ${cap:.2}) — \
                     raise [behavior] max_cost_usd or start a new session"
                ),
            );
        }
        "compacting" => state.compacting = true,
        "compacted" => {
            state.compacting = false;
            let degraded = v.get("degraded").and_then(Value::as_bool).unwrap_or(false);
            notice(
                state,
                if degraded {
                    "history folded — degraded".into()
                } else {
                    "history folded".into()
                },
            );
        }
        // The rung below `compacted`: no history was lost, so the notice says
        // so rather than sounding like a fold.
        "cleared" => {
            state.compacting = false;
            let count = v.get("count").and_then(Value::as_u64).unwrap_or(0);
            notice(state, format!("cleared {count} results"));
        }
        // `turn_done` rides in the prompt result; thinking stays in Sampling.
        _ => {}
    }
    Vec::new()
}

/// Turn a `context_report` payload into a `TranscriptItem::Report`.
///
/// A payload whose `rows` will not deserialize degrades to a notice, never to
/// an empty table: an empty table reads as "your context is empty", which is a
/// worse lie than admitting the report could not be read.
fn push_context_report(state: &mut State, v: &Value) {
    let Some(rows) = v
        .get("rows")
        .cloned()
        .and_then(|r| serde_json::from_value::<Vec<ContextRow>>(r).ok())
    else {
        notice(
            state,
            "could not read the context report the engine sent".into(),
        );
        return;
    };
    // A present `window` is used verbatim, zero included — only an absent one
    // falls back. The engine's window is the one that governs compaction.
    let window = v
        .get("window")
        .and_then(Value::as_u64)
        .unwrap_or(state.context_window);
    // Every row counts toward the total, including the ones the table hides
    // and the ones this binary does not recognize.
    let estimated = rows.iter().map(|r| r.tokens).sum::<u64>();
    let reported = state.live_context;
    let mut display: Vec<(ContextKind, u64)> = rows
        .into_iter()
        .filter(|r| r.tokens > 0)
        .map(|r| (r.kind, r.tokens))
        .collect();
    // Sorted here rather than trusted from the wire: display order is this
    // client's business.
    display.sort_by_key(|(kind, _)| *kind);
    state.transcript.push(TranscriptItem::Report(ContextReport {
        model: state.model.clone(),
        window,
        reported,
        estimated,
        free: window.saturating_sub(estimated.max(reported.unwrap_or(0))),
        rows: display,
    }));
}

/// Turn a `workflows_report` payload into a `TranscriptItem::WorkflowsReport`.
/// Same honesty rule as `push_context_report`: a payload without a `runs`
/// array is a notice, never an empty table.
fn push_workflows_report(state: &mut State, v: &Value) {
    let Some(runs) = v.get("runs").and_then(Value::as_array) else {
        notice(
            state,
            "could not read the workflows report the engine sent".into(),
        );
        return;
    };
    if runs.is_empty() {
        notice(state, "no workflows have run in this process".into());
        return;
    }
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let u64_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let rows = runs
        .iter()
        .map(|r| WorkflowRun {
            name: str_of(r, "name"),
            status: str_of(r, "status"),
            tokens: u64_of(r, "tokens"),
            elapsed_ms: u64_of(r, "elapsed_ms"),
            phases: r
                .get("phases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|p| {
                    let agents: Vec<&Value> = p
                        .get("agents")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .collect();
                    let statuses: Vec<&str> = agents
                        .iter()
                        .map(|a| a.get("status").and_then(Value::as_str).unwrap_or(""))
                        .collect();
                    WorkflowPhase {
                        title: str_of(p, "title"),
                        settled: statuses
                            .iter()
                            .filter(|s| matches!(**s, "done" | "failed"))
                            .count(),
                        started: agents.len(),
                        failed: statuses.iter().filter(|s| **s == "failed").count(),
                    }
                })
                .collect(),
        })
        .collect();
    state.transcript.push(TranscriptItem::WorkflowsReport(rows));
}

fn on_prompt_result(
    state: &mut State,
    kind: &str,
    text: Option<String>,
    usage: &Value,
    finished_at: Option<String>,
) -> Vec<Cmd> {
    // A turn that streamed nothing still shows its outcome text.
    if turn_chars(&state.transcript) == 0 {
        if let Some(t) = text.as_deref().filter(|t| kind == "done" && !t.is_empty()) {
            state
                .transcript
                .push(TranscriptItem::Assistant { text: t.into() });
        }
    }
    // A real execution error gets its own loud item; controlled stops
    // (cancelled / turn_limit / refused / …) stay muted notices.
    if kind == "error" {
        let msg = text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        state.transcript.push(TranscriptItem::Error {
            text: msg.unwrap_or("the turn failed").into(),
        });
    } else if let Some(n) = outcome_notice(kind, text.as_deref()) {
        notice(state, n);
    }
    // Warn only when the model never called the tool — a ✓/✗ card is its own
    // feedback — and only on a clean finish.
    if let Some(requested) = state.pending_skill.take() {
        if kind == "done" && !skill_addressed_this_turn(&state.transcript, &requested) {
            notice(
                state,
                format!(
                    "skill `{requested}` was not loaded — the model didn't call the skill \
                     tool this turn. Re-run /{requested}, or ask it to load the skill."
                ),
            );
        }
    }
    // The turn's own closing line, last so it reads as the full stop it is.
    // `work_ticks` is still this turn's; it resets below.
    state.transcript.push(TranscriptItem::TurnSummary {
        secs: state.work_ticks / crate::anim::TICK_HZ,
        calls: turn_tool_calls(&state.transcript),
        finished_at,
    });
    state.session_usage.add(usage);
    // What the *next* turn starts from: everything resident in this turn's
    // context. Computed here, not in `format_usage`, so the formatter stays a
    // formatter — `/context` wants the number, not the string.
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let live = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
    state.live_context = Some(live);
    state.usage_line = Some(format_usage(state, usage));
    state.phase = Phase::Idle;
    band_tidy(state);
    state.work_ticks = 0;
    state.interrupt_sent = false;
    state.retry = None;
    state.compacting = false;
    vec![Cmd::SetTitle(title(state, ""))]
}

/// Every tool call this turn made — merged cards count all their absorbed
/// calls, since each one really ran.
fn turn_tool_calls(transcript: &[TranscriptItem]) -> usize {
    transcript
        .iter()
        .rev()
        .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
        .filter_map(|i| match i {
            TranscriptItem::Tool { calls, .. } => Some(calls.len()),
            _ => None,
        })
        .sum()
}

/// Did any `skill` card name `requested` this turn (the trailing run since the
/// last user prompt)? Only its absence is worth a warning.
fn skill_addressed_this_turn(transcript: &[TranscriptItem], requested: &str) -> bool {
    transcript
        .iter()
        .rev()
        .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
        .any(|i| {
            matches!(i, TranscriptItem::Tool { name, summary, .. }
                if name == "skill" && skill_summary_loads(summary, requested))
        })
}

/// A `skill` summary that *loads* `requested`, not a `search:`/`list:` browse.
/// Leaf-name compare, so bare `/brainstorming` matches `superpowers:brainstorming`.
fn skill_summary_loads(summary: &str, requested: &str) -> bool {
    let Some(body) = summary.strip_prefix("skill ") else {
        return false;
    };
    let body = body.trim();
    if body.starts_with("search:") || body.starts_with("list:") || body == "list" {
        return false;
    }
    skill_leaf(body) == skill_leaf(requested)
}

/// A skill name without its `source:` qualifier.
fn skill_leaf(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name).trim()
}

fn outcome_notice(kind: &str, text: Option<&str>) -> Option<String> {
    Some(match kind {
        "done" => return None,
        "cancelled" => "turn cancelled".into(),
        // Name the knob: hitting this looks like an unexplained stop, and the
        // fix ([behavior] max_turns, or -1 for no cap) is not guessable.
        "turn_limit" => "turn limit reached — raise [behavior] max_turns (-1 = no cap)".into(),
        "refused" => "provider refused the request".into(),
        // Both carry their own sentence on the wire (`wire::outcome_frame`),
        // which already names the knob and the next action.
        "denial_spiral" | "budget" => text
            .unwrap_or("the session budget stopped the turn")
            .to_string(),
        other => format!("{other}: {}", text.unwrap_or(""))
            .trim_end_matches([':', ' '])
            .to_string(),
    })
}

/// Compact token count: verbatim below 1000, else one decimal with a `k`/`M`
/// suffix. The strip has one line to spend, so `12.0k` beats `12000`.
pub(crate) fn tok(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// The strip's usage line: session totals, this turn's context fullness, and
/// cost when — and only when — the payload reported one.
///
/// Per-turn input/output is overwritten by design (the strip shows one line);
/// the *totals* are what a human actually budgets against. The context gauge
/// reads the latest turn, because that is what the next turn will carry.
///
/// INVARIANT: no cost segment is rendered unless the turn result carried
/// `cost_usd` — the UI never estimates prices. R4 owns the catalog that
/// populates it (see the plan's RQ table). Enforced by
/// `cost_is_shown_only_when_the_payload_carries_it`.
/// Context fullness as a whole percentage, capped at 100; `None` on a zero
/// window. The one fullness formula: the strip's ctx chip
/// (`view::strip_chips`) and `/context` cannot drift.
pub(crate) fn ctx_pct(live: u64, window: u64) -> Option<u64> {
    (live * 100).checked_div(window).map(|pct| pct.min(100))
}

fn format_usage(state: &State, usage: &Value) -> String {
    let u = &state.session_usage;
    let mut parts = vec![
        format!("{} in", tok(u.input)),
        format!("{} out", tok(u.output)),
    ];
    if u.cache_read > 0 {
        parts.push(format!("{} cached", tok(u.cache_read)));
    }
    // Strip width is scarce: writes show only once there are some.
    if u.cache_written > 0 {
        parts.push(format!("{} written", tok(u.cache_written)));
    }
    // Per-turn, not accumulated (a session-wide average would blur a cold
    // first turn into a warm tenth one) — present only when this turn had
    // cache activity to report (§S1 cache telemetry).
    if let Some(ratio) = usage.get("hit_ratio").and_then(Value::as_f64) {
        parts.push(format!("{:.0}% hit", ratio * 100.0));
    }
    // The context share is the strip's ctx chip (0049 T1), read from
    // `State.live_context` — not a part of this line.
    if let Some(cost) = u.cost_usd {
        parts.push(format!("${cost:.2}"));
    }
    parts.join(" · ")
}

/// True while something else must own the keyboard and the screen: a live
/// reverse-i-search (the input box is showing its prompt, not the buffer),
/// or a permission ask / structured question (`on_ask_key` / `on_question_key`
/// intercept every key before it ever reaches here). The popup must not be
/// shown, and must not intercept keys, in either case.
fn modal_active(state: &State) -> bool {
    state.editor.search_prompt().is_some()
        || matches!(
            state.phase,
            Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. } | Phase::WaitingEgress { .. }
        )
}

/// Recompute the popup from the editor buffer. Called after every key that
/// reaches the editor and after a splice, so the popup is always a function
/// of what is actually typed. A buffer that is no longer a `/` command
/// re-arms `dismissed` — that is the only thing that clears it. While
/// something else owns the keyboard (`modal_active`) the popup stays closed
/// regardless of what the buffer says.
fn refresh(state: &mut State) {
    // 0043: typing disengages the band cursor.
    if !state.editor.is_empty() {
        state.band_cursor = None;
    }
    if modal_active(state) {
        state.completion = None;
        return;
    }
    let text = state.editor.text();
    if !text.starts_with('/') {
        state.dismissed = false;
    }
    state.completion = complete::recompute(
        &state.commands,
        &text,
        state.editor.cursor(),
        state.dismissed,
    );
}

/// Splice the highlighted command into the buffer. The spliced text carries a
/// trailing space, so `refresh` closes the popup on the way out.
fn accept_selected(state: &mut State) {
    let Some(idx) = state
        .completion
        .as_ref()
        .and_then(|c| c.matches.get(c.selected).copied())
    else {
        return;
    };
    let Some(name) = state.commands.get(idx).map(|c| c.name.clone()) else {
        return;
    };
    let text = complete::accept(&state.editor.text(), state.editor.cursor(), &name);
    state.editor.set_text(&text);
    refresh(state);
}

fn on_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    // Ctrl-C outranks every transient owner of the keyboard (help overlay,
    // popup, modals): idle it quits, busy it interrupts, and once an
    // interrupt is pending — from either key — it quits outright, so two
    // presses always suffice to leave.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.help_open = false;
        if state.phase == Phase::Idle || state.interrupt_sent {
            return vec![Cmd::Quit];
        }
        state.interrupt_sent = true;
        // No notice (0061 T20): the strip says `interrupting` and the hint
        // names the second press — a transcript item said it a third time.
        return vec![Cmd::Cancel];
    }
    // The help table scrolls on the arrow and page keys (0049 T7); every
    // other key closes it.
    if state.help_open {
        match key.code {
            KeyCode::PageUp | KeyCode::PageDown => {
                modal_page(state, key);
            }
            KeyCode::Up => state.modal_scroll = state.modal_scroll.saturating_sub(1),
            KeyCode::Down => state.modal_scroll = state.modal_scroll.saturating_add(1),
            _ => state.help_open = false,
        }
        return Vec::new();
    }
    // Ctrl-T expands model reasoning. Above the editor for the same reason as
    // the scroll keys: `Editor::handle` swallows every Ctrl chord it does not
    // itself bind.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        state.thinking_expanded = !state.thinking_expanded;
        return Vec::new();
    }
    // Ctrl-O unfolds the tool rollups, the same way and for the same reason.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
        state.tools_expanded = !state.tools_expanded;
        return Vec::new();
    }
    if matches!(state.phase, Phase::WaitingAsk { .. }) {
        return on_ask_key(state, key);
    }
    if matches!(state.phase, Phase::WaitingQuestion { .. }) {
        return on_question_key(state, key);
    }
    if matches!(state.phase, Phase::WaitingEgress { .. }) {
        return on_egress_key(state, key);
    }
    // Transcript scrolling, unconditional — not gated on vim mode, which is
    // the whole defect (`vim.rs::vertical` was the only emitter and needs
    // `[behavior] vim_mode = true`). PageUp/PageDown have no meaning in a
    // ten-row input box, so they need no layering; Ctrl-Home/Ctrl-End is the
    // document-start/end convention, leaving bare Home/End as line motions.
    // This sits above the editor because the generic Ctrl swallow in
    // `Editor::handle` would otherwise eat Ctrl-Home.
    // INVARIANT: reachable with `[behavior] vim_mode = false`. Enforced by
    // `page_keys_scroll_the_transcript_without_vim_mode`.
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let intent = match key.code {
        KeyCode::PageUp => Some(crate::scroll::Intent::Up(crate::scroll::PAGE)),
        KeyCode::PageDown => Some(crate::scroll::Intent::Down(crate::scroll::PAGE)),
        KeyCode::Home if ctrl => Some(crate::scroll::Intent::Top),
        KeyCode::End if ctrl => Some(crate::scroll::Intent::Bottom),
        _ => None,
    };
    if let Some(intent) = intent {
        scroll_route(state, intent);
        return Vec::new();
    }
    // The esc ladder (0042 D2, 0043 D5): child stream shown → back to main
    // (band cursor kept); band cursor engaged → disengage; else a busy turn
    // interrupts. Under vim this fires only from Normal with nothing pending
    // (Insert esc always reaches the editor); an open completion popup keeps
    // its own esc, layered below.
    if key.code == KeyCode::Esc && esc_at_top_level(state) {
        if state.selected_agent.is_some() {
            state.selected_agent = None;
            state.agent_scroll = None;
            band_tidy(state);
            return Vec::new();
        }
        if state.band_cursor.is_some() {
            state.band_cursor = None;
            return Vec::new();
        }
        if state.phase != Phase::Idle && state.completion.is_none() {
            return interrupt_or_detach(state);
        }
    }
    // The plan card's two answers (0056 T3). Guarded on an empty draft like
    // `?` above, so a human mid-sentence still just types an `a`.
    if state.editor.is_empty() && live_plan(state).is_some() {
        match key.code {
            KeyCode::Char('a') => {
                settle_plan(state);
                notice(state, "plan approved — leaving plan mode".into());
                return vec![
                    Cmd::SetPlan(false),
                    Cmd::SendPrompt(crate::paste::PromptPayload::text_only(
                        PLAN_APPROVED_PROMPT.into(),
                    )),
                ];
            }
            KeyCode::Char('r') => {
                settle_plan(state);
                notice(
                    state,
                    "revising — say what to change and the model replans".into(),
                );
                return Vec::new();
            }
            _ => {}
        }
    }
    if key.code == KeyCode::Char('?') && state.editor.is_empty() {
        state.help_open = true;
        state.modal_scroll = 0;
        return Vec::new();
    }
    // The popup owns these four keys while it is open. Esc is layered — it
    // dismisses here and only reaches the editor's Insert→Normal transition
    // on a second press. Enter splices and then falls through, so submitting
    // takes its ordinary path.
    if let Some(c) = &state.completion {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let last = c.matches.len().saturating_sub(1);
                let next = if key.code == KeyCode::Up {
                    c.selected.saturating_sub(1)
                } else {
                    (c.selected + 1).min(last)
                };
                if let Some(c) = &mut state.completion {
                    c.selected = next;
                }
                return Vec::new();
            }
            KeyCode::Esc => {
                state.dismissed = true;
                state.completion = None;
                return Vec::new();
            }
            KeyCode::Tab => {
                accept_selected(state);
                return Vec::new();
            }
            KeyCode::Enter => accept_selected(state),
            _ => {}
        }
    }
    // 0043 D2: the agent band owns ↑/↓ on an empty input — non-vim whenever
    // the band shows, vim from Normal only (Insert keeps history recall).
    // Sits above the editor so `handle_insert`'s recall never sees the key.
    let band_keys = state.editor.is_empty()
        && !state.band_spawns().is_empty()
        && (!state.vim_mode || state.editor.mode() == crate::vim::Mode::Normal);
    if band_keys && matches!(key.code, KeyCode::Up | KeyCode::Down) {
        band_move(state, key.code == KeyCode::Up);
        return Vec::new();
    }
    // 0043 D3: Enter on the highlighted row opens it — a spawn's stream, or
    // main. The cursor stays engaged.
    if key.code == KeyCode::Enter && state.editor.is_empty() {
        if let Some(row) = state.band_cursor.clone() {
            state.selected_agent = match row {
                BandRow::Spawn(id) => Some(id),
                BandRow::Main => None,
            };
            state.agent_scroll = None;
            band_tidy(state);
            return Vec::new();
        }
    }
    let event = state.editor.handle(key);
    refresh(state);
    match event {
        EditorEvent::Submit(text) if text.trim().is_empty() => {
            // The editor already reset its buffer; stale attachments must
            // not leak their numbering into the next draft.
            state.attachments.clear();
            // INVARIANT: the editor's live-token set matches `State.attachments`.
            // Enforced by `empty_submit_syncs_live_tokens_so_a_stale_look_alike_stays_prose`.
            state
                .editor
                .set_live_tokens(paste::live_tokens(&state.attachments));
            Vec::new()
        }
        EditorEvent::Submit(text) => {
            // Expand while the side table is alive — it dies with this
            // draft. The wire gets paste content + image paths; disk history
            // gets the fully-expanded text (exactly the bytes pre-compaction
            // behavior wrote, so a recalled entry is self-contained).
            let history_text = paste::expand_for_history(&text, &state.attachments);
            // INVARIANT: the recall ring holds fully-expanded text, so a
            // recalled prompt is self-contained. Enforced by
            // `recalling_an_image_prompt_replays_the_path_not_a_dead_token`.
            state.editor.remember(history_text.clone());
            let payload = paste::expand_for_wire(&text, &state.attachments);
            state.attachments.clear();
            // INVARIANT: the editor's live-token set matches `State.attachments`.
            // Enforced by `normal_submit_syncs_live_tokens_so_a_stale_look_alike_stays_prose`.
            state
                .editor
                .set_live_tokens(paste::live_tokens(&state.attachments));
            let cmds = submit(state, text.clone(), payload);
            // Persist only prompt-starting submissions (they emit SendPrompt),
            // and only when the literal text wasn't a slash command — a skill
            // invocation desugars to a prompt but shouldn't leave its `/name`
            // (or the expanded template) on disk. In-session recall still walks
            // everything via the editor's own ring.
            let starts_turn = cmds.iter().any(|c| matches!(c, Cmd::SendPrompt(_)));
            if starts_turn && !text.trim_start().starts_with('/') {
                let mut out = vec![Cmd::AppendHistory(history_text)];
                out.extend(cmds);
                out
            } else {
                cmds
            }
        }
        EditorEvent::OpenExternal(text) => vec![Cmd::OpenEditor(text)],
        // Vim j/k (0043 D4): the agent band's cursor while the band shows,
        // else one item of whichever stream is shown. The wheel and page
        // keys keep calling `scroll_route` — they never touch the band.
        EditorEvent::ScrollUp => {
            if state.band_spawns().is_empty() {
                scroll_route(state, crate::scroll::Intent::Up(1));
            } else {
                band_move(state, true);
            }
            Vec::new()
        }
        EditorEvent::ScrollDown => {
            if state.band_spawns().is_empty() {
                scroll_route(state, crate::scroll::Intent::Down(1));
            } else {
                band_move(state, false);
            }
            Vec::new()
        }
        EditorEvent::None => Vec::new(),
    }
}

/// 0033 Task 8b: composer-only handling for the pre-open window — the
/// terminal is up but the session is not. Typing, editing and paste behave
/// exactly as normal; Enter marks the draft queued instead of submitting
/// (`fire_queued_submit` replays it on open); everything session-addressed
/// — slash commands included — stays inert, buffered as text.
pub fn pre_open_input(state: &mut State, msg: Msg) {
    match msg {
        Msg::Key(key) => {
            let event = state.editor.handle(key);
            refresh(state);
            if let EditorEvent::Submit(text) = event {
                // The editor already reset its buffer; put the draft back so
                // the open transition submits exactly what was typed.
                state.editor.set_text(&text);
                if !text.trim().is_empty() {
                    state.queued_submit = true;
                }
            }
            // Scroll/external-editor events are inert: there is no transcript
            // to scroll and no suspend machinery running yet.
        }
        // The paste arm of `update` touches only the editor and the
        // attachment side table — safe pre-open, and reusing it keeps the
        // marker bookkeeping in one place.
        Msg::Paste(_) => {
            let _ = update(state, msg);
        }
        _ => {}
    }
}

/// The open transition (0033 Task 8b): replay the queued draft through the
/// normal Enter path — history append, token expansion, exactly one
/// `Cmd::SendPrompt` — as if the user had pressed Enter right now.
pub fn fire_queued_submit(state: &mut State) -> Vec<Cmd> {
    if !std::mem::take(&mut state.queued_submit) {
        return Vec::new();
    }
    on_key(state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

/// Is this esc the app's to walk the ladder with (0042 D2)? Vim: Normal with
/// nothing pending — Insert esc always reaches the editor and never
/// interrupts. Non-vim: the shipped empty-editor rule, unchanged.
fn esc_at_top_level(state: &State) -> bool {
    if state.vim_mode {
        state.editor.mode() == crate::vim::Mode::Normal && !state.editor.pending_active()
    } else {
        state.editor.is_empty()
    }
}

/// The band cursor (0043 D3) over rows `[main, spawn₁..spawnₙ]`, clamped at
/// both ends. Disengaged, it engages at the shown row and then moves, so ↓
/// from main lands on spawn₁ in one press. An empty band never engages.
fn band_move(state: &mut State, up: bool) {
    let mut rows = vec![BandRow::Main];
    rows.extend(state.band_spawns().into_iter().filter_map(|i| match i {
        TranscriptItem::Tool { id, .. } => Some(BandRow::Spawn(id.clone())),
        _ => None,
    }));
    if rows.len() == 1 {
        return;
    }
    let shown = state
        .selected_agent
        .as_deref()
        .map(|id| BandRow::Spawn(id.into()))
        .unwrap_or(BandRow::Main);
    let from = state
        .band_cursor
        .as_ref()
        .and_then(|c| rows.iter().position(|r| r == c))
        .or_else(|| rows.iter().position(|r| *r == shown))
        .unwrap_or(0);
    let to = if up {
        from.saturating_sub(1)
    } else {
        (from + 1).min(rows.len() - 1)
    };
    state.band_cursor = Some(rows[to].clone());
    band_tidy(state);
}

/// A cursor has nothing to sit in once the band is gone, and must not
/// resurface when the next turn spawns.
fn band_tidy(state: &mut State) {
    if state.band_spawns().is_empty() {
        state.band_cursor = None;
    }
}

/// Route a scroll intent (0039 D8): the shown child stream's line offset
/// when one is selected, the transcript otherwise.
fn scroll_route(state: &mut State, intent: crate::scroll::Intent) {
    if state.selected_agent.is_none() {
        crate::scroll::apply(state, intent);
        return;
    }
    // Line-indexed over the drill-in's rows; the view clamps again at render,
    // so an over-estimate here only costs a keypress at the boundary.
    let len = state
        .selected_agent
        .as_deref()
        .and_then(|want| {
            state.transcript.iter().find_map(|i| match i {
                TranscriptItem::Tool { id, children, .. } if id == want => Some(children.len() + 2),
                _ => None,
            })
        })
        .unwrap_or(0);
    let cur = state.agent_scroll.unwrap_or(len);
    state.agent_scroll = match intent {
        crate::scroll::Intent::Top => Some(0),
        crate::scroll::Intent::Bottom => None,
        crate::scroll::Intent::Up(n) => Some(cur.saturating_sub(n)),
        crate::scroll::Intent::Down(n) => {
            let next = cur.saturating_add(n);
            if next >= len {
                None
            } else {
                Some(next)
            }
        }
    };
}

/// The Esc ladder: the first press asks the engine to cancel; the second
/// stops waiting for the answer. Control returns unconditionally — the
/// client must never depend on the server to hand the prompt line back.
fn interrupt_or_detach(state: &mut State) -> Vec<Cmd> {
    if !state.interrupt_sent {
        state.interrupt_sent = true;
        return vec![Cmd::Cancel];
    }
    state.detached_turns += 1;
    state.phase = Phase::Idle;
    state.retry = None;
    state.compacting = false;
    band_tidy(state);
    state.work_ticks = 0;
    state.interrupt_sent = false;
    notice(
        state,
        "control is yours — the interrupted turn is abandoned".into(),
    );
    // One more cancel for the road: harmless if the turn is already dead,
    // decisive if the first one raced the wire.
    vec![Cmd::Cancel, Cmd::SetTitle(title(state, ""))]
}

/// The display/wire fork: the transcript keeps `text` (tokens and all — what
/// the human typed), the wire gets `payload` (tokens expanded, image paths
/// riding). A built-in slash command drops the payload; the skill desugar
/// re-uses its images.
fn submit(state: &mut State, text: String, payload: paste::PromptPayload) -> Vec<Cmd> {
    if let Some(rest) = text.trim().strip_prefix('/') {
        return slash_command(state, rest, payload);
    }
    // Anything the human sends answers a live plan card — usually a revision.
    settle_plan(state);
    if state.phase == Phase::Idle {
        state
            .transcript
            .push(TranscriptItem::User { text: text.into() });
        state.phase = Phase::Sampling { ticks: 0 };
        state.scroll = Scroll::Follow;
        // The new turn's silence starts at the send, not at the last frame of
        // the turn before it.
        state.since_frame = 0;
        vec![
            Cmd::SendPrompt(payload),
            Cmd::SetTitle(title(state, " — working")),
        ]
    } else {
        state.transcript.push(TranscriptItem::Steer {
            text: text.into(),
            queued: true,
        });
        vec![Cmd::SendSteer(payload)]
    }
}

/// The TUI's slash commands. Built-ins resolve first; an unmatched
/// `/<skill>` asks the model to load that skill, which is the human
/// override for skills the tool description no longer names. Anything
/// else is a transcript notice — unresolved slash input never reaches the
/// model.
fn slash_command(state: &mut State, rest: &str, payload: paste::PromptPayload) -> Vec<Cmd> {
    let (cmd, arg) = rest
        .split_once(char::is_whitespace)
        .map(|(c, a)| (c, a.trim()))
        .unwrap_or((rest.trim(), ""));
    match cmd {
        "rename" => {
            // The one source of truth for what a session name may be — the
            // same function `acp.rs` validates against, so the TUI and the
            // wire can never disagree (this file already imports
            // `hotl_tools::rules::PermissionMode` for exactly that reason).
            let Some(name) = hotl_types::normalize_session_name(arg) else {
                notice(state, "usage: /rename <name> (1–64 chars)".into());
                return Vec::new();
            };
            state.session_name = Some(name.clone());
            notice(state, format!("session renamed to {name}"));
            let suffix = if state.phase == Phase::Idle {
                ""
            } else {
                " — working"
            };
            vec![Cmd::Rename(name), Cmd::SetTitle(title(state, suffix))]
        }
        // A toggle, not a mode switch: plan composes with whatever `/mode`
        // says. `on`/`off` are for scripted input, where a toggle is a race.
        "plan" => {
            let want = match arg.trim() {
                "" => !state.plan,
                "on" | "true" => true,
                "off" | "false" => false,
                _ => {
                    notice(state, "usage: /plan [on|off]".into());
                    return Vec::new();
                }
            };
            set_plan(state, want)
        }
        "mode" => {
            // Delegate to `PermissionMode::from_str` — the same parser ACP's
            // `session/set_mode` uses — so the TUI and the wire protocol
            // share one source of truth on what a valid mode name is
            // (including the `dont_ask`/`dont-ask` aliases a hand-rolled
            // list here previously rejected). The canonical `as_str()` form
            // is what gets stored/sent, so the badge and the wire payload
            // never disagree with what the alias actually meant.
            // `/mode plan` predates the split; send the user to `/plan`
            // rather than calling their old muscle memory a typo.
            if hotl_tools::rules::is_legacy_plan_word(arg.trim()) {
                notice(state, "plan is now its own toggle — use /plan".into());
                return Vec::new();
            }
            let Some(mode) = hotl_tools::rules::PermissionMode::from_str(arg.trim()) else {
                notice(state, "usage: /mode <ask|bypass|dontask>".into());
                return Vec::new();
            };
            set_mode(state, mode.as_str())
        }
        // Reports rather than cycles when bare: a five-rung cycle is
        // unguessable, unlike `/plan`'s two-state toggle.
        "effort" => match arg.trim() {
            "" => {
                let report = format!("effort {}", effort_report(state));
                notice(state, report);
                Vec::new()
            }
            "default" | "unset" | "none" => set_effort(state, None),
            other => match other.parse::<hotl_tools::agents::Effort>() {
                Ok(e) => set_effort(state, Some(e.as_str())),
                Err(_) => {
                    notice(
                        state,
                        "usage: /effort <low|medium|high|xhigh|max|default>".into(),
                    );
                    Vec::new()
                }
            },
        },
        // `/goal <condition>` sets/replaces, bare reports, and any of the
        // clear words ends it early. Optimistic like `/mode`; the engine's
        // `goal_changed` broadcast is the correction channel.
        "goal" => match arg.trim() {
            "" => {
                let report = match (&state.goal, &state.goal_resolved) {
                    // Armed and resting: a stall keeps the goal but stops
                    // spending, and the line has to say which of the two it is.
                    (Some(c), _) => format!(
                        "◎ goal {} · {}m · {} turn(s) · {} in / {} out — {c}",
                        if state.goal_stalled {
                            "armed (paused after idle turns)"
                        } else {
                            "active"
                        },
                        state.goal_ticks / (60 * crate::anim::TICK_HZ),
                        state.goal_turns,
                        tok(state.goal_usage.input),
                        tok(state.goal_usage.output),
                    ),
                    (None, Some((c, word, turns))) => {
                        format!("◎ last goal {word} after {turns} turn(s) — {c}")
                    }
                    (None, None) => format!(
                        "no goal set — /goal <condition> keeps the turn going until the \
                         condition is satisfied. Checks the harness decides itself, no \
                         model involved: {}",
                        hotl_types::CONDITION_LEAVES
                    ),
                };
                notice(state, report);
                // The evaluator's own words, on their own line: what it says
                // is missing is the most actionable thing on the screen.
                if state.goal.is_some() {
                    if let Some(reason) = state.goal_last_reason.clone() {
                        notice(state, format!("last check: {reason}"));
                    }
                    // What the harness itself saw each validate_cmd do —
                    // the line that says why a `met` was refused.
                    for line in state.goal_evidence.clone() {
                        notice(state, format!("  {line}"));
                    }
                }
                Vec::new()
            }
            "clear" | "stop" | "off" | "reset" | "none" | "cancel" => {
                if let Some(condition) = state.goal.take() {
                    state.goal_ticks = 0;
                    state.goal_turns = 0;
                    state.goal_last_reason = None;
                    state.goal_evidence.clear();
                    state.goal_usage = SessionUsage::default();
                    state.goal_stalled = false;
                    notice(state, format!("goal cleared: {condition}"));
                    vec![Cmd::SetGoal(None)]
                } else {
                    notice(state, "no goal set".into());
                    Vec::new()
                }
            }
            // The one validator every entry point funnels through, so the
            // TUI and the wire cannot disagree on what a goal may be.
            _ => match hotl_types::normalize_goal(arg) {
                Some(goal) => {
                    state.goal = Some(goal.clone());
                    state.goal_ticks = 0;
                    state.goal_turns = 0;
                    state.goal_last_reason = None;
                    state.goal_evidence.clear();
                    state.goal_usage = SessionUsage::default();
                    state.goal_stalled = false;
                    // Idle: the condition is the directive (0048), submitted
                    // behind the set — the engine's command channel is FIFO,
                    // so the goal is armed before the turn it gates is
                    // admitted. Not via `submit`: a condition may begin with
                    // `/` (a path) and must never read as a slash command.
                    // Mid-turn: set only; the gate fires when this turn ends.
                    if state.phase == Phase::Idle {
                        notice(state, format!("◎ goal set — working toward it now: {goal}"));
                        state.transcript.push(TranscriptItem::User {
                            text: goal.clone().into(),
                        });
                        state.phase = Phase::Sampling { ticks: 0 };
                        state.scroll = Scroll::Follow;
                        vec![
                            Cmd::SetGoal(Some(goal.clone())),
                            Cmd::SendPrompt(paste::PromptPayload::text_only(goal)),
                            Cmd::SetTitle(title(state, " — working")),
                        ]
                    } else {
                        notice(
                            state,
                            format!("◎ goal set — the turn keeps going until it is met: {goal}"),
                        );
                        vec![Cmd::SetGoal(Some(goal))]
                    }
                }
                None => {
                    notice(
                        state,
                        "usage: /goal <condition> (1–4000 chars) | /goal clear".into(),
                    );
                    Vec::new()
                }
            },
        },
        // Re-read `config.toml` without losing the session. The settings half
        // goes first so the theme flips at once while the engine rebuild — a
        // provider handshake and a skill walk — is still in flight.
        //
        // Idle-only, deliberately: a rebuild replaces the session, and the
        // reply a running turn is mid-way through producing would die with it.
        // Abandoning a turn stays the user's call (the esc ladder), never a
        // side effect of a command about configuration.
        "reload" => {
            if state.phase != Phase::Idle {
                notice(
                    state,
                    "/reload needs an idle session — finish the turn or press esc twice".into(),
                );
                return Vec::new();
            }
            notice(state, "reloading config…".into());
            vec![Cmd::ReloadSettings, Cmd::ReloadConfig]
        }
        // `?` only opens help while the buffer is empty, so the moment you
        // have typed anything help is unreachable — a discoverability bug,
        // not a new feature.
        "help" => {
            state.help_open = true;
            state.modal_scroll = 0;
            Vec::new()
        }
        // The single highest-value "what am I actually running?" answer, and
        // exactly the state the §5.7 mode bug proved users could not see.
        "status" => {
            let name = state.session_name.as_deref().unwrap_or("(unnamed)");
            let todos = state.todos.len();
            let plan = if state.plan { " · plan" } else { "" };
            let goal = match &state.goal {
                Some(_) => " · ◎ goal active",
                None => "",
            };
            notice(
                state,
                format!(
                    "{name} · model {} · mode {}{plan} · effort {} · context {} tok · \
                     {todos} todo(s){goal}",
                    state.model,
                    state.mode,
                    effort_report(state),
                    state.context_window
                ),
            );
            Vec::new()
        }
        // The breakdown `/status` and `/cost` cannot give: what is *in* the
        // window, by source. Round-trips through the engine — only the actor
        // can see the system prompt, the tool schemas and the real projection.
        //
        // No idle guard, unlike `/reload`: this appends nothing and replaces
        // nothing, so it is safe mid-turn.
        "context" => vec![Cmd::RequestContext],
        // Every run of the `workflow` tool this process has started, with
        // per-phase progress — read-only, safe mid-turn like `/context`.
        "workflows" => vec![Cmd::RequestWorkflows],
        // The strip shows a compact line; this prints the breakdown without
        // stealing strip width.
        "cost" => {
            let u = state.session_usage;
            let mut text = format!(
                "session: {} in · {} out · {} cached · {} written",
                tok(u.input),
                tok(u.output),
                tok(u.cache_read),
                tok(u.cache_written)
            );
            // Recent, not averaged: a session-wide ratio would blur a cold
            // cache mid-session into the warm turns before it (0032).
            let denom = u.last_input + u.last_cache_read;
            if let Some(pct) = (u.last_cache_read * 100).checked_div(denom) {
                text.push_str(&format!(" · cache {pct}% last turn"));
            }
            match u.cost_usd {
                Some(c) => text.push_str(&format!(" · ${c:.2}")),
                // R4 owns the price catalog; inventing a number here would be
                // worse than saying nothing.
                None => text.push_str(" · cost not reported by the provider"),
            }
            notice(state, text);
            Vec::new()
        }
        // The transcript is a projection, so clearing the *view* is safe and
        // client-side. The notice must say so: a user who thinks this
        // truncated the model's context is worse off than one who never ran it.
        "clear" => {
            state.transcript.clear();
            state.scroll = Scroll::Follow;
            // 0039/0043: neither a selection id nor a band row may dangle
            // into the next transcript.
            state.selected_agent = None;
            state.agent_scroll = None;
            state.band_cursor = None;
            notice(
                state,
                "cleared the transcript view — the session log and the model's context are untouched"
                    .into(),
            );
            Vec::new()
        }
        "quit" => vec![Cmd::Quit],
        other if state.skills.iter().any(|s| s == other) => {
            // Desugars to an ordinary prompt: the model calls the skill tool,
            // so the TUI never reads skill files itself. The ARGUMENTS come
            // from the expanded payload, not the raw buffer, so pastes arrive
            // as content and image markers keep pointing at real attachments.
            let expanded_arg = payload
                .text
                .trim()
                .strip_prefix('/')
                .and_then(|r| r.split_once(char::is_whitespace))
                .map(|(_, a)| a.trim())
                .unwrap_or("");
            let mut text = format!("Load the skill `{other}` and follow it for this task.");
            if !expanded_arg.is_empty() {
                text.push_str(&format!("\n\nARGUMENTS: {expanded_arg}"));
            }
            let payload = paste::PromptPayload {
                text: text.clone(),
                images: payload.images,
            };
            // Record the request to confirm it loaded — only a fresh turn's;
            // a queued skill is that turn's to verify, not this one's.
            if state.phase == Phase::Idle {
                state.pending_skill = Some(other.to_string());
            }
            submit(state, text, payload)
        }
        // A saved workflow recipe (0044), after the built-ins and skills —
        // the precedence `set_workflows` hides shadowed names by. Desugars
        // to a prompt naming the recipe; the tool call is the model's.
        other if state.workflows.iter().any(|w| w == other) => {
            let rest = payload
                .text
                .trim()
                .strip_prefix('/')
                .and_then(|r| r.split_once(char::is_whitespace))
                .map(|(_, a)| a.trim())
                .unwrap_or("");
            let mut text = format!(
                "Run the saved workflow `{other}` with the `workflow` tool (name = \"{other}\")."
            );
            if !rest.is_empty() {
                text.push_str(&format!(" Arguments: {rest}"));
            }
            let payload = paste::PromptPayload {
                text: text.clone(),
                images: payload.images,
            };
            submit(state, text, payload)
        }
        other => {
            notice(state, format!("unknown command: /{other}"));
            Vec::new()
        }
    }
}

/// Rows a PageUp/PageDown moves a modal body (0049 T6).
const MODAL_PAGE: usize = 5;

/// PageUp/PageDown scroll a tall ask/question/help body; the view clamps
/// the top. True when the key was consumed.
fn modal_page(state: &mut State, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::PageUp => state.modal_scroll = state.modal_scroll.saturating_sub(MODAL_PAGE),
        KeyCode::PageDown => state.modal_scroll = state.modal_scroll.saturating_add(MODAL_PAGE),
        _ => return false,
    }
    true
}

fn on_ask_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    if modal_page(state, key) {
        return Vec::new();
    }
    let Phase::WaitingAsk {
        req_id,
        summary,
        input,
        denying,
        ..
    } = &mut state.phase
    else {
        return Vec::new();
    };
    let req_id = *req_id;
    let offers_secret_reads = secret_read_grant_applies(summary);
    if *denying {
        match key.code {
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => {
                *denying = false;
                input.clear();
            }
            KeyCode::Enter => {
                let message = Some(input.clone()).filter(|m| !m.trim().is_empty());
                return resume_after_ask(state, req_id, false, false, message);
            }
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Char('y') => resume_after_ask(state, req_id, true, false, None),
        // Plan 0022: approve *and* lift the credential read-deny, for this one
        // command. Ignored unless the ask is one the grant can reach, so the
        // key never silently does nothing.
        KeyCode::Char('s') if offers_secret_reads => {
            resume_after_ask(state, req_id, true, true, None)
        }
        KeyCode::Char('n') => {
            *denying = true;
            Vec::new()
        }
        // The modal is the model waiting on you; wanting out of it is
        // wanting the turn gone. Same ladder as the plain-editor Esc.
        KeyCode::Esc => interrupt_or_detach(state),
        _ => Vec::new(),
    }
}

/// Does the per-command credential-read grant mean anything for this ask?
///
/// Only `bash` spawns a confined child, and only while the credential tier is
/// still denied — the sandbox label carries `reads:open` once an operator
/// lifted it via `[sandbox].readable`. Reading the label rather than the
/// process globals is deliberate: in attach mode the TUI is a *different
/// process* from the engine, where those globals are un-inited.
pub(crate) fn secret_read_grant_applies(summary: &str) -> bool {
    summary.starts_with("bash [") && !summary.contains("reads:open")
}

/// `ask_user`'s modal (tier-1 gap #4): number keys 1-N pick an option
/// instantly (submits right away — no confirm step, matching `on_ask_key`'s
/// `y`), as does `Enter` on the `↑`/`↓` cursor (0049 T6); any other
/// printable character starts free text instead (typing commits to free
/// text — once `input` is non-empty, digits are just more text, never a
/// late option pick). Esc while typing free text backs out to the picker
/// rather than submitting a partial answer.
fn on_question_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    if modal_page(state, key) {
        return Vec::new();
    }
    let Phase::WaitingQuestion {
        req_id,
        options,
        input,
        selected,
        ..
    } = &mut state.phase
    else {
        return Vec::new();
    };
    let req_id = *req_id;
    if !input.is_empty() {
        match key.code {
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => input.clear(),
            KeyCode::Enter => {
                let text = input.clone();
                return resume_after_question(state, req_id, Vec::new(), Some(text));
            }
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = c as usize - '1' as usize;
            if let Some(opt) = options.get(idx) {
                let label = opt.label.clone();
                return resume_after_question(state, req_id, vec![label], None);
            }
        }
        KeyCode::Up => *selected = selected.saturating_sub(1),
        KeyCode::Down => *selected = (*selected + 1).min(options.len().saturating_sub(1)),
        KeyCode::Enter => {
            if let Some(opt) = options.get(*selected) {
                let label = opt.label.clone();
                return resume_after_question(state, req_id, vec![label], None);
            }
        }
        KeyCode::Char(c) => input.push(c),
        // Same ladder as the ask picker: Esc with nothing typed is "I want
        // the turn gone", not a dead key.
        KeyCode::Esc => return interrupt_or_detach(state),
        _ => {}
    }
    Vec::new()
}

fn resume_after_question(
    state: &mut State,
    req_id: u64,
    selected: Vec<String>,
    free_text: Option<String>,
) -> Vec<Cmd> {
    state.phase = Phase::Sampling { ticks: 0 };
    vec![
        Cmd::ReplyQuestion {
            req_id,
            selected,
            free_text,
        },
        Cmd::SetTitle(title(state, " — working")),
    ]
}

/// The egress modal: two keys, both session-scoped. `y` allows the host for
/// the rest of the session; anything else denies, which is also the safe
/// default for a stray keypress. Esc takes the same interrupt-or-detach ladder
/// the other modals use.
fn on_egress_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    let Phase::WaitingEgress { req_id, host } = &state.phase else {
        return Vec::new();
    };
    let (req_id, host) = (*req_id, host.clone());
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => resume_after_egress(state, req_id, true, &host),
        KeyCode::Char('n') | KeyCode::Char('N') => resume_after_egress(state, req_id, false, &host),
        KeyCode::Esc => interrupt_or_detach(state),
        _ => Vec::new(),
    }
}

fn resume_after_egress(state: &mut State, req_id: u64, allow: bool, host: &str) -> Vec<Cmd> {
    if allow {
        // The grant is session-scoped, and hotl never writes config.toml for
        // you — so say what to paste (plan 0026 decision 9).
        notice(
            state,
            format!(
                "allowed \"{host}\" for this session — to make it permanent, \
                 add it to [network].allow in config.toml"
            ),
        );
    } else {
        notice(state, format!("denied \"{host}\" for this session"));
    }
    state.phase = Phase::Sampling { ticks: 0 };
    vec![
        Cmd::ReplyEgress { req_id, allow },
        Cmd::SetTitle(title(state, " — working")),
    ]
}

fn resume_after_ask(
    state: &mut State,
    req_id: u64,
    allow: bool,
    secret_reads: bool,
    message: Option<String>,
) -> Vec<Cmd> {
    state.phase = Phase::Sampling { ticks: 0 };
    vec![
        Cmd::ReplyPermission {
            req_id,
            allow,
            secret_reads,
            message,
        },
        Cmd::SetTitle(title(state, " — working")),
    ]
}

fn on_tick(state: &mut State) {
    // The goal's own clock: ticks arrive only while a turn runs, so this
    // measures the time the loop actually spends working.
    if state.goal.is_some() {
        state.goal_ticks += 1;
    }
    match &mut state.phase {
        Phase::Sampling { ticks } | Phase::Streaming { ticks, .. } => *ticks += 1,
        Phase::Tool { ticks, .. } => *ticks += 1,
        Phase::Idle
        | Phase::WaitingAsk { .. }
        | Phase::WaitingQuestion { .. }
        | Phase::WaitingEgress { .. } => {}
    }
    // The animation's whole-turn clock advances only while the turn is moving —
    // the same phases that animate. A blocked prompt pauses the cycle where it
    // stood; a fresh turn restarts it from 0 (reset on entry to `Idle`).
    if matches!(
        state.phase,
        Phase::Sampling { .. } | Phase::Streaming { .. } | Phase::Tool { .. }
    ) {
        state.work_ticks += 1;
        // Silence only counts while a turn is running: waiting on you is not
        // the engine being quiet.
        state.since_frame += 1;
        if let Some(retry) = &mut state.retry {
            retry.ticks += 1;
        }
        // EVERY running card ticks (0037), not just the newest: a sibling's
        // tool_done keeps the phase in `Tool` on the oldest card's clock
        // (0049 T1b); running cards tick regardless. Bounded to this turn:
        // running cards cannot predate the last prompt.
        for item in state.transcript.iter_mut().rev() {
            if matches!(item, TranscriptItem::User { .. }) {
                break;
            }
            if let TranscriptItem::Tool {
                ticks,
                status: ToolStatus::Running | ToolStatus::AutoAllowed { .. },
                ..
            } = item
            {
                *ticks += 1;
            }
        }
    }
}

fn append_assistant(state: &mut State, text: &str) {
    if let Some(TranscriptItem::Assistant { text: t }) = state.transcript.last_mut() {
        t.push_str(text);
    } else {
        state
            .transcript
            .push(TranscriptItem::Assistant { text: text.into() });
    }
}

/// The trailing open assistant bubble's length, or 0 when the last item is
/// something else (a tool card, a notice) — then the next delta starts a new
/// bubble and there is nothing to take back.
fn open_assistant_len(state: &State) -> usize {
    match state.transcript.last() {
        Some(TranscriptItem::Assistant { text }) => text.as_str().len(),
        _ => 0,
    }
}

/// Cut the open assistant bubble back to [`State::stream_mark`], dropping it
/// entirely when nothing was there before this sample.
fn truncate_open_assistant(state: &mut State) {
    let mark = state.stream_mark;
    let Some(TranscriptItem::Assistant { text }) = state.transcript.last_mut() else {
        return;
    };
    if text.as_str().len() <= mark {
        return;
    }
    if mark == 0 {
        state.transcript.pop();
    } else {
        text.truncate(mark);
    }
}

/// Cards still running this turn, transcript order, as `(name, count,
/// oldest ticks)` — bounded to the turn: cards cannot predate the last prompt.
pub fn running_cards(state: &State) -> Vec<(String, usize, u64)> {
    let mut out: Vec<(String, usize, u64)> = Vec::new();
    for item in state.transcript.iter().rev() {
        match item {
            TranscriptItem::User { .. } => break,
            TranscriptItem::Tool {
                name,
                ticks,
                status: ToolStatus::Running | ToolStatus::AutoAllowed { .. },
                ..
            } => match out.iter_mut().find(|(n, ..)| n == name) {
                Some((_, count, oldest)) => {
                    *count += 1;
                    *oldest = (*oldest).max(*ticks);
                }
                None => out.push((name.clone(), 1, *ticks)),
            },
            _ => {}
        }
    }
    out.reverse();
    out
}

/// The newest running card's reported line count (0061 T25), for the strip's
/// rank-2 slot. `None` when nothing running has spoken.
pub fn running_lines(state: &State) -> Option<u64> {
    state
        .transcript
        .iter()
        .rev()
        .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
        .find_map(|i| match i {
            TranscriptItem::Tool {
                status: ToolStatus::Running | ToolStatus::AutoAllowed { .. },
                progress: Some(pr),
                ..
            } => Some(pr.lines),
            _ => None,
        })
}

/// After a tool settles: another card still running keeps the turn in the
/// tool phase (on the oldest one's clock, so the strip's timer never resets
/// while work continues); none left → the model is sampling its next step.
/// Only `text_delta` enters `Streaming` (0049 T1b) — before this, a sibling
/// finishing read `writing · ~0 tok` over cards still running.
fn settle_phase(state: &mut State) {
    let running = running_cards(state);
    state.phase = match running.iter().max_by_key(|(_, _, t)| *t) {
        Some((name, _, ticks)) => Phase::Tool {
            name: name.clone(),
            ticks: *ticks,
        },
        None => Phase::Sampling { ticks: 0 },
    };
}

/// Streaming resumes with this turn's running char total (chars survive a
/// tool interlude by recount, not by stashing — the recount happens at the
/// first delta after the interlude).
fn enter_streaming(state: &mut State) {
    let ticks = match state.phase {
        Phase::Streaming { ticks, .. } => ticks,
        _ => 0,
    };
    state.phase = Phase::Streaming {
        ticks,
        chars: turn_chars(&state.transcript),
    };
}

/// Reasoning characters since the last prompt (0061 T21) — the counterpart of
/// [`turn_chars`], which counts only what was written.
pub(crate) fn thinking_chars(transcript: &[TranscriptItem]) -> u64 {
    transcript
        .iter()
        .rev()
        .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
        .map(|i| match i {
            TranscriptItem::Thinking { text } => text.len() as u64,
            _ => 0,
        })
        .sum()
}

fn turn_chars(transcript: &[TranscriptItem]) -> u64 {
    transcript
        .iter()
        .rev()
        .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
        .map(|i| match i {
            TranscriptItem::Assistant { text } => text.len() as u64,
            _ => 0,
        })
        .sum()
}

fn plan_step(v: &Value) -> PlanStep {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    PlanStep {
        content: text("content").unwrap_or_default(),
        proof: text("validate_cmd").or_else(|| text("acceptance")),
        after: v
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|d| {
                d.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The live plan card, if one is still awaiting an answer.
fn live_plan(state: &State) -> Option<&PresentedPlan> {
    state.transcript.iter().rev().find_map(|i| match i {
        TranscriptItem::Plan(p) if p.live => Some(p),
        _ => None,
    })
}

/// Retire the live plan card. Called by the two answers and by any ordinary
/// prompt — a human who typed something answered the card by doing so.
fn settle_plan(state: &mut State) {
    for item in state.transcript.iter_mut() {
        if let TranscriptItem::Plan(p) = item {
            p.live = false;
        }
    }
}

fn notice(state: &mut State, text: String) {
    state
        .transcript
        .push(TranscriptItem::Notice { text: text.into() });
}

/// Un-pin the newest queued steer chip (`SteerRejected`, `prompt_queued`).
/// Newest-only is wrong once two steers can queue at once (tracker #43) —
/// the real fix needs an id on `TranscriptItem::Steer` — but until then this
/// is the one place that decision lives.
fn clear_newest_queued_steer(state: &mut State) {
    if let Some(TranscriptItem::Steer { queued, .. }) = state
        .transcript
        .iter_mut()
        .rev()
        .find(|i| matches!(i, TranscriptItem::Steer { queued: true, .. }))
    {
        *queued = false;
    }
}

/// `/mode <name>`: optimistic local update (the badge flips immediately) plus
/// the durable `SetMode` the surface issues. Never starts a turn — a mode
/// switch is session bookkeeping, not a prompt.
fn set_mode(state: &mut State, mode: &str) -> Vec<Cmd> {
    state.mode = mode.to_string();
    notice(state, format!("permission mode set to {mode}"));
    vec![Cmd::SetMode(mode.to_string())]
}

/// What a bare `/effort` (and `/status`) reports (0030 Task 8): the explicit
/// setting, else the session's resolved default marked as such, else the
/// bare word for "the provider decides".
fn effort_report(state: &State) -> String {
    let base = match (&state.effort, &state.default_effort) {
        (Some(e), _) => e.clone(),
        (None, Some(d)) => format!("{d} (default)"),
        (None, None) => "default".into(),
    };
    // A pinned rung outranks the schedule, so naming the schedule beside it
    // would be a lie — it is only shown while it still governs.
    match (&state.effort_schedule, &state.effort) {
        (Some(sched), None) => format!("{base} (schedule: {sched})"),
        _ => base,
    }
}

/// `/effort <level>`: optimistic local update plus the durable `SetEffort`.
/// Never starts a turn — same session-bookkeeping shape as `/mode`.
fn set_effort(state: &mut State, effort: Option<&str>) -> Vec<Cmd> {
    state.effort = effort.map(str::to_string);
    if effort.is_none() {
        // Cleared ≠ the session default: the provider's own default governs
        // from here on, so the handshake-seeded default no longer describes
        // this session.
        state.default_effort = None;
    }
    notice(
        state,
        match effort {
            Some(e) => format!("effort set to {e}"),
            None => "effort cleared — the provider's own default applies \
                     (not the session default a fresh session starts with)"
                .into(),
        },
    );
    vec![Cmd::SetEffort(effort.map(str::to_string))]
}

/// `/plan`: same shape on the other axis.
fn set_plan(state: &mut State, plan: bool) -> Vec<Cmd> {
    state.plan = plan;
    notice(
        state,
        if plan {
            "plan mode on — file edits will ask, everything else follows the mode".into()
        } else {
            "plan mode off".to_string()
        },
    );
    vec![Cmd::SetPlan(plan)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn upd(s: &mut State, v: Value) -> Vec<Cmd> {
        update(s, Msg::Update(v))
    }

    fn press(s: &mut State, code: KeyCode) -> Vec<Cmd> {
        update(s, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl(s: &mut State, c: char) -> Vec<Cmd> {
        update(
            s,
            Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
        )
    }

    /// A non-`Char` key with modifiers (Ctrl-Home, Ctrl-End, …).
    fn press_mod(s: &mut State, code: KeyCode, mods: KeyModifiers) -> Vec<Cmd> {
        update(s, Msg::Key(KeyEvent::new(code, mods)))
    }

    /// 0033 Task 8b: printable keys land in the composer; Enter queues the
    /// draft (no Cmd, no transcript item, no phase change — there is no
    /// session yet); the open transition then fires exactly one SendPrompt.
    #[test]
    fn pre_open_typing_queues_and_the_open_transition_submits_once() {
        let mut s = State::new(false, "m".into());
        for c in "hi there".chars() {
            pre_open_input(
                &mut s,
                Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        assert_eq!(s.editor.text(), "hi there");
        assert!(!s.queued_submit, "typing alone must not queue");
        pre_open_input(
            &mut s,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(s.queued_submit, "Enter queues the draft");
        assert_eq!(s.editor.text(), "hi there", "the draft survives the queue");
        assert!(s.transcript.is_empty(), "nothing echoes before the session");
        assert_eq!(s.phase, Phase::Idle);

        let cmds = fire_queued_submit(&mut s);
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::SendPrompt(p) => Some(p.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, ["hi there"], "exactly one SendPrompt: {cmds:?}");
        assert!(!s.queued_submit, "the queue is one-shot");
        assert!(
            fire_queued_submit(&mut s).is_empty(),
            "a second transition fires nothing"
        );
    }

    /// An empty Enter pre-open queues nothing, and an unqueued open
    /// transition emits nothing.
    #[test]
    fn pre_open_empty_enter_never_queues() {
        let mut s = State::new(false, "m".into());
        pre_open_input(
            &mut s,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(!s.queued_submit);
        assert!(fire_queued_submit(&mut s).is_empty());
    }

    fn notices(n: usize) -> Vec<TranscriptItem> {
        (0..n)
            .map(|i| TranscriptItem::Notice {
                text: i.to_string().into(),
            })
            .collect()
    }

    fn type_str(s: &mut State, text: &str) {
        for c in text.chars() {
            press(s, KeyCode::Char(c));
        }
    }

    fn ask(s: &mut State) {
        update(
            s,
            Msg::PermissionRequest {
                req_id: 7,
                summary: "run bash: rm -rf ./x".into(),
                protected_why: None,
                diff: Vec::new(),
            },
        );
    }

    fn on_result(s: &mut State, kind: &str, text: Option<String>, usage: &Value) -> Vec<Cmd> {
        update(
            s,
            Msg::PromptResult {
                outcome_kind: kind.into(),
                outcome_text: text,
                usage: usage.clone(),
                finished_at: None,
            },
        )
    }

    /// A provider/transport failure must land as a loud `Error` item, never
    /// the muted `Notice` used for routine chatter — that is the whole point
    /// of surfacing it. Controlled stops stay notices.
    #[test]
    fn a_provider_error_is_a_loud_error_item_not_a_muted_notice() {
        let mut s = State::test_default();
        on_result(
            &mut s,
            "error",
            Some("HTTP 400: invalid_request_error: dangling tool_calls".into()),
            &json!({}),
        );
        assert!(
            matches!(
                last_spoken(&s),
                Some(TranscriptItem::Error { text }) if text.contains("HTTP 400")
            ),
            "an execution error must be an Error item: {:?}",
            last_spoken(&s)
        );

        // A controlled stop is still a muted notice, not an error.
        let mut s = State::test_default();
        on_result(&mut s, "turn_limit", None, &json!({}));
        assert!(matches!(
            last_spoken(&s),
            Some(TranscriptItem::Notice { .. })
        ));
    }

    fn skill_card(summary: &str, status: ToolStatus) -> TranscriptItem {
        TranscriptItem::Tool {
            id: "t1".into(),
            name: "skill".into(),
            summary: summary.into(),
            status,
            ticks: 0,
            calls: vec![ToolCall {
                id: "t1".into(),
                ok: None,
                lines: None,
                bytes: None,
            }],
            children: Vec::new(),
            child_text: String::new(),
            progress: None,
        }
    }

    // A recorded `/<skill>` dispatch: the turn's user item plus the request.
    fn requested_skill(s: &mut State, name: &str) {
        s.transcript.push(TranscriptItem::User {
            text: format!("Load the skill `{name}`").into(),
        });
        s.pending_skill = Some(name.into());
    }

    /// The last item a turn produced, past the closing summary every
    /// `PromptResult` now appends (0061 T9).
    fn last_spoken(s: &State) -> Option<&TranscriptItem> {
        s.transcript
            .iter()
            .rev()
            .find(|i| !matches!(i, TranscriptItem::TurnSummary { .. }))
    }

    fn warned_unloaded(s: &State) -> bool {
        matches!(last_spoken(s), Some(TranscriptItem::Notice { text }) if text.contains("not loaded"))
    }

    /// 0061 T9: every turn closes with one line saying what it cost. It goes
    /// last, after the outcome notice — a full stop, not a preamble.
    #[test]
    fn a_prompt_result_appends_a_turn_summary_after_the_outcome_notice() {
        let mut s = State::test_default();
        s.transcript
            .push(TranscriptItem::User { text: "go".into() });
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: echo"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"bash","ok":true,"lines":1,"bytes":2}),
        );
        s.work_ticks = 134 * crate::anim::TICK_HZ;
        on_result(&mut s, "turn_limit", None, &json!({}));
        assert!(
            matches!(
                s.transcript.last(),
                Some(TranscriptItem::TurnSummary {
                    secs: 134,
                    calls: 1,
                    finished_at: None
                })
            ),
            "{:?}",
            s.transcript.last()
        );
        assert!(
            matches!(last_spoken(&s), Some(TranscriptItem::Notice { .. })),
            "the outcome notice comes first: {:?}",
            s.transcript
        );
        // Merged cards count every call they absorbed.
        let mut s = State::test_default();
        s.transcript
            .push(TranscriptItem::User { text: "go".into() });
        for id in ["p1", "p2", "p3"] {
            upd(
                &mut s,
                json!({"type":"tool_start","id":id,"name":"read","summary":"read app.rs"}),
            );
            upd(
                &mut s,
                json!({"type":"tool_done","id":id,"name":"read","ok":true,"lines":1,"bytes":2}),
            );
        }
        on_result(&mut s, "done", None, &json!({}));
        assert!(
            matches!(
                s.transcript.last(),
                Some(TranscriptItem::TurnSummary { calls: 3, .. })
            ),
            "{:?}",
            s.transcript.last()
        );
    }

    #[test]
    fn a_requested_skill_that_never_loaded_warns_and_is_forgotten() {
        let mut s = State::test_default();
        requested_skill(&mut s, "brainstorming");
        on_result(&mut s, "done", None, &json!({}));
        assert!(warned_unloaded(&s), "{:?}", s.transcript.last());
        assert_eq!(s.pending_skill, None);
    }

    #[test]
    fn a_loaded_skill_is_not_warned_even_when_qualified() {
        // Bare /brainstorming, model loads the plugin form — leaf-name match.
        let mut s = State::test_default();
        requested_skill(&mut s, "brainstorming");
        s.transcript.push(skill_card(
            "skill superpowers:brainstorming",
            ToolStatus::Done,
        ));
        on_result(&mut s, "done", None, &json!({}));
        assert!(!warned_unloaded(&s), "{:?}", s.transcript.last());
    }

    #[test]
    fn a_failed_skill_load_is_not_double_reported() {
        // The ✗ card is the feedback; a warning on top would be noise.
        let mut s = State::test_default();
        requested_skill(&mut s, "brainstorming");
        s.transcript
            .push(skill_card("skill brainstorming", ToolStatus::Failed));
        on_result(&mut s, "done", None, &json!({}));
        assert!(!warned_unloaded(&s), "{:?}", s.transcript.last());
    }

    #[test]
    fn an_interrupted_skill_turn_is_forgotten_without_a_warning() {
        let mut s = State::test_default();
        requested_skill(&mut s, "brainstorming");
        on_result(&mut s, "cancelled", None, &json!({}));
        assert!(!warned_unloaded(&s));
        assert_eq!(s.pending_skill, None);
    }

    #[test]
    fn the_usage_line_shows_cache_reads_and_session_totals() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        let usage = |i, o, c| {
            json!({
                "input_tokens": i, "output_tokens": o,
                "cache_read_input_tokens": c, "cache_creation_input_tokens": 0
            })
        };
        on_result(&mut s, "done", None, &usage(1_000, 500, 4_000));
        on_result(&mut s, "done", None, &usage(2_000, 500, 8_000));

        let line = s.usage_line.clone().unwrap();
        assert!(line.contains("3.0k in"), "session totals: {line}");
        assert!(line.contains("1.0k out"), "{line}");
        assert!(
            line.contains("12.0k cached"),
            "cache reads must show: {line}"
        );
        // (2_000 + 8_000) / 200_000 of the window is live in the latest turn
        // — the strip's ctx chip reads it from here, never from the line.
        assert_eq!(s.live_context, Some(10_000));
        assert!(!line.contains("ctx"), "the gauge is a chip: {line}");
    }

    #[test]
    fn hit_ratio_percentage_shows_when_present() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        on_result(
            &mut s,
            "done",
            None,
            &json!({
                "input_tokens": 25, "output_tokens": 5,
                "cache_read_input_tokens": 50, "cache_creation_input_tokens": 25,
                "hit_ratio": 0.5
            }),
        );
        let line = s.usage_line.clone().unwrap();
        assert!(line.contains("50% hit"), "hit ratio must show: {line}");
    }

    #[test]
    fn hit_ratio_is_omitted_when_the_payload_carries_none() {
        let mut s = State::test_default();
        on_result(&mut s, "done", None, &json!({"input_tokens": 10}));
        assert!(
            !s.usage_line.as_ref().unwrap().contains("hit"),
            "no cache activity in the payload, no hit-ratio segment: {:?}",
            s.usage_line
        );
    }

    #[test]
    fn cost_shows_the_last_turns_cache_split() {
        let mut s = State::test_default();
        on_result(
            &mut s,
            "done",
            None,
            &json!({
                "input_tokens": 5, "output_tokens": 1,
                "cache_read_input_tokens": 90, "cache_creation_input_tokens": 5,
            }),
        );
        slash(&mut s, "cost");
        let text = last_notice(&s);
        assert!(text.contains("cache 90% last turn"), "{text}");
    }

    /// Cache writes accumulate like reads and surface on `/cost` always and
    /// on the strip only once nonzero (0046 D6).
    #[test]
    fn cache_writes_accumulate_and_show_once_nonzero() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        let usage = |i, c, w| {
            json!({
                "input_tokens": i, "output_tokens": 1,
                "cache_read_input_tokens": c, "cache_creation_input_tokens": w
            })
        };
        on_result(&mut s, "done", None, &usage(100, 50, 0));
        let line = s.usage_line.clone().unwrap();
        assert!(
            !line.contains("written"),
            "zero writes stay off the strip: {line}"
        );
        slash(&mut s, "cost");
        assert!(
            last_notice(&s).contains("· 0 written"),
            "{}",
            last_notice(&s)
        );

        on_result(&mut s, "done", None, &usage(100, 50, 700));
        on_result(&mut s, "done", None, &usage(100, 50, 500));
        assert_eq!(s.session_usage.cache_written, 1_200);
        let line = s.usage_line.clone().unwrap();
        assert!(line.contains("1.2k written"), "{line}");
        slash(&mut s, "cost");
        assert!(
            last_notice(&s).contains("1.2k written"),
            "{}",
            last_notice(&s)
        );
    }

    #[test]
    fn cost_omits_cache_health_with_no_denominator() {
        let mut s = State::test_default();
        slash(&mut s, "cost");
        let text = last_notice(&s);
        assert!(!text.contains("% last turn"), "{text}");
    }

    #[test]
    fn the_last_turn_split_overwrites_not_accumulates() {
        let mut s = State::test_default();
        // A cold first turn followed by a warm one: the pair must describe
        // the warm turn alone, or the health line blurs exactly the
        // regression it exists to catch.
        on_result(
            &mut s,
            "done",
            None,
            &json!({"input_tokens": 100, "cache_read_input_tokens": 0}),
        );
        on_result(
            &mut s,
            "done",
            None,
            &json!({"input_tokens": 10, "cache_read_input_tokens": 90}),
        );
        assert_eq!(s.session_usage.last_input, 10);
        assert_eq!(s.session_usage.last_cache_read, 90);
    }

    #[test]
    fn cost_is_shown_only_when_the_payload_carries_it() {
        let mut s = State::test_default();
        on_result(&mut s, "done", None, &json!({"input_tokens": 10}));
        assert!(
            !s.usage_line.as_ref().unwrap().contains('$'),
            "no invented prices"
        );

        on_result(
            &mut s,
            "done",
            None,
            &json!({"input_tokens": 10, "cost_usd": 0.0123}),
        );
        assert!(s.usage_line.as_ref().unwrap().contains("$0.01"));
    }

    #[test]
    fn the_context_gauge_is_omitted_without_a_window() {
        let mut s = State::test_default();
        s.context_window = 0;
        on_result(&mut s, "done", None, &json!({"input_tokens": 10}));
        assert_eq!(ctx_pct(s.live_context.unwrap(), s.context_window), None);
    }

    #[test]
    fn ctx_pct_caps_at_one_hundred_and_refuses_a_zero_window() {
        assert_eq!(ctx_pct(24_000, 200_000), Some(12));
        assert_eq!(ctx_pct(0, 200_000), Some(0));
        // An over-full window (compaction pending) still reads as full,
        // never as an absurd 150%.
        assert_eq!(ctx_pct(300_000, 200_000), Some(100));
        assert_eq!(ctx_pct(10, 0), None);
    }

    #[test]
    fn thinking_deltas_accumulate_into_one_item() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type": "thinking_delta", "text": "first "}));
        upd(&mut s, json!({"type": "thinking_delta", "text": "second"}));
        assert_eq!(
            s.transcript,
            vec![TranscriptItem::Thinking {
                text: "first second".into()
            }]
        );
        // Text after thinking starts a separate assistant item.
        upd(&mut s, json!({"type": "text_delta", "text": "answer"}));
        assert_eq!(s.transcript.len(), 2);
    }

    #[test]
    fn empty_thinking_deltas_create_nothing() {
        // Until R3 sends `thinking.display: "summarized"` the deltas are empty
        // — that must render as nothing, not as an empty dimmed block.
        let mut s = State::test_default();
        upd(&mut s, json!({"type": "thinking_delta", "text": ""}));
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn ctrl_t_toggles_thinking_expansion() {
        let mut s = State::test_default();
        assert!(!s.thinking_expanded);
        ctrl(&mut s, 't');
        assert!(s.thinking_expanded);
        ctrl(&mut s, 't');
        assert!(!s.thinking_expanded);
    }

    /// 0061 T25: liveness lands on the card and nowhere else — it is not a
    /// `text_delta`, and the strip already follows the cards.
    #[test]
    fn tool_progress_lands_on_its_card_without_touching_the_phase() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: cargo build"}),
        );
        let phase = s.phase.clone();
        upd(
            &mut s,
            json!({"type":"tool_progress","id":"p1","name":"bash","tail":"Compiling hotl","lines":12,"bytes":480}),
        );
        assert_eq!(s.phase, phase, "progress moved the phase");
        let Some(TranscriptItem::Tool { progress, .. }) = s.transcript.last() else {
            panic!("the card is the last item")
        };
        let pr = progress.clone().expect("the tail landed");
        assert_eq!(pr.tail, "Compiling hotl");
        assert_eq!((pr.lines, pr.bytes), (12, 480));
    }

    /// An agent card's second row is its own account of itself; a child's
    /// bash tail has no place on it (0061 decision 3).
    #[test]
    fn tool_progress_on_an_agent_card_is_ignored() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"s1","name":"spawn","summary":"spawn survey"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_progress","id":"s1","name":"spawn","tail":"anything","lines":1,"bytes":1}),
        );
        let Some(TranscriptItem::Tool { progress, .. }) = s.transcript.last() else {
            panic!("the card is the last item")
        };
        assert!(progress.is_none());
    }

    /// The tail row gives way to the result row when the tool settles.
    #[test]
    fn tool_done_clears_progress() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: cargo build"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_progress","id":"p1","name":"bash","tail":"Compiling","lines":1,"bytes":9}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"bash","ok":true,"lines":1,"bytes":9}),
        );
        let Some(TranscriptItem::Tool { progress, .. }) = s.transcript.last() else {
            panic!("the card is the last item")
        };
        assert!(progress.is_none());
    }

    /// A tool that is printing is not a session that has gone quiet.
    #[test]
    fn a_tool_progress_frame_counts_as_activity() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: cargo build"}),
        );
        s.since_frame = 30 * crate::anim::TICK_HZ;
        upd(
            &mut s,
            json!({"type":"tool_progress","id":"p1","name":"bash","tail":"Compiling","lines":1,"bytes":9}),
        );
        assert_eq!(s.since_frame, 0);
    }

    /// 0061 T24: a call waiting on the subprocess budget gets a parked card
    /// that its own `tool_start` promotes — the same card, not a second one.
    #[test]
    fn a_tool_queued_frame_parks_a_queued_card_that_its_tool_start_promotes() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_queued","id":"p1","name":"bash","summary":"bash: cargo test","ahead":2}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![("p1".into(), ToolStatus::Queued { ahead: 2 })]
        );
        assert!(
            matches!(s.phase, Phase::Sampling { .. }),
            "a queued card is not a running tool: {:?}",
            s.phase
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: cargo test"}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![("p1".into(), ToolStatus::Running)],
            "one card, promoted"
        );
        assert!(matches!(s.phase, Phase::Tool { .. }));
    }

    /// No clock and no place on the strip: nothing has started.
    #[test]
    fn a_queued_card_never_ticks_or_enters_the_strip() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_queued","id":"p1","name":"bash","summary":"bash: ls","ahead":0}),
        );
        update(&mut s, Msg::Tick);
        update(&mut s, Msg::Tick);
        let Some(TranscriptItem::Tool { ticks, .. }) = s.transcript.last() else {
            panic!("the queued card is the last item")
        };
        assert_eq!(*ticks, 0, "a parked card has no clock");
        assert!(running_cards(&s).is_empty(), "it is not running");
    }

    /// Promoting at the tail would reorder the transcript around whatever
    /// landed while the call waited.
    #[test]
    fn a_queued_card_promotes_in_place_not_at_the_tail() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_queued","id":"p1","name":"bash","summary":"bash: cargo test","ahead":0}),
        );
        upd(&mut s, json!({"type":"text_delta","text":"meanwhile"}));
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: cargo test"}),
        );
        assert_eq!(tool_statuses(&s).len(), 1, "no second card");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Assistant { .. })),
            "the prose that landed while it waited stays last: {:?}",
            s.transcript.last()
        );
    }

    /// 0061 T23 (tracker #35): the fold blocks the actor for a hook and a
    /// model call, and until now emitted nothing until it was over.
    #[test]
    fn a_compacting_frame_sets_the_fold_flag_and_compacted_clears_it() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"compacting","items":42}));
        assert!(s.compacting);
        upd(&mut s, json!({"type":"compacted","degraded":false}));
        assert!(!s.compacting);
    }

    #[test]
    fn cleared_and_the_prompt_result_also_clear_the_fold_flag() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"compacting","items":4}));
        upd(&mut s, json!({"type":"cleared","count":2}));
        assert!(!s.compacting);

        let mut s = State::test_default();
        upd(&mut s, json!({"type":"compacting","items":4}));
        on_result(&mut s, "done", None, &json!({}));
        assert!(!s.compacting);
    }

    /// 0061 T22: a backoff parks a countdown, and the first frame that proves
    /// the re-send landed clears it.
    #[test]
    fn a_retrying_frame_parks_a_countdown_that_the_next_delta_clears() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"retrying","attempt":2,"max":5,"reason":"HTTP 429: slow","delay_ms":4000,"status":429}),
        );
        let retry = s.retry.clone().expect("the backoff parked");
        assert_eq!(retry.attempt, 2);
        assert_eq!(retry.max, Some(5));
        assert_eq!(retry.status, "HTTP 429");
        assert_eq!(retry.delay_ticks, Some(4 * crate::anim::TICK_HZ));
        upd(&mut s, json!({"type":"text_delta","text":"back"}));
        assert!(s.retry.is_none(), "the delta proved the re-send landed");

        // A thinking delta counts too.
        upd(
            &mut s,
            json!({"type":"retrying","attempt":1,"reason":"stream idle for 300s"}),
        );
        let retry = s.retry.clone().expect("the backoff parked");
        assert_eq!(retry.max, None);
        assert_eq!(
            retry.status, "stream idle for 300s",
            "no status, the reason"
        );
        assert_eq!(retry.delay_ticks, None, "an older peer sends no delay");
        upd(&mut s, json!({"type":"thinking_delta","text":"mm"}));
        assert!(s.retry.is_none());
    }

    #[test]
    fn a_tool_start_clears_the_retry() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"retrying","attempt":1,"reason":"boom"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: ls"}),
        );
        assert!(s.retry.is_none());
    }

    /// The countdown rides the same clock everything else does: it advances
    /// only while a turn runs.
    #[test]
    fn retry_ticks_advance_only_while_running() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"retrying","attempt":1,"reason":"boom"}),
        );
        update(&mut s, Msg::Tick);
        assert_eq!(s.retry.as_ref().unwrap().ticks, 1);
        s.phase = Phase::Idle;
        update(&mut s, Msg::Tick);
        assert_eq!(s.retry.as_ref().unwrap().ticks, 1, "idle ticks nothing");
    }

    /// 0061 T21: thinking is not writing. `enter_streaming` on a thinking
    /// delta put the strip on `writing · ~0 tok` for the whole reasoning pass,
    /// because `Streaming.chars` counts Assistant text only.
    #[test]
    fn thinking_deltas_keep_the_sampling_phase() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 5 };
        upd(&mut s, json!({"type":"thinking_delta","text":"mulling"}));
        assert_eq!(
            s.phase,
            Phase::Sampling { ticks: 5 },
            "the clock kept running"
        );
    }

    /// A model that thinks again after writing goes back to thinking.
    #[test]
    fn thinking_after_writing_returns_to_sampling() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(&mut s, json!({"type":"text_delta","text":"half an answer"}));
        assert!(matches!(s.phase, Phase::Streaming { .. }));
        upd(&mut s, json!({"type":"thinking_delta","text":"wait"}));
        assert!(matches!(s.phase, Phase::Sampling { .. }), "{:?}", s.phase);
        // A tool phase with a running card is not disturbed (0049 T1b).
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"bash: ls"}),
        );
        upd(&mut s, json!({"type":"thinking_delta","text":"wait"}));
        assert!(matches!(s.phase, Phase::Tool { .. }), "{:?}", s.phase);
    }

    /// 0061 T20: the strip and the hint both say it now, so a transcript item
    /// saying it a third time is noise the answer has to scroll past.
    #[test]
    fn an_interrupt_pushes_no_notice() {
        for key in [KeyCode::Esc, KeyCode::Char('c')] {
            let mut s = State::test_default();
            s.phase = Phase::Streaming { ticks: 0, chars: 0 };
            let before = s.transcript.len();
            if key == KeyCode::Esc {
                press(&mut s, KeyCode::Esc); // Insert → Normal (0042 D2)
                press(&mut s, KeyCode::Esc);
            } else {
                ctrl(&mut s, 'c');
            }
            assert!(s.interrupt_sent, "{key:?}");
            assert_eq!(s.transcript.len(), before, "{key:?} pushed a notice");
        }
        // The detach notice stays: control changing hands is a fact about the
        // session, not a repeat of the strip.
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc);
        press(&mut s, KeyCode::Esc);
        press(&mut s, KeyCode::Esc);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { .. })),
            "{:?}",
            s.transcript.last()
        );
    }

    /// 0061 T19: anything off the wire is proof the engine is alive, whatever
    /// it said — so every wire-borne message restarts the gap.
    #[test]
    fn every_wire_message_resets_the_quiet_gap() {
        let msgs = || {
            vec![
                Msg::Update(json!({"type":"text_delta","text":"hi"})),
                Msg::PermissionRequest {
                    req_id: 1,
                    summary: "bash: ls".into(),
                    protected_why: None,
                    diff: Vec::new(),
                },
                Msg::QuestionRequest {
                    req_id: 2,
                    question: Question {
                        header: "which?".into(),
                        prompt: "pick".into(),
                        options: Vec::new(),
                        multi: false,
                    },
                },
                Msg::EgressRequest {
                    req_id: 3,
                    host: "example.com".into(),
                },
                Msg::PromptResult {
                    outcome_kind: "done".into(),
                    outcome_text: None,
                    usage: json!({}),
                    finished_at: None,
                },
                Msg::SteerRejected { why: "no".into() },
            ]
        };
        for msg in msgs() {
            let mut s = State::test_default();
            s.since_frame = 5 * crate::anim::TICK_HZ;
            update(&mut s, msg);
            assert_eq!(s.since_frame, 0, "a wire frame left the gap running");
        }
        // A key press is not the engine speaking.
        let mut s = State::test_default();
        s.since_frame = 5 * crate::anim::TICK_HZ;
        ctrl(&mut s, 'o');
        assert_eq!(s.since_frame, 5 * crate::anim::TICK_HZ);
    }

    /// Waiting on you is not the engine being quiet, and neither is idle.
    #[test]
    fn the_quiet_gap_grows_only_while_a_turn_runs() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        update(&mut s, Msg::Tick);
        assert_eq!(s.since_frame, 1);
        s.phase = Phase::Idle;
        update(&mut s, Msg::Tick);
        assert_eq!(s.since_frame, 1, "idle ticks nothing");
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "bash: ls".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        update(&mut s, Msg::Tick);
        assert_eq!(s.since_frame, 1, "an ask ticks nothing");
    }

    /// A new turn's silence starts at the send, not at the last frame of the
    /// turn before it.
    #[test]
    fn submit_restarts_the_quiet_gap() {
        let mut s = State::test_default();
        s.since_frame = 30 * crate::anim::TICK_HZ;
        for c in "go".chars() {
            update(
                &mut s,
                Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        update(
            &mut s,
            Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert_eq!(s.since_frame, 0);
    }

    /// 0061 T7: the twin of Ctrl-T for work.
    #[test]
    fn ctrl_o_toggles_tool_expansion() {
        let mut s = State::test_default();
        assert!(!s.tools_expanded);
        ctrl(&mut s, 'o');
        assert!(s.tools_expanded);
        ctrl(&mut s, 'o');
        assert!(!s.tools_expanded);
    }

    /// `Editor::handle` swallows every Ctrl chord it does not itself bind, so
    /// the toggle has to be caught above it — including mid-draft.
    #[test]
    fn ctrl_o_is_intercepted_above_the_editor_in_insert_mode() {
        let mut s = State::test_default();
        for c in "cargo".chars() {
            update(
                &mut s,
                Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        ctrl(&mut s, 'o');
        assert!(s.tools_expanded, "the editor ate the chord");
        assert_eq!(s.editor.text(), "cargo", "the draft survived");
    }

    #[test]
    fn a_modal_transition_clears_the_editor_search() {
        let mut s = State::test_default();
        s.editor.load_history(vec!["cargo test".into()]);
        s.editor
            .handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(s.editor.search_prompt().is_some());
        update(
            &mut s,
            Msg::PermissionRequest {
                req_id: 1,
                summary: "write ./x".into(),
                protected_why: None,
                diff: Vec::new(),
            },
        );
        assert!(
            s.editor.search_prompt().is_none(),
            "search survived the ask"
        );
    }

    #[test]
    fn mode_changed_updates_the_badge_state() {
        let mut s = State::test_default();
        assert_eq!(s.mode, "ask");
        upd(&mut s, json!({"type": "mode_changed", "mode": "auto"}));
        assert_eq!(s.mode, "auto");
    }

    #[test]
    fn a_multiline_paste_fires_no_turns() {
        let mut s = State::new(false, "m".into());
        let cmds = update(&mut s, Msg::Paste("a\nb\nc\nd".into()));
        assert!(cmds.is_empty(), "paste must not emit SendPrompt: {cmds:?}");
        // 3+ lines compact to a token; the content parks in the side table.
        assert_eq!(s.editor.text(), "[Pasted text #1 +4 lines]");
        assert_eq!(
            s.attachments,
            vec![paste::Attachment::Paste {
                text: "a\nb\nc\nd".into(),
                lines: 4
            }]
        );
        assert_eq!(s.phase, Phase::Idle);
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn a_two_line_paste_stays_literal() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("a\nb".into()));
        assert_eq!(s.editor.text(), "a\nb");
        assert!(s.attachments.is_empty());
    }

    #[test]
    fn an_image_path_paste_compacts_to_a_token() {
        let mut s = State::new(false, "m".into());
        // The dropped form: escaped space, trailing space — one smoke case;
        // the full form table lives in paste.rs.
        update(&mut s, Msg::Paste("/tmp/My\\ Shot.png ".into()));
        assert_eq!(s.editor.text(), "[Image #1]");
        assert_eq!(
            s.attachments,
            vec![paste::Attachment::Image {
                path: "/tmp/My Shot.png".into(),
                media_type: "image/png".into()
            }]
        );
    }

    #[test]
    fn submit_expands_pastes_ships_image_paths_and_clears_the_table() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        type_str(&mut s, " and ");
        update(&mut s, Msg::Paste("x\ny\nz".into()));
        let cmds = press(&mut s, KeyCode::Enter);
        // Transcript shows what the human typed, tokens and all.
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::User { text })
                if text == "[Image #1] and [Pasted text #1 +3 lines]"
        ));
        // History gets the fully-expanded bytes; the wire gets paste content
        // plus the image path with data unfilled (the runtime seam's job).
        let [Cmd::AppendHistory(h), Cmd::SendPrompt(p), Cmd::SetTitle(_)] = &cmds[..] else {
            panic!("unexpected cmds: {cmds:?}");
        };
        assert_eq!(h, "/tmp/shot.png and x\ny\nz");
        assert_eq!(p.text, "[Image #1] and x\ny\nz");
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].path, "/tmp/shot.png");
        assert_eq!(p.images[0].data, None);
        assert!(s.attachments.is_empty(), "the table dies with the draft");
    }

    #[test]
    fn recalling_an_image_prompt_replays_the_path_not_a_dead_token() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        type_str(&mut s, " what is this?");
        press(&mut s, KeyCode::Enter);
        // Up recalls the EXPANDED prompt, so a re-submit is self-contained.
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "/tmp/shot.png what is this?");
    }

    #[test]
    fn markers_number_per_kind_and_reset_after_submit() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/1.png".into()));
        update(&mut s, Msg::Paste("l1\nl2\nl3".into()));
        update(&mut s, Msg::Paste("/a/2.png".into()));
        assert_eq!(
            s.editor.text(),
            "[Image #1][Pasted text #1 +3 lines][Image #2]"
        );
        press(&mut s, KeyCode::Enter);
        // A fresh draft numbers from #1 again.
        update(&mut s, Msg::Paste("/a/3.png".into()));
        assert_eq!(s.editor.text(), "[Image #1]");
        assert_eq!(s.attachments.len(), 1);
    }

    #[test]
    fn a_mangled_token_submits_literally_and_drops_the_orphan() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/b.png".into()));
        // The human deletes a char INSIDE the token (backspace at its end
        // would swallow it whole — that path has its own vim.rs test).
        s.editor.cursor_to((0, 9));
        press(&mut s, KeyCode::Backspace);
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find(|c| matches!(c, Cmd::SendPrompt(_))) else {
            panic!("expected a prompt: {cmds:?}");
        };
        assert_eq!(p.text, "[Image #]");
        assert!(p.images.is_empty(), "the orphan must not ship");
    }

    #[test]
    fn backspace_swallows_a_token_only_while_its_attachment_lives() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        assert_eq!(s.editor.text(), "[Image #1]");
        press(&mut s, KeyCode::Backspace);
        assert_eq!(s.editor.text(), "", "a live token deletes whole");

        // Same grammar, no side-table entry: one char, like any other prose.
        type_str(&mut s, "why does it render [Image #2]");
        press(&mut s, KeyCode::Backspace);
        assert_eq!(s.editor.text(), "why does it render [Image #2");
    }

    #[test]
    fn normal_submit_syncs_live_tokens_so_a_stale_look_alike_stays_prose() {
        // Without the sync-after-clear in the non-empty Submit arm, the
        // live set would still hold "[Image #1]" from the paste and the
        // whole re-typed token below would get swallowed instead of one char.
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        press(&mut s, KeyCode::Enter);
        assert!(s.attachments.is_empty());
        type_str(&mut s, "why does it render [Image #1]");
        press(&mut s, KeyCode::Backspace);
        assert_eq!(s.editor.text(), "why does it render [Image #1");
    }

    #[test]
    fn empty_submit_syncs_live_tokens_so_a_stale_look_alike_stays_prose() {
        // Same discriminator, via the empty-buffer Submit arm: swallow the
        // token (buffer empties, `attachments` still holds it), submit the
        // empty buffer, then confirm a re-typed look-alike is just prose.
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        press(&mut s, KeyCode::Backspace);
        assert_eq!(s.editor.text(), "");
        assert_eq!(s.attachments.len(), 1);
        press(&mut s, KeyCode::Enter);
        assert!(s.attachments.is_empty());
        type_str(&mut s, "why does it render [Image #1]");
        press(&mut s, KeyCode::Backspace);
        assert_eq!(s.editor.text(), "why does it render [Image #1");
    }

    #[test]
    fn a_steer_carries_images_identically() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        update(&mut s, Msg::Paste("/a/b.png".into()));
        let cmds = press(&mut s, KeyCode::Enter);
        let [Cmd::SendSteer(p)] = &cmds[..] else {
            panic!("expected a steer: {cmds:?}");
        };
        assert_eq!(p.text, "[Image #1]");
        assert_eq!(p.images[0].path, "/a/b.png");
    }

    #[test]
    fn an_empty_submit_clears_stale_attachments() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/b.png".into()));
        // Delete the whole token, then submit the empty buffer.
        for _ in 0.."[Image #1]".len() {
            press(&mut s, KeyCode::Backspace);
        }
        press(&mut s, KeyCode::Enter);
        assert!(s.attachments.is_empty());
    }

    #[test]
    fn editor_roundtrip_keeps_attachments() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/b.png".into()));
        // `$EDITOR` replaces the whole buffer via set_text; the token
        // survives textually and the side table lives in State, so the
        // attachment still ships at submit.
        let cmds = update(
            &mut s,
            Msg::EditorDone(Ok(Some("look: [Image #1] please".into()))),
        );
        assert!(cmds.is_empty(), "{cmds:?}");
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find(|c| matches!(c, Cmd::SendPrompt(_))) else {
            panic!("expected a prompt: {cmds:?}");
        };
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].path, "/a/b.png");
    }

    #[test]
    fn page_keys_scroll_the_transcript_without_vim_mode() {
        // The regression: scroll used to be reachable only from vim Normal
        // mode, which `[behavior] vim_mode = false` (the default) makes
        // unreachable.
        let mut s = State::new(false, "m".into());
        s.transcript = notices(30);
        press(&mut s, KeyCode::PageUp);
        assert_eq!(s.scroll, Scroll::At(20));
        press_mod(&mut s, KeyCode::Home, KeyModifiers::CONTROL);
        assert_eq!(s.scroll, Scroll::At(0));
        press_mod(&mut s, KeyCode::End, KeyModifiers::CONTROL);
        assert_eq!(s.scroll, Scroll::Follow);
        press(&mut s, KeyCode::PageDown);
        assert_eq!(s.scroll, Scroll::Follow);
        assert!(s.editor.text().is_empty(), "scroll keys must not type");
    }

    #[test]
    fn bare_home_and_end_are_line_motions_not_scroll() {
        let mut s = State::new(false, "m".into());
        s.transcript = notices(30);
        s.editor.set_text("hello world");
        press(&mut s, KeyCode::Home);
        assert_eq!(s.editor.cursor(), (0, 0));
        assert_eq!(s.scroll, Scroll::Follow, "bare Home must not scroll");
        press(&mut s, KeyCode::End);
        assert_eq!(s.editor.cursor(), (0, 11));
    }

    #[test]
    fn prompt_echoes_immediately_and_enters_sampling() {
        let mut s = State::test_default();
        type_str(&mut s, "hello");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::User { text }) if text == "hello")
        );
        assert!(matches!(s.phase, Phase::Sampling { .. }));
        assert!(matches!(
            cmds[..],
            [Cmd::AppendHistory(_), Cmd::SendPrompt(_), Cmd::SetTitle(_)]
        ));
    }

    /// 0056 T3: the plan arrives as a card, `a` approves it (leaving plan
    /// mode and sending the implement prompt), and the card stops offering
    /// its keys once answered.
    #[test]
    fn a_presented_plan_becomes_a_card_that_a_approves() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type":"plan_presented","summary":"swap the parser","path":"/p/current.md",
                   "nodes":[
                       {"content":"write the lexer","status":"pending",
                        "validate_cmd":"cargo test -p lex","id":"n1"},
                       {"content":"wire it up","status":"pending","dependencies":["n1"]}]}),
        );
        let Some(TranscriptItem::Plan(plan)) = s.transcript.last() else {
            panic!("no plan card: {:?}", s.transcript);
        };
        assert!(plan.live);
        assert_eq!(plan.summary, "swap the parser");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].proof.as_deref(), Some("cargo test -p lex"));
        assert_eq!(plan.steps[1].after, vec!["n1".to_string()]);
        assert_eq!(plan.path.as_deref(), Some("/p/current.md"));

        let cmds = press(&mut s, KeyCode::Char('a'));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SetPlan(false))),
            "approve must leave plan mode: {cmds:?}"
        );
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find(|c| matches!(c, Cmd::SendPrompt(_))) else {
            panic!("approve must send the implement prompt: {cmds:?}");
        };
        assert_eq!(p.text, PLAN_APPROVED_PROMPT);
        assert!(live_plan(&s).is_none(), "an answered card keeps no keys");
        // …and the key is an ordinary character again.
        press(&mut s, KeyCode::Char('a'));
        assert_eq!(s.editor.text(), "a");
    }

    /// `r` dismisses the card and hands the keyboard back — the revision is
    /// just the next thing you type.
    #[test]
    fn r_dismisses_the_plan_card_without_sending_anything() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type":"plan_presented","summary":"x","nodes":[
                {"content":"a","status":"pending"}]}),
        );
        let cmds = press(&mut s, KeyCode::Char('r'));
        assert!(cmds.is_empty(), "revise sends nothing: {cmds:?}");
        assert!(live_plan(&s).is_none());
    }

    /// A draft in progress owns its own letters: `a` mid-sentence types.
    #[test]
    fn the_plan_keys_never_steal_from_a_draft() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type":"plan_presented","summary":"x","nodes":[
                {"content":"a","status":"pending"}]}),
        );
        press(&mut s, KeyCode::Char('w'));
        let cmds = press(&mut s, KeyCode::Char('a'));
        assert!(cmds.is_empty());
        assert_eq!(s.editor.text(), "wa");
        assert!(live_plan(&s).is_some(), "the card is still waiting");
    }

    #[test]
    fn todos_changed_populates_state() {
        let mut s = State::test_default();
        assert!(s.todos.is_empty());
        upd(
            &mut s,
            json!({"type":"todos_changed","items":[
                {"content":"wire the gate","status":"in_progress"},
                {"content":"write docs","status":"pending"}
            ]}),
        );
        assert_eq!(s.todos.len(), 2);
        assert_eq!(s.todos[0].content, "wire the gate");
        assert_eq!(s.todos[0].status, hotl_tools::todo::TodoStatus::InProgress);

        // A later `todos_changed` fully replaces the list (including down
        // to empty — the model clearing it is a real, renderable state).
        upd(&mut s, json!({"type":"todos_changed","items":[]}));
        assert!(s.todos.is_empty());
    }

    #[test]
    fn text_delta_moves_sampling_to_streaming_and_counts_chars() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 8 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        assert!(matches!(s.phase, Phase::Streaming { chars: 6, .. }));
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Assistant { text }) if text == "hi you")
        );
    }

    #[test]
    fn tool_start_and_done_drive_tool_phase_and_card() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        upd(
            &mut s,
            json!({"type":"tool_start","id":"t1","name":"bash","summary":"echo hi"}),
        );
        assert!(matches!(&s.phase, Phase::Tool { name, .. } if name == "bash"));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Tool {
                status: ToolStatus::Running,
                ..
            })
        ));
        upd(
            &mut s,
            json!({"type":"tool_done","id":"t1","name":"bash","ok":true}),
        );
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Tool {
                status: ToolStatus::Done,
                ..
            })
        ));
        assert!(
            matches!(s.phase, Phase::Sampling { .. }),
            "nothing runs, so the model is thinking: {:?}",
            s.phase
        );
    }

    /// 0049 T1b: `tool_done` settles into the tool phase while a sibling
    /// still runs (the strip lists every running card on the oldest clock),
    /// into sampling once none does, and only a delta writes.
    #[test]
    fn a_sibling_settling_keeps_the_tool_phase_while_others_run() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"s1","name":"spawn","summary":"explore a"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"s2","name":"spawn","summary":"explore b"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"g1","name":"grep","summary":"grep foo"}),
        );
        update(&mut s, Msg::Tick);
        upd(
            &mut s,
            json!({"type":"tool_done","id":"g1","name":"grep","ok":true}),
        );
        assert!(
            matches!(&s.phase, Phase::Tool { name, .. } if name == "spawn"),
            "{:?}",
            s.phase
        );
        assert!(
            crate::anim::strip_text(&s).starts_with("spawn ×2 · "),
            "{}",
            crate::anim::strip_text(&s)
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"s1","name":"spawn","ok":true}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"s2","name":"spawn","ok":true}),
        );
        assert!(
            matches!(s.phase, Phase::Sampling { ticks: 0 }),
            "nothing runs → thinking: {:?}",
            s.phase
        );
        assert!(crate::anim::strip_text(&s).starts_with("thinking · 0s"));
        upd(&mut s, json!({"type":"text_delta","text":"done"}));
        assert!(
            matches!(s.phase, Phase::Streaming { chars: 4, .. }),
            "only a delta writes: {:?}",
            s.phase
        );
    }

    #[test]
    fn chars_still_survive_a_tool_interlude_by_recount() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        upd(
            &mut s,
            json!({"type":"tool_start","id":"t1","name":"bash","summary":"echo hi"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"t1","name":"bash","ok":true}),
        );
        assert!(
            matches!(s.phase, Phase::Sampling { .. }),
            "settled, sampling: {:?}",
            s.phase
        );
        upd(&mut s, json!({"type":"text_delta","text":"!"}));
        assert!(
            matches!(s.phase, Phase::Streaming { chars: 7, .. }),
            "{:?}",
            s.phase
        );
    }

    /// The card statuses of every `TranscriptItem::Tool`, in transcript order.
    fn tool_statuses(s: &State) -> Vec<(String, ToolStatus)> {
        s.transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Tool { id, status, .. } => Some((id.clone(), status.clone())),
                _ => None,
            })
            .collect()
    }

    /// 0037's cards-per-page rendering, narrowed by 0039 D3: calls with
    /// DISTINCT merge keys (different paths) still keep their own cards and
    /// settle each under their own id, dones arriving in any order.
    #[test]
    fn distinct_key_calls_keep_their_own_cards() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        for id in ["p1", "p2", "p3"] {
            upd(
                &mut s,
                json!({"type":"tool_start","id":id,"name":"read","summary":format!("read {id}.rs · 2 lines")}),
            );
        }
        // Out of order, mixed outcomes: p2 fails, then p1, then p3 succeed.
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p2","name":"read","ok":false}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"read","ok":true}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p3","name":"read","ok":true}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![
                ("p1".into(), ToolStatus::Done),
                ("p2".into(), ToolStatus::Failed),
                ("p3".into(), ToolStatus::Done),
            ],
            "every card settles under its own id; none is left Running"
        );
    }

    /// 0039 D3 — DELIBERATELY supersedes 0037 D4's cards-per-page rendering
    /// (the owner chose merging): same-key concurrent calls absorb into ONE
    /// accumulating card, every id settles out of order via `calls` (D5), and
    /// a mixed batch settles Failed (D4). The `tool_done` no-card fallback
    /// must never fire for an absorbed id — no orphans, no duplicates.
    #[test]
    fn same_key_concurrent_calls_merge_and_settle_each_id_out_of_order() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        for (id, line) in [("p1", 1), ("p2", 501), ("p3", 1001)] {
            upd(
                &mut s,
                json!({"type":"tool_start","id":id,"name":"read","summary":format!("read app.rs · from line {line}")}),
            );
        }
        assert_eq!(
            tool_statuses(&s),
            vec![("p1".into(), ToolStatus::Running)],
            "one ×3 card, anchored on the first id"
        );
        // Second and third dones land while the first is still outstanding.
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p2","name":"read","ok":false}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p3","name":"read","ok":true}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![("p1".into(), ToolStatus::Running)],
            "still running while p1 is outstanding — and no orphan card"
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"read","ok":true}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![("p1".into(), ToolStatus::Failed)],
            "a card summarizing three calls must not hide p2's failure"
        );
        let Some(TranscriptItem::Tool { calls, summary, .. }) = s.transcript.last() else {
            panic!("the merged card is the last item");
        };
        assert_eq!(calls.len(), 3, "all three ids on the one card");
        assert_eq!(summary, "read app.rs", "summary is the merge key");
    }

    /// 0039: a consecutive same-key call re-opens the settled card — Done
    /// re-enters Running and the card's clock resumes (on_tick ticks every
    /// running card, so duration accumulates across absorbed calls).
    #[test]
    fn consecutive_same_key_reads_absorb_and_resume_ticking() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"read","summary":"read app.rs · from line 1"}),
        );
        update(&mut s, Msg::Tick);
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"read","ok":true}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p2","name":"read","summary":"read app.rs · from line 501"}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![("p1".into(), ToolStatus::Running)],
            "the Done card re-enters Running"
        );
        update(&mut s, Msg::Tick);
        let Some(TranscriptItem::Tool { ticks, .. }) = s.transcript.last() else {
            panic!("the merged card is the last item");
        };
        assert_eq!(*ticks, 2, "the clock resumed instead of resetting");
    }

    /// 0061 T2: the counts on `tool_done` land on the call that settled, so
    /// the card can render `N lines` without the stream carrying the body.
    #[test]
    fn tool_done_records_line_and_byte_counts_on_its_call() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"cargo build"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"bash","ok":true,"lines":1204,"bytes":51233}),
        );
        let Some(TranscriptItem::Tool { calls, .. }) = s.transcript.last() else {
            panic!("the card is the last item");
        };
        assert_eq!(calls[0].lines, Some(1204));
        assert_eq!(calls[0].bytes, Some(51_233));
    }

    /// An older peer sends no counts. `None` is not `0`: the card must render
    /// no result row at all rather than claiming the tool printed nothing.
    #[test]
    fn tool_done_without_counts_leaves_them_none() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"bash","summary":"cargo build"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"bash","ok":true}),
        );
        let Some(TranscriptItem::Tool { calls, .. }) = s.transcript.last() else {
            panic!("the card is the last item");
        };
        assert_eq!(calls[0].lines, None);
        assert_eq!(calls[0].bytes, None);

        // The no-card fallback (§2b Respond) carries them too.
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type":"tool_done","id":"r1","name":"read","ok":true,"lines":3,"bytes":5}),
        );
        let Some(TranscriptItem::Tool { calls, .. }) = s.transcript.last() else {
            panic!("the fallback card is the last item");
        };
        assert_eq!((calls[0].lines, calls[0].bytes), (Some(3), Some(5)));
    }

    /// 0039 D3: failures are never laundered — a retry after a failed call
    /// starts a fresh card instead of being absorbed into the red one.
    #[test]
    fn a_failed_card_never_absorbs_the_retry() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"read","summary":"read app.rs"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"read","ok":false}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p2","name":"read","summary":"read app.rs"}),
        );
        assert_eq!(
            tool_statuses(&s),
            vec![
                ("p1".into(), ToolStatus::Failed),
                ("p2".into(), ToolStatus::Running),
            ],
            "the failure stays visible; the retry is its own card"
        );
    }

    /// 0039 D3: absorb reaches only the immediately previous item — an
    /// interleaving item breaks the run — and `spawn` never merges.
    #[test]
    fn merge_stops_at_an_interleaving_item_and_never_touches_spawn() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p1","name":"read","summary":"read app.rs"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p1","name":"read","ok":true}),
        );
        upd(&mut s, json!({"type":"text_delta","text":"looking…"}));
        upd(
            &mut s,
            json!({"type":"tool_start","id":"p2","name":"read","summary":"read app.rs"}),
        );
        assert_eq!(
            tool_statuses(&s).len(),
            2,
            "the assistant text between them breaks the run"
        );
        // Two same-summary spawns stay two cards.
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        for id in ["s1", "s2"] {
            upd(
                &mut s,
                json!({"type":"tool_start","id":id,"name":"spawn","summary":"spawn survey"}),
            );
        }
        assert_eq!(
            tool_statuses(&s),
            vec![
                ("s1".into(), ToolStatus::Running),
                ("s2".into(), ToolStatus::Running),
            ],
            "spawn never merges"
        );
    }

    /// 0039: interleaved `child_tool` frames route to their OWN spawn card by
    /// parent_id; a denied child (settled in one frame) lands failed.
    #[test]
    fn child_tool_frames_nest_under_their_own_spawn_card() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        for id in ["s1", "s2"] {
            upd(
                &mut s,
                json!({"type":"tool_start","id":id,"name":"spawn","summary":"spawn survey"}),
            );
        }
        // Interleaved: s2's child starts first, then s1's; s1's settles ok,
        // s2's second child arrives already denied.
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"s2","id":"c1","name":"read","summary":"read a.rs","phase":"start"}),
        );
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"s1","id":"c2","name":"grep","summary":"grep foo","phase":"start"}),
        );
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"s1","id":"c2","name":"grep","summary":"","phase":"done","ok":true}),
        );
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"s2","id":"c3","name":"write","summary":"write (denied)","phase":"done","ok":false}),
        );
        let children_of = |s: &State, spawn: &str| -> Vec<(String, Option<bool>)> {
            s.transcript
                .iter()
                .find_map(|i| match i {
                    TranscriptItem::Tool { id, children, .. } if id == spawn => Some(
                        children
                            .iter()
                            .map(|c| (c.id.clone(), c.ok))
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(
            children_of(&s, "s1"),
            vec![("c2".into(), Some(true))],
            "s1 owns only its own child, settled ok"
        );
        assert_eq!(
            children_of(&s, "s2"),
            vec![("c1".into(), None), ("c3".into(), Some(false))],
            "s2's running child plus the denied one, settled-failed"
        );
        // 0044: a done frame's `tokens` lands on the settled child; absent
        // stays `None`.
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"s2","id":"c1","name":"read","summary":"","phase":"done","ok":true,"tokens":777}),
        );
        let tokens_of = |s: &State, spawn: &str| -> Vec<Option<u64>> {
            s.transcript
                .iter()
                .find_map(|i| match i {
                    TranscriptItem::Tool { id, children, .. } if id == spawn => {
                        Some(children.iter().map(|c| c.tokens).collect::<Vec<_>>())
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(tokens_of(&s, "s2"), vec![Some(777), None]);
        assert_eq!(tokens_of(&s, "s1"), vec![None]);
        // Child activity must not perturb the parent's phase.
        assert!(matches!(s.phase, Phase::Tool { .. }), "{:?}", s.phase);
        // A frame whose parent card is gone (reattach) drops silently.
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"nope","id":"c9","name":"read","summary":"read b.rs","phase":"start"}),
        );
    }

    fn start_spawn(s: &mut State, id: &str) {
        upd(
            s,
            json!({"type":"tool_start","id":id,"name":"spawn","summary":format!("spawn {id}")}),
        );
    }

    fn settle(s: &mut State, id: &str) {
        upd(s, json!({"type":"tool_done","id":id,"ok":true}));
    }

    fn band_ids(s: &State) -> Vec<&str> {
        s.band_spawns()
            .into_iter()
            .filter_map(|i| match i {
                TranscriptItem::Tool { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 0058 T2: a child's prose accumulates on its OWN spawn card, is
    /// tail-capped, and never touches the parent's streaming phase.
    #[test]
    fn child_text_lands_on_its_own_card_tail_capped() {
        let mut s = State::new(false, "m".into());
        upd(
            &mut s,
            json!({"type":"tool_start","id":"s1","name":"spawn","summary":"spawn explore"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"s2","name":"spawn","summary":"spawn plan"}),
        );
        let phase_before = s.phase.clone();
        upd(
            &mut s,
            json!({"type":"child_text","parent_id":"s1","text":"reading "}),
        );
        upd(
            &mut s,
            json!({"type":"child_text","parent_id":"s1","text":"the parser"}),
        );
        upd(
            &mut s,
            json!({"type":"child_text","parent_id":"nope","text":"dropped"}),
        );
        assert_eq!(
            s.phase, phase_before,
            "child prose must not move the parent's phase"
        );
        let text_of_card = |s: &State, want: &str| -> String {
            s.transcript
                .iter()
                .find_map(|i| match i {
                    TranscriptItem::Tool { id, child_text, .. } if id == want => {
                        Some(child_text.clone())
                    }
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(text_of_card(&s, "s1"), "reading the parser");
        assert_eq!(text_of_card(&s, "s2"), "", "siblings never bleed");

        upd(
            &mut s,
            json!({"type":"child_text","parent_id":"s1","text":"x".repeat(CHILD_TEXT_CAP)}),
        );
        let kept = text_of_card(&s, "s1");
        assert_eq!(kept.chars().count(), CHILD_TEXT_CAP);
        assert!(kept.ends_with('x'), "the tail is what a watcher wants");
    }

    /// 0044: a running `workflow` card is a band row like a spawn, and two
    /// workflow cards with equal summaries never merge (D3's spawn rule).
    #[test]
    fn a_running_workflow_card_is_a_band_row_and_never_merges() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        let start = |s: &mut State, id: &str| {
            upd(
                s,
                json!({"type":"tool_start","id":id,"name":"workflow","summary":"workflow `x` — 1 phase"}),
            )
        };
        start(&mut s, "w1");
        start(&mut s, "w2");
        assert_eq!(band_ids(&s), vec!["w1", "w2"]);
        let cards = s
            .transcript
            .iter()
            .filter(|i| matches!(i, TranscriptItem::Tool { name, .. } if name == "workflow"))
            .count();
        assert_eq!(cards, 2, "equal summaries must not merge into one card");
        // Its forwarded agents land on the card like a spawn's children.
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"w1","id":"r:A:a:0","name":"agent","summary":"A · a","phase":"start"}),
        );
        upd(
            &mut s,
            json!({"type":"child_tool","parent_id":"w1","id":"r:A:a:0","name":"agent","summary":"","phase":"done","ok":true,"tokens":1200}),
        );
        let Some(TranscriptItem::Tool { children, .. }) = s
            .transcript
            .iter()
            .find(|i| matches!(i, TranscriptItem::Tool { id, .. } if id == "w1"))
        else {
            panic!()
        };
        assert_eq!(
            (children[0].ok, children[0].tokens),
            (Some(true), Some(1200))
        );
        settle(&mut s, "w1");
        assert_eq!(band_ids(&s), vec!["w2"]);
    }

    /// 0043 D1: a spawn row exists while its card runs — the settle drops it.
    #[test]
    fn settled_spawns_leave_the_band() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        start_spawn(&mut s, "s2");
        assert_eq!(band_ids(&s), vec!["s1", "s2"]);
        settle(&mut s, "s1");
        assert_eq!(band_ids(&s), vec!["s2"]);
        settle(&mut s, "s2");
        assert!(band_ids(&s).is_empty());
    }

    /// 0043 D1: a card stranded running by a cancelled turn must not hold
    /// the band open.
    #[test]
    fn a_stranded_running_spawn_leaves_the_band_when_the_turn_goes_idle() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        assert_eq!(band_ids(&s), vec!["s1"]);
        s.phase = Phase::Idle;
        assert!(band_ids(&s).is_empty());
    }

    /// 0043 D1: the shown spawn is pinned — settled or not, it stays listed
    /// until you leave its stream.
    #[test]
    fn the_shown_spawn_stays_in_the_band_after_it_settles_until_you_leave() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        s.selected_agent = Some("s1".into());
        settle(&mut s, "s1");
        assert_eq!(band_ids(&s), vec!["s1"], "pinned by the shown stream");
        press(&mut s, KeyCode::Esc); // Insert → Normal
        press(&mut s, KeyCode::Esc); // stream → main
        assert_eq!(s.selected_agent, None);
        assert!(band_ids(&s).is_empty());
    }

    /// 0043 D1: the highlighted spawn is pinned until the cursor moves off.
    #[test]
    fn the_highlighted_spawn_stays_in_the_band_until_you_move_off() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        start_spawn(&mut s, "s2");
        press(&mut s, KeyCode::Esc); // Insert → Normal
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Down);
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s2".into())));
        settle(&mut s, "s2");
        assert_eq!(band_ids(&s), vec!["s1", "s2"], "pinned by the cursor");
        press(&mut s, KeyCode::Up);
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s1".into())));
        assert_eq!(band_ids(&s), vec!["s1"]);
    }

    /// 0043: a `Main` cursor has nothing to sit in once the band is gone.
    #[test]
    fn the_band_cursor_clears_when_the_band_empties() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        press(&mut s, KeyCode::Esc);
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Up);
        assert_eq!(s.band_cursor, Some(BandRow::Main));
        settle(&mut s, "s1");
        assert_eq!(s.band_cursor, None);
    }

    /// 0043 D2: non-vim ↑/↓ move the band cursor while the band shows and
    /// the input is empty — clamped, no wrap, and no history recall fires.
    #[test]
    fn arrows_move_the_band_cursor_without_vim_mode_when_the_band_is_visible() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        start_spawn(&mut s, "s2");
        s.editor.remember("older".into());
        press(&mut s, KeyCode::Down);
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s1".into())));
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Down);
        assert_eq!(
            s.band_cursor,
            Some(BandRow::Spawn("s2".into())),
            "clamped at the last row"
        );
        press(&mut s, KeyCode::Up);
        press(&mut s, KeyCode::Up);
        assert_eq!(s.band_cursor, Some(BandRow::Main));
        press(&mut s, KeyCode::Up);
        assert_eq!(s.band_cursor, Some(BandRow::Main), "clamped at main");
        assert_eq!(s.editor.text(), "", "no recall fired");
    }

    /// 0043 D2: with no band, non-vim ↑/↓ recall history exactly as before.
    #[test]
    fn arrows_recall_history_without_vim_mode_when_no_band() {
        let mut s = State::new(false, "m".into());
        s.editor.remember("older".into());
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "older");
        assert_eq!(s.band_cursor, None);
    }

    /// 0043 D2: vim Insert keeps history on the arrows unconditionally.
    #[test]
    fn arrows_recall_history_in_vim_insert_even_with_a_band() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        s.editor.remember("older".into());
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "older");
        assert_eq!(s.band_cursor, None);
    }

    /// 0043 D2: vim Normal + empty input — j/k and ↑/↓ both move the band cursor.
    #[test]
    fn jk_and_arrows_move_the_band_cursor_in_vim_normal() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        press(&mut s, KeyCode::Esc); // Insert → Normal
        press(&mut s, KeyCode::Char('j'));
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s1".into())));
        press(&mut s, KeyCode::Char('k'));
        assert_eq!(s.band_cursor, Some(BandRow::Main));
        press(&mut s, KeyCode::Down);
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s1".into())));
    }

    /// 0043 D4: with no band, j/k fall back to their pre-0042 meaning — one
    /// transcript item per press.
    #[test]
    fn jk_scroll_the_transcript_with_no_band() {
        let mut s = State::test_default();
        s.transcript = notices(30);
        press(&mut s, KeyCode::Esc);
        press(&mut s, KeyCode::Char('k'));
        assert_eq!(s.scroll, Scroll::At(29));
        assert_eq!(s.band_cursor, None);
    }

    /// 0043 D3: Enter on a spawn row opens its stream; Enter on `main`
    /// returns. Neither submits, and the cursor stays engaged.
    #[test]
    fn enter_on_a_band_row_opens_its_stream_and_enter_on_main_returns() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        press(&mut s, KeyCode::Down);
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(s.selected_agent.as_deref(), Some("s1"));
        assert_eq!(s.agent_scroll, None);
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s1".into())));
        press(&mut s, KeyCode::Up);
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(s.selected_agent, None);
        assert_eq!(s.band_cursor, Some(BandRow::Main));
    }

    /// 0043 D5: esc in Normal walks the ladder — stream → main (cursor
    /// kept), cursor → disengaged, then interrupt.
    #[test]
    fn esc_in_normal_walks_stream_then_band_cursor_then_interrupt() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        press(&mut s, KeyCode::Esc); // Insert → Normal
        press(&mut s, KeyCode::Char('j'));
        press(&mut s, KeyCode::Enter);
        assert_eq!(s.selected_agent.as_deref(), Some("s1"));

        let cmds = press(&mut s, KeyCode::Esc); // stream → main
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(s.selected_agent, None);
        assert_eq!(
            s.band_cursor,
            Some(BandRow::Spawn("s1".into())),
            "back to main keeps the cursor"
        );

        let cmds = press(&mut s, KeyCode::Esc); // cursor → disengaged
        assert!(cmds.is_empty(), "{cmds:?}");
        assert_eq!(s.band_cursor, None);

        let cmds = press(&mut s, KeyCode::Esc); // now the turn
        assert!(cmds.contains(&Cmd::Cancel), "{cmds:?}");
    }

    /// 0043: a buffer going non-empty disengages the band cursor, and the
    /// typed char lands in the prompt.
    #[test]
    fn typing_disengages_the_band_cursor_and_lands_in_the_prompt() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        press(&mut s, KeyCode::Down);
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s1".into())));
        press(&mut s, KeyCode::Char('h'));
        assert_eq!(s.editor.text(), "h", "the char lands in the prompt");
        assert_eq!(s.band_cursor, None, "typing disengages");
    }

    /// 0039/0043: `/clear` drops the band cursor, the selection and its
    /// scroll — neither an id may dangle into the next transcript.
    #[test]
    fn clear_drops_band_cursor_selection_and_scroll() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        s.phase = Phase::Idle;
        s.selected_agent = Some("s1".into());
        s.agent_scroll = Some(1);
        s.band_cursor = Some(BandRow::Main);
        type_and_submit(&mut s, "/clear");
        assert_eq!(s.selected_agent, None);
        assert_eq!(s.agent_scroll, None);
        assert_eq!(s.band_cursor, None);
    }

    /// 0043: the cursor is id-based — a row settling above it never shifts
    /// the highlight onto another agent.
    #[test]
    fn a_settled_row_above_the_cursor_does_not_shift_the_highlight() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        for id in ["s1", "s2", "s3"] {
            start_spawn(&mut s, id);
        }
        press(&mut s, KeyCode::Esc);
        for _ in 0..3 {
            press(&mut s, KeyCode::Down);
        }
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s3".into())));
        settle(&mut s, "s1");
        assert_eq!(s.band_cursor, Some(BandRow::Spawn("s3".into())));
        assert_eq!(band_ids(&s), vec!["s2", "s3"]);
    }

    /// 0042 D2: esc in Insert always reaches the editor — empty buffer and
    /// running turn included. Pre-0042 this esc interrupted the turn, which
    /// made vim Normal unreachable mid-turn.
    #[test]
    fn esc_in_insert_enters_normal_and_never_interrupts() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(!cmds.contains(&Cmd::Cancel), "{cmds:?}");
        assert_eq!(s.editor.mode(), crate::vim::Mode::Normal);
    }

    /// 0039 D8: scroll keys target the shown child stream's line offset; the
    /// transcript's own scroll is untouched.
    #[test]
    fn scroll_keys_target_the_selected_child_stream() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        start_spawn(&mut s, "s1");
        for i in 0..12 {
            upd(
                &mut s,
                json!({"type":"child_tool","parent_id":"s1","id":format!("c{i}"),"name":"read","summary":format!("read c{i}.rs"),"phase":"start"}),
            );
        }
        s.selected_agent = Some("s1".into());
        press(&mut s, KeyCode::PageUp);
        assert!(s.agent_scroll.is_some(), "pgup scrolls the child stream");
        assert_eq!(s.scroll, Scroll::Follow, "the transcript never moved");
        press(&mut s, KeyCode::PageDown);
        assert_eq!(s.agent_scroll, None, "pgdn returns to follow-tail");
    }

    /// vim_mode = false stays byte-identical (0042): empty-editor esc still
    /// interrupts in one press.
    #[test]
    fn esc_with_empty_editor_still_interrupts_without_vim_mode() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(cmds.contains(&Cmd::Cancel), "{cmds:?}");
    }

    /// vim_mode = false (0042/0043 D6): Enter with text still submits — the
    /// band intercept needs an empty buffer.
    #[test]
    fn enter_still_submits_without_vim_mode() {
        let mut s = State::new(false, "m".into());
        s.transcript = notices(3);
        type_str(&mut s, "hi");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SendPrompt(_))),
            "{cmds:?}"
        );
        assert_eq!(s.band_cursor, None);
    }

    /// 0037: several `tool_auto_allowed` frames can park before any of their
    /// `tool_start`s arrive (a parallel chunk gates every call up front) —
    /// each rule must reach its own card, not the next start to happen by.
    #[test]
    fn parked_auto_rules_attach_by_id_not_arrival_order() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_auto_allowed","id":"a","name":"read","rule":"reads"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_auto_allowed","id":"b","name":"read","rule":"pages"}),
        );
        // Starts arrive in the opposite order.
        upd(
            &mut s,
            json!({"type":"tool_start","id":"b","name":"read","summary":"read y"}),
        );
        upd(
            &mut s,
            json!({"type":"tool_start","id":"a","name":"read","summary":"read x"}),
        );
        let statuses = tool_statuses(&s);
        assert_eq!(statuses[0].0, "b");
        assert!(
            matches!(&statuses[0].1, ToolStatus::AutoAllowed { rule } if rule == "pages"),
            "{statuses:?}"
        );
        assert!(
            matches!(&statuses[1].1, ToolStatus::AutoAllowed { rule } if rule == "reads"),
            "{statuses:?}"
        );
    }

    /// 0037: every running card ticks, not just the newest — and a sibling's
    /// tool_done flipping the phase to Streaming must not freeze the cards
    /// still running (pre-0037 they froze at 0s: ticks only reached the
    /// newest card, and only in Phase::Tool).
    #[test]
    fn every_running_card_ticks_settled_ones_do_not() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        // Distinct paths — distinct merge keys, so the three stay three
        // cards (same-key consecutive calls would merge, 0039 D3).
        for id in ["p1", "p2", "p3"] {
            upd(
                &mut s,
                json!({"type":"tool_start","id":id,"name":"read","summary":format!("read {id}.rs")}),
            );
        }
        // p3 settles; p1/p2 keep running and keep the phase in `Tool`.
        upd(
            &mut s,
            json!({"type":"tool_done","id":"p3","name":"read","ok":true}),
        );
        assert!(matches!(&s.phase, Phase::Tool { name, .. } if name == "read"));
        update(&mut s, Msg::Tick);
        update(&mut s, Msg::Tick);
        let ticks: Vec<(String, u64)> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Tool { id, ticks, .. } => Some((id.clone(), *ticks)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ticks,
            vec![("p1".into(), 2), ("p2".into(), 2), ("p3".into(), 0)],
            "running cards advance, the settled one holds"
        );
    }

    /// A `tool_done` with no card (§2b Respond skips execution, so no
    /// `tool_start` ever ran) still surfaces as a settled card — before ids
    /// it silently re-marked some older same-name card instead.
    #[test]
    fn a_done_without_a_start_becomes_its_own_settled_card() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(
            &mut s,
            json!({"type":"tool_done","id":"r1","name":"read","ok":true}),
        );
        assert_eq!(tool_statuses(&s), vec![("r1".into(), ToolStatus::Done)]);
    }

    #[test]
    fn permission_request_freezes_into_waiting_ask() {
        let mut s = State::test_default();
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 3,
        };
        update(
            &mut s,
            Msg::PermissionRequest {
                req_id: 7,
                summary: "run bash".into(),
                protected_why: Some("prod".into()),
                diff: Vec::new(),
            },
        );
        let before = s.phase.clone();
        assert!(
            matches!(&before, Phase::WaitingAsk { req_id: 7, summary, protected_why: Some(w), .. }
            if summary == "run bash" && w == "prod")
        );
        update(&mut s, Msg::Tick);
        assert_eq!(
            s.phase, before,
            "the loop halts — ticks do not advance in an ask"
        );
    }

    #[test]
    fn ask_y_allows_and_n_with_reason_denies() {
        let mut s = State::test_default();
        ask(&mut s);
        let cmds = press(&mut s, KeyCode::Char('y'));
        assert!(matches!(
            cmds[..],
            [
                Cmd::ReplyPermission {
                    req_id: 7,
                    allow: true,
                    secret_reads: false,
                    message: None
                },
                ..
            ]
        ));
        assert!(!matches!(s.phase, Phase::WaitingAsk { .. }));

        ask(&mut s);
        press(&mut s, KeyCode::Char('n'));
        type_str(&mut s, "wrong dir");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(
            matches!(&cmds[..], [Cmd::ReplyPermission { req_id: 7, allow: false, message: Some(m), .. }, ..]
            if m == "wrong dir")
        );
        assert!(!matches!(s.phase, Phase::WaitingAsk { .. }));
    }

    /// Plan 0026: two keys, both session-scoped, and anything else is inert
    /// rather than an accidental grant.
    #[test]
    fn egress_modal_answers_y_and_n_and_says_the_grant_is_session_scoped() {
        let raise = |s: &mut State| {
            update(
                s,
                Msg::EgressRequest {
                    req_id: 11,
                    host: "registry.npmjs.org".into(),
                },
            );
        };

        let mut s = State::test_default();
        raise(&mut s);
        assert!(matches!(s.phase, Phase::WaitingEgress { req_id: 11, .. }));
        // A stray key is not an answer.
        assert!(press(&mut s, KeyCode::Char('q')).is_empty());
        assert!(matches!(s.phase, Phase::WaitingEgress { .. }));

        let cmds = press(&mut s, KeyCode::Char('y'));
        assert!(
            matches!(
                cmds[..],
                [
                    Cmd::ReplyEgress {
                        req_id: 11,
                        allow: true
                    },
                    ..
                ]
            ),
            "{cmds:?}"
        );
        assert!(!matches!(s.phase, Phase::WaitingEgress { .. }));
        // The grant is session-scoped and hotl does not write config.toml, so
        // the transcript has to say where a permanent grant goes.
        let notices = format!("{:?}", s.transcript);
        assert!(
            notices.contains("for this session") && notices.contains("[network].allow"),
            "{notices}"
        );

        let mut s = State::test_default();
        raise(&mut s);
        let cmds = press(&mut s, KeyCode::Char('n'));
        assert!(
            matches!(
                cmds[..],
                [
                    Cmd::ReplyEgress {
                        req_id: 11,
                        allow: false
                    },
                    ..
                ]
            ),
            "{cmds:?}"
        );
    }

    /// Plan 0022: `s` allows *and* lifts the credential read-deny, but only
    /// on an ask the grant can reach. Everywhere else the key must fall
    /// through to the catch-all, because an option that does nothing is worse
    /// than no option.
    #[test]
    fn ask_s_grants_credential_reads_only_where_it_applies() {
        let ask_with = |s: &mut State, summary: &str| {
            update(
                s,
                Msg::PermissionRequest {
                    req_id: 7,
                    summary: summary.into(),
                    protected_why: None,
                    diff: Vec::new(),
                },
            );
        };
        let mut s = State::test_default();
        ask_with(&mut s, "bash [sandboxed:seatbelt]: cat ~/.ssh/id_ed25519");
        let cmds = press(&mut s, KeyCode::Char('s'));
        assert!(
            matches!(
                cmds[..],
                [
                    Cmd::ReplyPermission {
                        req_id: 7,
                        allow: true,
                        secret_reads: true,
                        message: None
                    },
                    ..
                ]
            ),
            "{cmds:?}"
        );

        for summary in [
            // Not bash: nothing spawns, so there is nothing to lift.
            "write ~/.ssh/config",
            // Already lifted by [sandbox].readable — the label says so.
            "bash [sandboxed:landlock reads:open]: cat ~/.ssh/id_ed25519",
        ] {
            let mut s = State::test_default();
            ask_with(&mut s, summary);
            assert!(
                press(&mut s, KeyCode::Char('s')).is_empty(),
                "`s` must be inert for `{summary}`"
            );
            assert!(
                matches!(s.phase, Phase::WaitingAsk { .. }),
                "`{summary}` must still be waiting"
            );
        }
    }

    fn question(s: &mut State) {
        update(
            s,
            Msg::QuestionRequest {
                req_id: 9,
                question: Question {
                    header: "Scope".into(),
                    prompt: "How far?".into(),
                    options: vec![
                        QuestionOption {
                            label: "MVP".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "Full".into(),
                            description: Some("everything".into()),
                        },
                    ],
                    multi: false,
                },
            },
        );
    }

    #[test]
    fn question_request_freezes_into_waiting_question_with_the_options() {
        let mut s = State::test_default();
        s.phase = Phase::Idle;
        question(&mut s);
        assert!(matches!(
            &s.phase,
            Phase::WaitingQuestion { req_id: 9, header, options, .. }
            if header == "Scope" && options.len() == 2
        ));
        update(&mut s, Msg::Tick);
        assert!(
            matches!(s.phase, Phase::WaitingQuestion { .. }),
            "the loop halts on a question exactly like a permission ask"
        );
    }

    #[test]
    fn selecting_option_index_by_digit_emits_the_answer_with_that_label() {
        let mut s = State::test_default();
        question(&mut s);
        let cmds = press(&mut s, KeyCode::Char('2'));
        assert!(matches!(
            &cmds[..],
            [Cmd::ReplyQuestion { req_id: 9, selected, free_text: None }, ..]
            if selected == &vec!["Full".to_string()]
        ));
        assert!(!matches!(s.phase, Phase::WaitingQuestion { .. }));
    }

    #[test]
    fn typing_instead_of_a_digit_switches_to_free_text_and_enter_submits_it() {
        let mut s = State::test_default();
        question(&mut s);
        type_str(&mut s, "neither, do it differently");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(
            &cmds[..],
            [Cmd::ReplyQuestion { req_id: 9, selected, free_text: Some(t) }, ..]
            if selected.is_empty() && t == "neither, do it differently"
        ));
        assert!(!matches!(s.phase, Phase::WaitingQuestion { .. }));
    }

    #[test]
    fn a_digit_out_of_range_is_a_no_op_and_a_digit_after_typing_is_just_text() {
        let mut s = State::test_default();
        question(&mut s);
        // Only 2 options: '9' is out of range and does not submit.
        let cmds = press(&mut s, KeyCode::Char('9'));
        assert!(cmds.is_empty());
        assert!(matches!(s.phase, Phase::WaitingQuestion { .. }));

        // Once free text has started, a digit is just another character.
        type_str(&mut s, "opt");
        press(&mut s, KeyCode::Char('2'));
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(
            &cmds[..],
            [Cmd::ReplyQuestion { free_text: Some(t), .. }, ..] if t == "opt2"
        ));
    }

    #[test]
    fn typing_mid_turn_queues_steer() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        type_str(&mut s, "wait");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(&cmds[..], [Cmd::SendSteer(t)] if t.text == "wait"));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Steer { queued: true, .. })
        ));
        upd(&mut s, json!({"type":"prompt_queued"}));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Steer { queued: false, .. })
        ));
    }

    #[test]
    fn steer_rejected_clears_the_chip_and_notices_why() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        type_str(&mut s, "wait");
        press(&mut s, KeyCode::Enter);
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Steer { queued: true, .. })
        ));
        update(
            &mut s,
            Msg::SteerRejected {
                why: "images[0] is empty".into(),
            },
        );
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("images[0]")),
            "the rejection reason must reach the transcript"
        );
        let chip = s
            .transcript
            .iter()
            .rev()
            .find(|i| matches!(i, TranscriptItem::Steer { .. }));
        assert!(
            matches!(chip, Some(TranscriptItem::Steer { queued: false, .. })),
            "the pinned chip must not outlive the rejection"
        );
    }

    /// A rejection with no matching queued chip (already cleared by a racing
    /// `prompt_queued`, or a stale id) must degrade to a notice-only, not panic.
    #[test]
    fn steer_rejected_with_no_queued_chip_only_notices() {
        let mut s = State::test_default();
        update(&mut s, Msg::SteerRejected { why: "boom".into() });
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Notice { text }) if text.contains("boom")
        ));
    }

    #[test]
    fn esc_interrupts_then_second_esc_takes_control_back() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc); // Insert → Normal (0042 D2)
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent);
        let cmds = press(&mut s, KeyCode::Esc);
        assert_eq!(s.phase, Phase::Idle, "the second esc hands the prompt back");
        assert!(!s.interrupt_sent);
        assert_eq!(s.detached_turns, 1);
        assert!(cmds.contains(&Cmd::Cancel), "the dying turn is still told");
        assert!(
            matches!(cmds.last(), Some(Cmd::SetTitle(t)) if t == "hotl"),
            "the working suffix is dropped: {cmds:?}"
        );
    }

    /// The wire is FIFO, so everything a detached turn emits arrives before
    /// its prompt result. None of it may touch the phase the user took back —
    /// only durable session state (mode, todos) still lands.
    #[test]
    fn a_detached_turns_updates_and_asks_cannot_reclaim_the_screen() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc); // Insert → Normal (0042 D2)
        press(&mut s, KeyCode::Esc); // interrupt
        press(&mut s, KeyCode::Esc); // detach
        let items = s.transcript.len();
        upd(&mut s, json!({"type":"text_delta","text":"zombie"}));
        assert_eq!(
            s.phase,
            Phase::Idle,
            "a dead turn's delta restarted the spinner"
        );
        assert_eq!(s.transcript.len(), items);
        update(
            &mut s,
            Msg::PermissionRequest {
                req_id: 9,
                summary: "write ./x".into(),
                protected_why: None,
                diff: Vec::new(),
            },
        );
        assert_eq!(s.phase, Phase::Idle, "a dead turn's ask opened a modal");
        upd(&mut s, json!({"type":"mode_changed","mode":"plan"}));
        assert_eq!(s.mode, "plan", "durable session state still lands");
    }

    /// After a detach the old turn's result must be absorbed — usage folds
    /// into the session totals (those tokens were billed) but the phase
    /// belongs to whatever the user is doing now.
    #[test]
    fn a_detached_turns_late_result_is_absorbed_without_clobbering_a_new_turn() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc); // Insert → Normal (0042 D2)
        press(&mut s, KeyCode::Esc); // interrupt
        press(&mut s, KeyCode::Esc); // detach
        press(&mut s, KeyCode::Char('i')); // back to Insert to type
        type_str(&mut s, "hi");
        press(&mut s, KeyCode::Enter);
        assert!(matches!(s.phase, Phase::Sampling { .. }));
        let cmds = on_result(&mut s, "cancelled", None, &json!({"input_tokens": 7}));
        assert!(
            matches!(s.phase, Phase::Sampling { .. }),
            "the dead turn's result yanked the new turn back to idle"
        );
        assert_eq!(s.detached_turns, 0);
        assert_eq!(s.session_usage.input, 7, "billed tokens still count");
        assert!(
            cmds.is_empty(),
            "no title/notice churn for an abandoned turn"
        );
        // The next result is the live turn's and lands normally.
        on_result(&mut s, "done", None, &json!({}));
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn ctrl_c_escalates_to_quit_on_the_second_press() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent, "the first ctrl-c is an interrupt");
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
    }

    #[test]
    fn ctrl_c_after_an_esc_interrupt_quits() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc); // Insert → Normal (0042 D2)
        press(&mut s, KeyCode::Esc); // interrupt
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
    }

    /// 0049 T6 (LD5): `↓` moves the cursor and `Enter` picks exactly what
    /// the digit would; PageDown scrolls the body without answering.
    #[test]
    fn question_options_list_with_a_cursor_and_enter_picks() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::QuestionRequest {
                req_id: 9,
                question: Question {
                    header: "h".into(),
                    prompt: "p".into(),
                    options: vec![
                        QuestionOption {
                            label: "A".into(),
                            description: Some("first".into()),
                        },
                        QuestionOption {
                            label: "B".into(),
                            description: None,
                        },
                    ],
                    multi: false,
                },
            },
        );
        press(&mut s, KeyCode::PageDown);
        assert_eq!(s.modal_scroll, MODAL_PAGE, "pgdn scrolls, never answers");
        assert!(matches!(s.phase, Phase::WaitingQuestion { .. }));
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Down);
        assert!(
            matches!(s.phase, Phase::WaitingQuestion { selected: 1, .. }),
            "clamped at the last option: {:?}",
            s.phase
        );
        let cmds = press(&mut s, KeyCode::Enter);
        // The same reply shape the digit path produces for option 2.
        assert!(matches!(s.phase, Phase::Sampling { .. }));
        assert!(
            matches!(&cmds[..], [Cmd::ReplyQuestion { req_id: 9, selected, free_text: None }, ..]
                if selected == &vec!["B".to_string()]),
            "{cmds:?}"
        );
    }

    /// 0049 T7: the help table scrolls on ↓/pgdn and closes on anything else.
    #[test]
    fn help_scrolls_on_arrows_and_page_keys_and_closes_on_any_other_key() {
        let mut s = State::test_default();
        press(&mut s, KeyCode::Char('?'));
        assert!(s.help_open);
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::PageDown);
        assert!(s.help_open, "scroll keys keep it open");
        assert_eq!(s.modal_scroll, 1 + MODAL_PAGE);
        press(&mut s, KeyCode::Up);
        assert_eq!(s.modal_scroll, MODAL_PAGE);
        press(&mut s, KeyCode::Char('x'));
        assert!(!s.help_open, "any other key closes");
        assert!(s.editor.is_empty(), "the closing key is swallowed");
        press(&mut s, KeyCode::Char('?'));
        assert_eq!(s.modal_scroll, 0, "reopening starts at the top");
    }

    #[test]
    fn ctrl_c_is_never_swallowed_by_the_help_overlay() {
        let mut s = State::test_default();
        s.help_open = true;
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
        assert!(!s.help_open);
    }

    /// Esc in the ask picker joins the same ladder: the modal is the model
    /// waiting on you, and wanting out of it is wanting the turn gone.
    #[test]
    fn esc_interrupts_from_the_ask_picker_and_detaches_on_the_second_press() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        ask(&mut s);
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.phase, Phase::Idle, "the second esc closes the modal too");
    }

    #[test]
    fn esc_interrupts_from_the_question_picker() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::QuestionRequest {
                req_id: 3,
                question: Question {
                    header: "Pick".into(),
                    prompt: "which?".into(),
                    options: vec![],
                    multi: false,
                },
            },
        );
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent);
    }

    #[test]
    fn prompt_result_returns_to_idle_with_usage() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 4, chars: 9 };
        let cmds = update(
            &mut s,
            Msg::PromptResult {
                outcome_kind: "done".into(),
                outcome_text: Some("fin".into()),
                usage: json!({"input_tokens": 120, "output_tokens": 45}),
                finished_at: None,
            },
        );
        assert_eq!(s.phase, Phase::Idle);
        // Session totals; no cache segment (this turn read none), no cost
        // (the payload carried none), and the gauge is the strip's chip.
        assert_eq!(s.usage_line.as_deref(), Some("120 in · 45 out"));
        assert_eq!(s.live_context, Some(120));
        assert!(matches!(&cmds[..], [Cmd::SetTitle(t)] if t == "hotl"));
    }

    // `no_unreachable_phase_variants` retired here (0061 T23, decision 14):
    // the engine now emits a compaction-*start* event, and the fold renders as
    // a strip segment rather than a `Phase` variant — so a source grep for the
    // word `Compacting` no longer says anything about reachability, and it was
    // never a reachability check to begin with.

    #[test]
    fn compacted_and_retrying_become_notices() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"compacted","degraded":false}));
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("folded"))
        );
        upd(
            &mut s,
            json!({"type":"retrying","attempt":2,"reason":"overloaded"}),
        );
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("overloaded"))
        );
    }

    /// 0050 T3: a re-sample regenerates the answer, so the half-written one
    /// has to come off the screen — otherwise the turn reads as two replies.
    #[test]
    fn a_discarded_partial_is_taken_back_off_the_transcript() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"text_delta","text":"half a th"}));
        upd(
            &mut s,
            json!({"type":"retrying","attempt":1,"reason":"stream interrupted","discarded_partial":true}),
        );
        assert!(
            !s.transcript
                .iter()
                .any(|i| matches!(i, TranscriptItem::Assistant { .. })),
            "the partial bubble survived: {:?}",
            s.transcript
        );
        upd(
            &mut s,
            json!({"type":"text_delta","text":"the whole answer"}),
        );
        let full: Vec<&str> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(full, vec!["the whole answer"]);
    }

    /// The retry only takes back what *this* sample wrote: text the previous
    /// sample already finished stays.
    #[test]
    fn a_discarded_partial_keeps_the_text_that_came_before_it() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"text_delta","text":"settled. "}));
        // Any non-delta frame closes the sample and marks the boundary.
        upd(&mut s, json!({"type":"prompt_queued"}));
        upd(&mut s, json!({"type":"text_delta","text":"half a th"}));
        upd(
            &mut s,
            json!({"type":"retrying","attempt":1,"reason":"x","discarded_partial":true}),
        );
        let kept: Vec<&str> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(kept, vec!["settled. "]);
    }

    /// A pre-stream retry rendered nothing, so it takes nothing back.
    #[test]
    fn a_retry_without_the_flag_leaves_the_transcript_alone() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"text_delta","text":"keep me"}));
        upd(
            &mut s,
            json!({"type":"retrying","attempt":1,"reason":"429"}),
        );
        assert!(s
            .transcript
            .iter()
            .any(|i| matches!(i, TranscriptItem::Assistant { text } if text == "keep me")));
    }

    #[test]
    fn tick_only_advances_active_phases() {
        let mut s = State::test_default();
        let cmds = update(&mut s, Msg::Tick);
        assert!(cmds.is_empty());
        assert_eq!(s.phase, Phase::Idle);
        s.phase = Phase::Sampling { ticks: 0 };
        update(&mut s, Msg::Tick);
        assert!(matches!(s.phase, Phase::Sampling { ticks: 1 }));
    }

    #[test]
    fn work_ticks_advances_running_pauses_blocked_and_resets_at_turn_end() {
        // The whole-turn clock the activity animation rides (`anim::snake`).
        let mut s = State::test_default();

        // Idle does not advance it.
        update(&mut s, Msg::Tick);
        assert_eq!(s.work_ticks, 0, "idle must not advance the animation clock");

        // A running turn advances it — and keeps advancing across a
        // thinking → tool switch, which resets the *per-phase* ticks but not
        // this clock. That continuity is the whole reason it exists.
        s.phase = Phase::Sampling { ticks: 0 };
        update(&mut s, Msg::Tick);
        update(&mut s, Msg::Tick);
        assert_eq!(s.work_ticks, 2);
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 0,
        };
        update(&mut s, Msg::Tick);
        assert_eq!(s.work_ticks, 3, "the clock survives a sub-phase change");

        // Blocked on the user: the cycle freezes where it stood.
        s.phase = Phase::WaitingAsk {
            req_id: 1,
            summary: "s".into(),
            protected_why: None,
            input: String::new(),
            denying: false,
            diff: Vec::new(),
        };
        update(&mut s, Msg::Tick);
        assert_eq!(s.work_ticks, 3, "a blocked prompt freezes the cycle");

        // Turn end restarts the cycle from 0 — here via the esc-detach ladder
        // (the other reset is on a normal prompt result).
        s.phase = Phase::Sampling { ticks: 0 };
        s.interrupt_sent = true; // an interrupt esc already sent
        press(&mut s, KeyCode::Esc); // Insert → Normal (0042 D2)
        press(&mut s, KeyCode::Esc); // the next esc abandons the turn
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.work_ticks, 0, "a new turn restarts the animation cycle");
    }

    #[test]
    fn ctrl_c_quits_when_idle_cancels_when_running() {
        let mut s = State::test_default();
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Cancel]));
    }

    fn with_skills(names: &[(&str, &str)]) -> State {
        let mut s = State::test_default();
        for (name, description) in names {
            s.commands.push(crate::complete::Command {
                name: (*name).into(),
                description: (*description).into(),
                builtin: false,
            });
            s.skills.push((*name).into());
        }
        s
    }

    fn selected(s: &State) -> String {
        let c = s.completion.as_ref().expect("popup open");
        s.commands[c.matches[c.selected]].name.clone()
    }

    #[test]
    fn typing_a_slash_opens_the_popup_and_narrows_as_you_type() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/");
        // Twelve built-ins plus the one skill.
        assert_eq!(s.completion.as_ref().map(|c| c.matches.len()), Some(14));
        type_str(&mut s, "re");
        assert_eq!(selected(&s), "reload");
        // `reload`, `rename` and `review` prefix-match; no other built-in
        // contains "re".
        assert_eq!(s.completion.as_ref().map(|c| c.matches.len()), Some(3));
    }

    #[test]
    fn arrows_move_the_selection_and_saturate_at_both_ends() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        press(&mut s, KeyCode::Down);
        assert_eq!(selected(&s), "rename");
        assert_eq!(
            s.editor.text(),
            "/re",
            "the popup owns the arrows while open — no history recall, no buffer change"
        );
        press(&mut s, KeyCode::Up);
        press(&mut s, KeyCode::Up);
        assert_eq!(selected(&s), "reload", "up saturates at the top");
        for _ in 0..10 {
            press(&mut s, KeyCode::Down);
        }
        assert_eq!(selected(&s), "review", "down saturates at the bottom");
    }

    #[test]
    fn tab_completes_the_selection_without_starting_a_turn() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        let cmds = press(&mut s, KeyCode::Tab);
        assert!(cmds.is_empty(), "tab is not a submit: {cmds:?}");
        assert_eq!(s.editor.text(), "/reload ");
        assert!(
            s.completion.is_none(),
            "the trailing space closes the popup"
        );
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn enter_runs_the_highlighted_command_not_the_literal_text() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/pl");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(&cmds[..], [Cmd::SetPlan(true)]), "got {cmds:?}");
        assert!(
            !matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("unknown")),
            "the partial word must never reach slash_command"
        );
    }

    /// Finding 5: the one interaction the `State::skills`/`State::commands`
    /// split risks — both are populated at a single site (`tui.rs`) and
    /// cannot drift today, but nothing previously exercised selecting a
    /// *skill* row (as opposed to a builtin) through the popup and running
    /// it. `enter_runs_the_highlighted_command_not_the_literal_text` covers
    /// the builtin case; this is its skill-row counterpart.
    #[test]
    fn selecting_a_skill_in_the_popup_and_pressing_enter_dispatches_it() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/rev");
        assert_eq!(selected(&s), "review", "the skill is the highlighted match");
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(
            p.text.contains("Load the skill `review`"),
            "the popup selection must dispatch the skill, not the literal typed word: {}",
            p.text
        );
    }

    #[test]
    fn esc_dismisses_and_stays_dismissed_until_the_slash_is_gone() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        press(&mut s, KeyCode::Esc);
        assert!(s.completion.is_none());
        type_str(&mut s, "n");
        assert!(s.completion.is_none(), "still dismissed while typing");

        // Clearing the buffer past the slash re-arms it.
        for _ in 0.."/ren".chars().count() {
            press(&mut s, KeyCode::Backspace);
        }
        type_str(&mut s, "/re");
        assert!(s.completion.is_some(), "a fresh slash opens it again");
    }

    /// Esc is layered: it belongs to the popup first, and only reaches the
    /// editor's Insert→Normal transition once the popup is gone.
    #[test]
    fn the_first_esc_dismisses_the_popup_and_the_second_reaches_normal_mode() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.editor.mode(), crate::vim::Mode::Insert);
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.editor.mode(), crate::vim::Mode::Normal);
    }

    /// Finding 1 (blocking): reverse-i-search must own the keyboard the
    /// instant it starts. Before the fix, `state.completion` survived the
    /// `Ctrl-R` that started the search, so the popup's own Esc handler
    /// swallowed the first Esc — the search only ended on the second one.
    #[test]
    fn ctrl_r_closes_a_stale_popup_and_the_first_esc_ends_the_search() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        assert!(s.completion.is_some(), "popup open before ctrl-r");
        ctrl(&mut s, 'r');
        assert!(
            s.editor.search_prompt().is_some(),
            "ctrl-r must still start the search"
        );
        assert!(
            s.completion.is_none(),
            "the search owns the input area now — the popup must not survive it"
        );
        press(&mut s, KeyCode::Esc);
        assert!(
            s.editor.search_prompt().is_none(),
            "one esc must end the search outright, not get swallowed by a stale popup"
        );
    }

    /// A permission ask arriving mid-typing (the popup was open on a partial
    /// `/` word) must close the popup immediately — the ask owns the
    /// keyboard, and a stale menu must not linger over its card.
    #[test]
    fn a_permission_ask_mid_typing_closes_the_open_popup() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        assert!(s.completion.is_some(), "popup open before the ask arrives");
        ask(&mut s);
        assert!(
            s.completion.is_none(),
            "the ask must close a popup left open from mid-typing"
        );
    }

    #[test]
    fn an_argument_closes_the_popup_before_submit() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/mode ");
        assert!(s.completion.is_none());
        let cmds = type_and_submit(&mut s, "bypass");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "bypass"),
            "got {cmds:?}"
        );
    }

    /// The `$EDITOR` round trip replaces the whole buffer outside the normal
    /// key path. If the popup from a stale `/re` survives that replacement,
    /// the next Enter splices against a selection that no longer describes
    /// what's on screen — silently destroying the user's freehand prompt in
    /// favor of a bogus `/rename`.
    #[test]
    fn editor_done_clears_a_stale_popup_so_enter_submits_the_real_text() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        assert!(
            s.completion.is_some(),
            "popup open before the editor round trip"
        );
        update(&mut s, Msg::EditorDone(Ok(Some("explain the bug".into()))));
        assert!(
            s.completion.is_none(),
            "the popup must not survive a buffer replaced out from under it"
        );
        let cmds = press(&mut s, KeyCode::Enter);
        assert_eq!(s.editor.text(), "");
        assert!(
            matches!(
                &cmds[..],
                [Cmd::AppendHistory(h), Cmd::SendPrompt(p), Cmd::SetTitle(_)]
                    if h == "explain the bug" && p.text == "explain the bug"
            ),
            "the editor's real content must reach the model unchanged, got {cmds:?}"
        );
    }

    /// The regression the `Result` exists for: an editor that never started
    /// must not look like an editor the user closed without changing anything.
    ///
    /// Both used to be `None`, so on a box with no POSIX shell the key did
    /// nothing at all and said nothing — indistinguishable from a no-op, every
    /// time, forever.
    #[test]
    fn an_editor_that_never_ran_says_so_instead_of_looking_like_a_no_op() {
        let mut s = State::new(false, "m".into());

        // Aborted or unchanged: silent, and the draft is untouched.
        update(&mut s, Msg::EditorDone(Ok(None)));
        assert!(
            !matches!(s.transcript.last(), Some(TranscriptItem::Notice { .. })),
            "an abort must stay silent"
        );

        // Never started: the reason reaches the transcript.
        update(
            &mut s,
            Msg::EditorDone(Err("cannot open $EDITOR: no POSIX shell resolved".into())),
        );
        assert!(
            matches!(
                s.transcript.last(),
                Some(TranscriptItem::Notice { text }) if text.contains("no POSIX shell")
            ),
            "a failure must name itself: {:?}",
            s.transcript.last()
        );
    }

    fn type_and_submit(s: &mut State, text: &str) -> Vec<Cmd> {
        type_str(s, text);
        press(s, KeyCode::Enter)
    }

    #[test]
    fn slash_rename_sets_name_emits_cmd_and_title_not_a_prompt() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/rename fix-auth");
        assert_eq!(s.session_name.as_deref(), Some("fix-auth"));
        assert!(
            matches!(&cmds[..], [Cmd::Rename(n), Cmd::SetTitle(t)]
                if n == "fix-auth" && t == "hotl · fix-auth"),
            "got {cmds:?}"
        );
        assert_eq!(s.phase, Phase::Idle, "a slash command never starts a turn");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("fix-auth"))
        );
    }

    #[test]
    fn slash_rename_without_arg_shows_usage() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/rename");
        assert!(cmds.is_empty());
        assert_eq!(s.session_name, None);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("usage"))
        );
    }

    /// `/plan` is a toggle on its own axis now: it never touches `s.mode`,
    /// and a second invocation turns it back off.
    #[test]
    fn slash_plan_toggles_and_does_not_start_a_turn() {
        let mut s = State::test_default();
        let before = s.mode.clone();
        let cmds = type_and_submit(&mut s, "/plan");
        assert!(matches!(&cmds[..], [Cmd::SetPlan(true)]), "got {cmds:?}");
        assert!(s.plan);
        assert_eq!(s.mode, before, "the plan toggle must not move the mode");
        assert_eq!(s.phase, Phase::Idle);

        let cmds = type_and_submit(&mut s, "/plan");
        assert!(matches!(&cmds[..], [Cmd::SetPlan(false)]), "got {cmds:?}");
        assert!(!s.plan);
    }

    /// `on`/`off` exist because a bare toggle is a race for scripted input.
    #[test]
    fn slash_plan_accepts_explicit_on_and_off() {
        let mut s = State::test_default();
        assert!(matches!(
            &type_and_submit(&mut s, "/plan on")[..],
            [Cmd::SetPlan(true)]
        ));
        assert!(matches!(
            &type_and_submit(&mut s, "/plan on")[..],
            [Cmd::SetPlan(true)],
        ));
        assert!(s.plan, "`on` is idempotent, not a toggle");
        assert!(matches!(
            &type_and_submit(&mut s, "/plan off")[..],
            [Cmd::SetPlan(false)]
        ));
        assert!(!s.plan);
        // Anything else is usage, not a silent no-op.
        assert!(type_and_submit(&mut s, "/plan sideways").is_empty());
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("usage"))
        );
    }

    /// `/mode plan` was valid before the split. It must point at `/plan`
    /// rather than read as a typo.
    #[test]
    fn slash_mode_plan_redirects_to_the_toggle() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode plan");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(!s.plan, "the redirect notice must not also flip the axis");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("/plan"))
        );
    }

    #[test]
    fn slash_mode_sets_the_named_mode() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode dontask");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "dontask"),
            "got {cmds:?}"
        );
        assert_eq!(s.mode, "dontask");
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn slash_mode_accepts_dont_ask_alias_via_shared_parser() {
        // Finding 2 (Plan 2 review, MINOR): the old hardcoded
        // ["ask","auto","plan","dontask"] list rejected the `dont_ask`
        // alias that `PermissionMode::from_str` (and ACP) accept. Now that
        // `/mode` delegates to that parser, the alias must work, and the
        // canonical `as_str()` form ("dontask") is what gets sent/stored —
        // not the raw alias the user typed.
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode dont_ask");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "dontask"),
            "got {cmds:?}"
        );
        assert_eq!(s.mode, "dontask");
    }

    #[test]
    fn slash_mode_unknown_shows_usage_and_never_reaches_model() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode wat");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert_eq!(s.phase, Phase::Idle);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("usage"))
        );
    }

    #[test]
    fn slash_effort_sets_the_level() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/effort xhigh");
        assert!(
            matches!(&cmds[..], [Cmd::SetEffort(Some(e))] if e == "xhigh"),
            "got {cmds:?}"
        );
        assert_eq!(s.effort.as_deref(), Some("xhigh"));
        assert_eq!(s.phase, Phase::Idle);
        // The alias goes through the same parser the wire uses, and the
        // canonical spelling is what gets stored and sent.
        let cmds = type_and_submit(&mut s, "/effort x-high");
        assert!(
            matches!(&cmds[..], [Cmd::SetEffort(Some(e))] if e == "xhigh"),
            "got {cmds:?}"
        );
    }

    /// No cycling: five rungs are unguessable, unlike `/plan`'s two states.
    #[test]
    fn slash_effort_bare_reports_and_emits_no_cmd() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/effort");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("default"))
        );
    }

    /// 0030 Task 8: bare `/effort` reports the resolved value, never the lie
    /// "default" when the session genuinely runs at the seeded default.
    #[test]
    fn slash_effort_bare_reports_the_session_default_honestly() {
        // Unset with a known session default: named, and marked as a default.
        let mut s = State::test_default();
        s.default_effort = Some("xhigh".into());
        type_and_submit(&mut s, "/effort");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("effort xhigh (default)")),
            "got {:?}",
            s.transcript.last()
        );
        // An explicit set wins, with no default marker.
        type_and_submit(&mut s, "/effort max");
        type_and_submit(&mut s, "/effort");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("effort max") && !text.contains("(default)"))
        );
        // An explicit clear means the provider default — NOT the session
        // default the handshake seeded.
        type_and_submit(&mut s, "/effort default");
        type_and_submit(&mut s, "/effort");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("effort default") && !text.contains("xhigh")),
            "got {:?}",
            s.transcript.last()
        );
    }

    /// The other surface's clear also stops the default being reported.
    #[test]
    fn effort_changed_null_drops_the_session_default() {
        let mut s = State::test_default();
        s.default_effort = Some("xhigh".into());
        upd(&mut s, json!({"type": "effort_changed", "effort": null}));
        assert_eq!(s.effort, None);
        assert_eq!(s.default_effort, None);
    }

    #[test]
    fn slash_effort_default_clears_it() {
        let mut s = State::test_default();
        type_and_submit(&mut s, "/effort max");
        let cmds = type_and_submit(&mut s, "/effort default");
        assert!(matches!(&cmds[..], [Cmd::SetEffort(None)]), "got {cmds:?}");
        assert_eq!(s.effort, None);
    }

    /// `ultra` is the word someone arriving from another harness will try —
    /// the usage line naming the five rungs is what makes that recoverable.
    #[test]
    fn slash_effort_unknown_shows_usage_and_never_reaches_model() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/effort ultra");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.effort, None);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("usage"))
        );
    }

    #[test]
    fn status_line_shows_the_effort() {
        let mut s = State::test_default();
        type_and_submit(&mut s, "/status");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("effort default")),
            "got {:?}",
            s.transcript.last()
        );
        type_and_submit(&mut s, "/effort high");
        type_and_submit(&mut s, "/status");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("effort high"))
        );
    }

    /// 0036: a `tool_flagged` update is a loud Notice plus the running chip
    /// count — allowed and refused directions worded apart, and the count
    /// only ever grows.
    #[test]
    fn tool_flagged_notices_and_bumps_the_chip_count() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type": "tool_flagged", "name": "write", "summary": "write Makefile",
                   "why": "protected write", "denied": false}),
        );
        assert_eq!(s.flag_count, 1);
        assert!(
            last_notice(&s).starts_with("⚑ allowed with notice: write Makefile"),
            "{}",
            last_notice(&s)
        );
        upd(
            &mut s,
            json!({"type": "tool_flagged", "name": "write", "summary": "write /outside",
                   "why": "outside-session write", "denied": true}),
        );
        assert_eq!(s.flag_count, 2, "the chip is a running count");
        assert!(
            last_notice(&s).starts_with("⚑ refused: write /outside"),
            "{}",
            last_notice(&s)
        );
    }

    /// A change made by another attached surface reaches this one.
    #[test]
    fn effort_changed_updates_the_state() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type": "effort_changed", "effort": "max"}));
        assert_eq!(s.effort.as_deref(), Some("max"));
        // Null is "the provider's own default", not "missing".
        upd(&mut s, json!({"type": "effort_changed", "effort": null}));
        assert_eq!(s.effort, None);
    }

    // --- /goal (plan 0034) ------------------------------------------

    #[test]
    fn slash_goal_when_idle_sets_and_submits_the_condition_as_the_prompt() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/goal   all tests pass  ");
        // Set first, then the prompt: the engine's command channel is FIFO,
        // so the goal is armed before the turn it should gate is admitted.
        assert!(
            matches!(
                &cmds[..],
                [Cmd::SetGoal(Some(g)), Cmd::SendPrompt(p), Cmd::SetTitle(_)]
                    if g == "all tests pass" && p.text == "all tests pass" && p.images.is_empty()
            ),
            "got {cmds:?}"
        );
        assert_eq!(s.goal.as_deref(), Some("all tests pass"));
        assert_eq!(s.phase, Phase::Sampling { ticks: 0 });
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::User { text }) if text == "all tests pass"),
            "the condition is the directive, and it shows as one"
        );
        // The notice precedes the directive it announces.
        let n = s.transcript.len();
        assert!(
            matches!(&s.transcript[n - 2], TranscriptItem::Notice { text } if text.as_str().contains("working toward it now")),
            "{:?}",
            s.transcript[n - 2]
        );
    }

    /// Mid-turn, `/goal` only arms the gate (it fires when this turn ends);
    /// nothing is submitted and nothing is steered.
    #[test]
    fn slash_goal_while_a_turn_runs_only_sets() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 7 };
        let before = s.transcript.len();
        let cmds = type_and_submit(&mut s, "/goal ship it");
        assert!(
            matches!(&cmds[..], [Cmd::SetGoal(Some(g))] if g == "ship it"),
            "got {cmds:?}"
        );
        assert_eq!(s.phase, Phase::Sampling { ticks: 7 });
        assert!(
            !s.transcript[before..].iter().any(|i| matches!(
                i,
                TranscriptItem::User { .. } | TranscriptItem::Steer { .. }
            )),
            "a mid-turn set neither prompts nor steers"
        );
        assert!(
            last_notice(&s).contains("keeps going until it is met"),
            "{}",
            last_notice(&s)
        );
    }

    #[test]
    fn slash_goal_every_clear_word_ends_it_and_clearing_nothing_stays_local() {
        for word in ["clear", "stop", "off", "reset", "none", "cancel"] {
            let mut s = State::test_default();
            type_and_submit(&mut s, "/goal ship it");
            s.goal_ticks = 5;
            s.goal_turns = 2;
            let cmds = type_and_submit(&mut s, &format!("/goal {word}"));
            assert!(
                matches!(&cmds[..], [Cmd::SetGoal(None)]),
                "{word}: {cmds:?}"
            );
            assert_eq!(s.goal, None, "{word}");
            assert_eq!((s.goal_ticks, s.goal_turns), (0, 0), "{word}");
        }
        // Clearing when none is active sends nothing — the engine treats it
        // as a no-op and so does the surface.
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/goal clear");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(
            last_notice(&s).contains("no goal set"),
            "{}",
            last_notice(&s)
        );
    }

    /// 0051: clearing echoes what it ended — a goal set an hour ago is not
    /// something the human still has on screen.
    #[test]
    fn slash_goal_clear_echoes_the_condition() {
        let mut s = State::test_default();
        type_and_submit(&mut s, "/goal ship it");
        let cmds = type_and_submit(&mut s, "/goal clear");
        assert_eq!(cmds, vec![Cmd::SetGoal(None)]);
        assert!(
            last_notice(&s).contains("goal cleared: ship it"),
            "{}",
            last_notice(&s)
        );
    }

    /// Bare `/goal` is the status surface: spend from the event, the
    /// evaluator's last words on their own line, the stall named, and — with
    /// nothing active — the goal that resolved this session.
    #[test]
    fn slash_goal_bare_shows_spend_reason_and_the_resolved_summary() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type": "goal_changed", "goal": "tests pass"}),
        );
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "not_yet", "reason": "no commit yet",
                   "turns": 4, "usage": {"input_tokens": 41_214, "output_tokens": 3_100}}),
        );
        s.goal_ticks = 12 * 60 * crate::anim::TICK_HZ;
        type_and_submit(&mut s, "/goal");
        let lines = tail_notices(&s, 2);
        assert!(
            lines[0].contains("◎ goal active · 12m · 4 turn(s) · 41.2k in / 3.1k out — tests pass"),
            "{lines:?}"
        );
        assert!(lines[1].contains("last check: no commit yet"), "{lines:?}");

        // A stall keeps the goal, and the line says it is resting.
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "stalled",
                   "reason": "no tool ran in the last 8 goal turns", "turns": 8,
                   "usage": {"input_tokens": 80_000, "output_tokens": 6_000}}),
        );
        type_and_submit(&mut s, "/goal");
        let lines = tail_notices(&s, 2);
        assert!(
            lines[0].contains("◎ goal armed (paused after idle turns) · 12m"),
            "{lines:?}"
        );

        // Resolution: the clear lands first and takes the condition with it,
        // so the verdict has to be told what it resolved.
        upd(&mut s, json!({"type": "goal_changed", "goal": null}));
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "met", "reason": "green",
                   "turns": 9, "usage": {"input_tokens": 90_000, "output_tokens": 7_000}}),
        );
        type_and_submit(&mut s, "/goal");
        assert!(
            last_notice(&s).contains("◎ last goal achieved after 9 turn(s) — tests pass"),
            "{}",
            last_notice(&s)
        );
    }

    #[test]
    fn slash_goal_bare_reports_and_overlong_shows_usage() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/goal");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(
            last_notice(&s).contains("no goal set"),
            "{}",
            last_notice(&s)
        );

        type_and_submit(&mut s, "/goal ship it");
        s.goal_ticks = 2 * 60 * crate::anim::TICK_HZ;
        s.goal_turns = 3;
        type_and_submit(&mut s, "/goal");
        let text = last_notice(&s);
        assert!(
            text.contains("◎ goal active · 2m · 3 turn(s) · 0 in / 0 out — ship it"),
            "{text}"
        );

        let cmds = slash(&mut s, &format!("goal {}", "x".repeat(4001)));
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(last_notice(&s).contains("usage"), "{}", last_notice(&s));
        assert_eq!(
            s.goal.as_deref(),
            Some("ship it"),
            "a rejected set must not clobber the active goal"
        );
    }

    /// Seed-then-correct like `mode_changed`: another surface's set resets
    /// the counters, a confirmation of this surface's own optimistic update
    /// leaves them alone, and the engine's own clear (a met verdict) lands.
    #[test]
    fn goal_changed_seeds_corrects_and_resets_counters() {
        let mut s = State::test_default();
        s.goal_ticks = 99;
        s.goal_turns = 4;
        upd(
            &mut s,
            json!({"type": "goal_changed", "goal": "tests pass"}),
        );
        assert_eq!(s.goal.as_deref(), Some("tests pass"));
        assert_eq!((s.goal_ticks, s.goal_turns), (0, 0));
        s.goal_ticks = 7;
        upd(
            &mut s,
            json!({"type": "goal_changed", "goal": "tests pass"}),
        );
        assert_eq!(s.goal_ticks, 7, "a confirmation must not reset the clock");
        upd(&mut s, json!({"type": "goal_changed", "goal": null}));
        assert_eq!(s.goal, None);
    }

    #[test]
    fn goal_verdict_updates_turns_and_leaves_a_notice() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "not_yet",
                   "reason": "no commit yet", "turns": 2}),
        );
        assert_eq!(s.goal_turns, 2);
        assert!(
            last_notice(&s).contains("(turn 2): not yet — no commit yet"),
            "{}",
            last_notice(&s)
        );
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "met", "reason": "done", "turns": 3}),
        );
        assert!(
            last_notice(&s).contains("achieved after 3 turn(s)"),
            "{}",
            last_notice(&s)
        );
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "eval_failed", "reason": "", "turns": 4}),
        );
        assert!(
            last_notice(&s).contains("stays active"),
            "{}",
            last_notice(&s)
        );
    }

    /// A `stalled` verdict names what to do next and leaves the goal alone:
    /// the clear, when it comes, arrives as `goal_changed` — never inferred
    /// from a verdict tag (0051 T1).
    #[test]
    fn goal_verdict_stalled_and_error_notices_name_the_next_step() {
        let mut s = State::test_default();
        upd(
            &mut s,
            json!({"type": "goal_changed", "goal": "tests pass"}),
        );
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "stalled",
                   "reason": "no tool ran in the last 8 goal turns", "turns": 8}),
        );
        let text = last_notice(&s);
        assert!(
            text.contains("◎ goal paused after 8 turn(s) without a tool call"),
            "{text}"
        );
        assert!(text.contains("your next prompt re-arms it"), "{text}");
        assert_eq!(
            s.goal.as_deref(),
            Some("tests pass"),
            "a stall must not clear the goal locally"
        );

        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "error",
                   "reason": "authentication failed: revoked", "turns": 2}),
        );
        let text = last_notice(&s);
        assert!(
            text.contains("◎ goal cleared after an unrecoverable error"),
            "{text}"
        );
        assert!(text.contains("authentication failed: revoked"), "{text}");
        assert!(text.contains("run /goal again to continue"), "{text}");
    }

    /// Durable session state, like `mode_changed`: a detached turn's goal
    /// updates still land — the engine may resolve the goal mid-detach.
    #[test]
    fn goal_updates_survive_a_detached_turn() {
        let mut s = State::test_default();
        s.detached_turns = 1;
        upd(
            &mut s,
            json!({"type": "goal_changed", "goal": "tests pass"}),
        );
        assert_eq!(s.goal.as_deref(), Some("tests pass"));
        upd(
            &mut s,
            json!({"type": "goal_verdict", "verdict": "not_yet", "reason": "r", "turns": 2}),
        );
        assert_eq!(s.goal_turns, 2);
    }

    /// The goal's clock advances only while a goal is set (and, like every
    /// tick, only arrives while a turn runs).
    #[test]
    fn the_goal_clock_advances_only_while_a_goal_is_set() {
        let mut s = State::test_default();
        update(&mut s, Msg::Tick);
        assert_eq!(s.goal_ticks, 0);
        s.goal = Some("g".into());
        update(&mut s, Msg::Tick);
        assert_eq!(s.goal_ticks, 1);
    }

    #[test]
    fn status_line_shows_an_active_goal() {
        let mut s = State::test_default();
        type_and_submit(&mut s, "/goal ship it");
        type_and_submit(&mut s, "/status");
        assert!(
            last_notice(&s).contains("◎ goal active"),
            "{}",
            last_notice(&s)
        );
    }

    #[test]
    fn unknown_slash_command_never_reaches_the_model() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/frobnicate now");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("/frobnicate"))
        );
    }

    fn slash(s: &mut State, rest: &str) -> Vec<Cmd> {
        slash_command(s, rest, paste::PromptPayload::text_only(format!("/{rest}")))
    }

    fn last_notice(s: &State) -> String {
        match s.transcript.last() {
            Some(TranscriptItem::Notice { text }) => text.as_str().to_string(),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    /// The last `n` notices, oldest first — for the commands that answer on
    /// more than one line.
    fn tail_notices(s: &State, n: usize) -> Vec<String> {
        let mut found: Vec<String> = s
            .transcript
            .iter()
            .rev()
            .filter_map(|i| match i {
                TranscriptItem::Notice { text } => Some(text.as_str().to_string()),
                _ => None,
            })
            .take(n)
            .collect();
        found.reverse();
        found
    }

    /// 0044: `/<recipe>` desugars to a prompt naming the saved workflow; the
    /// model makes the tool call. Arguments ride verbatim; none means no
    /// trailing clause. Precedence is builtin > skill > workflow.
    #[test]
    fn a_saved_workflow_slash_desugars_to_a_prompt_below_builtins_and_skills() {
        let mut s = State::test_default();
        s.set_skills(vec![("review".into(), String::new())]);
        s.set_workflows(vec![
            ("review-changes".into(), String::new()),
            ("review".into(), String::new()),
            ("context".into(), String::new()),
        ]);
        let cmds = slash(&mut s, "review-changes crates/hotl-workflow");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert_eq!(
            p.text,
            "Run the saved workflow `review-changes` with the `workflow` tool (name = \"review-changes\"). Arguments: crates/hotl-workflow"
        );
        let mut s2 = State::test_default();
        s2.set_workflows(vec![("review-changes".into(), String::new())]);
        let cmds = slash(&mut s2, "review-changes");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("{cmds:?}")
        };
        assert!(
            p.text.ends_with("(name = \"review-changes\")."),
            "{}",
            p.text
        );

        // A skill wins over a same-named recipe; a builtin over both.
        s.phase = Phase::Idle;
        let cmds = slash(&mut s, "review");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("{cmds:?}")
        };
        assert!(p.text.starts_with("Load the skill `review`"), "{}", p.text);
        assert_eq!(slash(&mut s, "context"), vec![Cmd::RequestContext]);
        assert_eq!(slash(&mut s, "workflows"), vec![Cmd::RequestWorkflows]);
    }

    #[test]
    fn a_workflows_report_becomes_an_item_and_malformed_or_empty_ones_notice() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::Update(json!({"type": "workflows_report", "runs": [{
                "id": "r1", "name": "review-changes", "status": "running", "tokens": 41_200, "elapsed_ms": 192_000,
                "phases": [
                    {"title": "Review", "agents": [{"status": "done"}, {"status": "done"}]},
                    {"title": "Verify", "agents": [{"status": "done"}, {"status": "failed"}, {"status": "running"}]},
                    {"title": "Find", "agents": []}
                ]
            }]})),
        );
        let Some(TranscriptItem::WorkflowsReport(runs)) = s.transcript.last() else {
            panic!("{:?}", s.transcript.last());
        };
        assert_eq!(runs.len(), 1);
        assert_eq!(
            (
                runs[0].name.as_str(),
                runs[0].status.as_str(),
                runs[0].tokens,
                runs[0].elapsed_ms
            ),
            ("review-changes", "running", 41_200, 192_000)
        );
        assert_eq!(
            runs[0].phases,
            vec![
                WorkflowPhase {
                    title: "Review".into(),
                    settled: 2,
                    started: 2,
                    failed: 0
                },
                WorkflowPhase {
                    title: "Verify".into(),
                    settled: 2,
                    started: 3,
                    failed: 1
                },
                WorkflowPhase {
                    title: "Find".into(),
                    settled: 0,
                    started: 0,
                    failed: 0
                },
            ]
        );
        update(&mut s, Msg::Update(json!({"type": "workflows_report"})));
        assert!(last_notice(&s).contains("could not read the workflows report"));
        update(
            &mut s,
            Msg::Update(json!({"type": "workflows_report", "runs": []})),
        );
        assert_eq!(last_notice(&s), "no workflows have run in this process");
    }

    #[test]
    fn rename_uses_the_shared_normalizer() {
        let mut s = State::test_default();
        slash(&mut s, "rename   spaced name  ");
        assert_eq!(s.session_name.as_deref(), Some("spaced name"));
        slash(&mut s, &format!("rename {}", "x".repeat(65)));
        assert!(last_notice(&s).contains("1–64"));
        // The one source of truth, not a copy of its rules.
        assert_eq!(
            hotl_types::normalize_session_name("  ok  ").as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn the_new_builtins_dispatch_and_are_completable() {
        // `complete::BUILTINS` and `slash_command`'s arms must agree — the pin
        // test below covers the set; this covers each one's effect.
        let mut s = State::test_default();
        assert!(slash(&mut s, "help").is_empty());
        assert!(s.help_open);

        let mut s = State::test_default();
        s.mode = "bypass".into();
        s.plan = true;
        s.model = "claude-opus-4-8".into();
        slash(&mut s, "status");
        let text = last_notice(&s);
        assert!(
            text.contains("bypass") && text.contains("plan") && text.contains("claude-opus-4-8"),
            "{text}"
        );

        let mut s = State::test_default();
        s.session_usage.input = 1_500;
        slash(&mut s, "cost");
        assert!(last_notice(&s).contains("1.5k"), "{}", last_notice(&s));

        let mut s = State::test_default();
        s.transcript = vec![TranscriptItem::Notice { text: "old".into() }];
        slash(&mut s, "clear");
        assert_eq!(
            s.transcript.len(),
            1,
            "the clear notice replaces the transcript"
        );
        assert!(
            last_notice(&s).contains("view"),
            "must not imply the log was cleared: {}",
            last_notice(&s)
        );

        let mut s = State::test_default();
        assert_eq!(slash(&mut s, "quit"), vec![Cmd::Quit]);
    }

    #[test]
    fn unknown_slash_still_reaches_no_model() {
        let mut s = State::test_default();
        let cmds = slash(&mut s, "compact");
        assert!(
            cmds.is_empty(),
            "/compact is deferred; it must not become a prompt"
        );
        assert!(last_notice(&s).contains("unknown command"));
    }

    /// Finding 4 (minor): `complete::BUILTINS` and `slash_command`'s match
    /// arms are two unpinned sources of truth for the same list — nothing
    /// enforces that a name in one exists in the other. This pins them: a
    /// name only `slash_command` recognizes just doesn't show up in the
    /// popup (silently missable), but a name only `BUILTINS` advertises is
    /// worse — the popup completes it and then Enter dispatches to the
    /// `unknown command: /<name>` notice. Add a 4th entry to `BUILTINS`
    /// without a matching arm and this test catches it.
    #[test]
    fn every_builtin_name_dispatches_to_something_other_than_unknown_command() {
        for cmd in complete::builtins() {
            let mut s = State::test_default();
            type_and_submit(&mut s, &format!("/{}", cmd.name));
            let unknown = format!("unknown command: /{}", cmd.name);
            assert!(
                !matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if *text == unknown),
                "BUILTINS lists `{}` but slash_command has no matching dispatch arm for it",
                cmd.name
            );
        }
    }

    // --- /context (plan 0028) ------------------------------------------

    /// The report the engine broadcasts, as JSON. Rows not named here are
    /// absent, which is a shape the client must tolerate even though the real
    /// engine always emits all twelve.
    fn context_report(window: u64, rows: &[(&str, u64)]) -> Value {
        json!({
            "type": "context_report",
            "window": window,
            "rows": rows.iter().map(|(k, n)| json!({"kind": k, "tokens": n}))
                .collect::<Vec<_>>(),
        })
    }

    fn last_report(s: &State) -> ContextReport {
        match s.transcript.last() {
            Some(TranscriptItem::Report(r)) => r.clone(),
            other => panic!("expected a report, got {other:?}"),
        }
    }

    #[test]
    fn slash_context_asks_the_engine() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/context");
        assert_eq!(cmds, vec![Cmd::RequestContext]);
        assert!(
            s.transcript.is_empty(),
            "the report is the broadcast's job, not the command's"
        );
    }

    /// Contrast `slash_reload_is_refused_while_a_turn_runs`: a reload replaces
    /// the session, a context read touches nothing.
    #[test]
    fn slash_context_works_mid_turn() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        assert_eq!(slash(&mut s, "context"), vec![Cmd::RequestContext]);
        assert_eq!(s.phase, Phase::Sampling { ticks: 0 });
    }

    #[test]
    fn a_context_report_becomes_a_report_item() {
        let mut s = State::test_default();
        s.model = "claude-opus-5".into();
        update(
            &mut s,
            Msg::Update(context_report(
                1_000_000,
                &[
                    ("system_prompt", 5_312),
                    ("messages", 102_438),
                    ("tool_results", 138_800),
                ],
            )),
        );
        let r = last_report(&s);
        assert_eq!(r.model, "claude-opus-5");
        assert_eq!(r.window, 1_000_000);
        assert_eq!(r.estimated, 5_312 + 102_438 + 138_800);
        assert_eq!(
            r.rows,
            vec![
                (ContextKind::SystemPrompt, 5_312),
                (ContextKind::Messages, 102_438),
                (ContextKind::ToolResults, 138_800),
            ]
        );
        assert_eq!(r.free, 1_000_000 - r.estimated);
    }

    #[test]
    fn zero_rows_are_dropped_from_the_report() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::Update(context_report(
                200_000,
                &[
                    ("system_prompt", 100),
                    ("memory", 0),
                    ("todos", 0),
                    ("images", 0),
                ],
            )),
        );
        let r = last_report(&s);
        assert_eq!(r.rows, vec![(ContextKind::SystemPrompt, 100)]);
        assert_eq!(r.estimated, 100, "the zeros still summed, they just add 0");
    }

    /// Rows arrive out of order from a hand-rolled client; display order is
    /// this client's business, so it sorts rather than trusts.
    #[test]
    fn report_rows_are_sorted_into_canonical_order() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::Update(context_report(
                200_000,
                &[("messages", 7), ("system_prompt", 3), ("memory", 5)],
            )),
        );
        assert_eq!(
            last_report(&s).rows,
            vec![
                (ContextKind::SystemPrompt, 3),
                (ContextKind::Memory, 5),
                (ContextKind::Messages, 7),
            ]
        );
    }

    #[test]
    fn free_space_never_overstates_the_room() {
        let mut s = State::test_default();
        s.live_context = Some(150_000);
        update(
            &mut s,
            Msg::Update(context_report(200_000, &[("messages", 10_000)])),
        );
        let r = last_report(&s);
        assert_eq!(r.reported, Some(150_000));
        assert_eq!(r.estimated, 10_000);
        assert_eq!(
            r.free, 50_000,
            "free space comes off the LARGER total, never the smaller"
        );
    }

    #[test]
    fn a_zero_window_does_not_divide() {
        let mut s = State::test_default();
        update(&mut s, Msg::Update(context_report(0, &[("messages", 10)])));
        let r = last_report(&s);
        assert_eq!(r.window, 0);
        assert_eq!(r.free, 0);
    }

    /// A newer engine's row must still be counted. Dropping it would report a
    /// smaller context than exists — the one direction this codebase refuses.
    #[test]
    fn an_unknown_row_kind_is_kept() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::Update(context_report(
                200_000,
                &[("messages", 10), ("future_thing", 99)],
            )),
        );
        let r = last_report(&s);
        assert_eq!(r.estimated, 109);
        assert!(r.rows.contains(&(ContextKind::Unknown, 99)));
    }

    /// 0044: the workflow roster joins completion after the skills; a recipe
    /// named like a built-in or a skill is hidden, since dispatch
    /// (builtin > skill > workflow) could never reach it.
    #[test]
    fn set_workflows_extends_completion_below_skills_and_hides_shadowed_names() {
        let mut s = State::test_default();
        s.set_skills(vec![("review".into(), "a skill".into())]);
        s.set_workflows(vec![
            ("review-changes".into(), "a recipe".into()),
            ("review".into(), "shadowed by the skill".into()),
            ("help".into(), "shadowed by a builtin".into()),
        ]);
        assert_eq!(s.workflows, vec!["review-changes", "review", "help"]);
        let names: Vec<&str> = s.commands.iter().map(|c| c.name.as_str()).collect();
        let review_at = names.iter().position(|n| *n == "review").unwrap();
        let recipe_at = names.iter().position(|n| *n == "review-changes").unwrap();
        assert!(review_at < recipe_at, "skills before workflows: {names:?}");
        assert_eq!(names.iter().filter(|n| **n == "review").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "help").count(), 1);
        assert!(!s.commands[recipe_at].builtin);
        // Re-seeding skills keeps the workflow rows (a reload sends both).
        s.set_skills(Vec::new());
        assert!(s.commands.iter().any(|c| c.name == "review-changes"));
        assert!(
            s.commands.iter().any(|c| c.name == "review"),
            "no longer shadowed"
        );
    }

    #[test]
    fn a_malformed_context_report_notices_instead_of_panicking() {
        let mut s = State::test_default();
        update(&mut s, Msg::Update(json!({"type": "context_report"})));
        assert!(
            last_notice(&s).contains("could not read the context report"),
            "an empty table would read as an empty context"
        );
    }

    #[test]
    fn the_report_carries_the_last_turns_reported_total() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::PromptResult {
                outcome_kind: "done".into(),
                outcome_text: None,
                usage: json!({
                    "input_tokens": 1_000,
                    "cache_read_input_tokens": 40_000,
                    "cache_creation_input_tokens": 300,
                }),
                finished_at: None,
            },
        );
        assert_eq!(s.live_context, Some(41_300));
        update(
            &mut s,
            Msg::Update(context_report(200_000, &[("messages", 10)])),
        );
        assert_eq!(last_report(&s).reported, Some(41_300));
    }

    #[test]
    fn slash_reload_emits_the_settings_half_before_the_wire_half() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/reload");
        assert_eq!(
            cmds,
            vec![Cmd::ReloadSettings, Cmd::ReloadConfig],
            "the theme must flip before the engine rebuild is awaited"
        );
        assert_eq!(s.phase, Phase::Idle, "a reload never starts a turn");
        assert!(last_notice(&s).contains("reloading"));
    }

    /// A rebuild replaces the session, taking the in-flight turn's reply with
    /// it. Abandoning a turn stays the esc ladder's job.
    #[test]
    fn slash_reload_is_refused_while_a_turn_runs() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        let cmds = slash(&mut s, "reload");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert_eq!(
            s.phase,
            Phase::Sampling { ticks: 0 },
            "the turn is untouched"
        );
        assert!(last_notice(&s).contains("idle session"));
    }

    #[test]
    fn config_reloaded_reseeds_model_mode_window_and_the_skill_roster() {
        let mut s = State::test_default();
        s.set_skills(vec![("old".into(), "gone after the reload".into())]);
        let cmds = update(
            &mut s,
            Msg::Update(json!({
                "type": "config_reloaded",
                "model": "anthropic/claude-opus-5",
                "mode": "plan",
                "context_window": 900_000,
                "skills": [{"name": "run", "description": "launch the app"}],
                "warnings": ["[skills.marketplaces] `bad name` — entry skipped"],
            })),
        );
        assert!(cmds.is_empty(), "a reload notification commands nothing");
        assert_eq!(s.model, "anthropic/claude-opus-5");
        assert_eq!(s.mode, "plan");
        assert_eq!(s.context_window, 900_000);
        assert_eq!(s.skills, vec!["run".to_string()], "the old roster is gone");
        assert!(
            s.commands.iter().any(|c| c.name == "run" && !c.builtin),
            "the completion table follows the roster"
        );
        assert!(
            s.commands.iter().any(|c| c.name == "reload" && c.builtin),
            "built-ins survive a roster swap"
        );
        assert!(!s.commands.iter().any(|c| c.name == "old"));
        let notices: Vec<&str> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Notice { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|t| t.contains("entry skipped")),
            "server-side warnings reach the transcript: {notices:?}"
        );
        assert!(
            notices.iter().any(|t| t.contains("config reloaded")),
            "{notices:?}"
        );
    }

    /// A zero (or absent) window is an older server saying nothing, not a
    /// claim that the model has no context — the gauge must keep dividing by
    /// what it already knows.
    #[test]
    fn config_reloaded_keeps_the_known_window_when_the_server_reports_none() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        update(
            &mut s,
            Msg::Update(json!({"type": "config_reloaded", "model": "m", "mode": "ask"})),
        );
        assert_eq!(s.context_window, 200_000);
    }

    #[test]
    fn a_failed_reload_says_the_previous_config_is_still_live() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::Update(json!({
                "type": "config_reload_failed",
                "reason": "TOML parse error at line 3",
            })),
        );
        let text = last_notice(&s);
        assert!(text.contains("TOML parse error"), "{text}");
        assert!(text.contains("still live"), "{text}");
    }

    /// The detached-turn filter swallows everything a dead turn emits. A
    /// reload is not the dead turn talking — it replaced the session outright,
    /// and a swallowed `config_reloaded` would leave the badge, the model and
    /// the roster describing an engine that is gone.
    #[test]
    fn a_detached_turn_does_not_swallow_the_reload_notifications() {
        let mut s = State::test_default();
        s.detached_turns = 1;
        update(
            &mut s,
            Msg::Update(
                json!({"type": "config_reloaded", "model": "m2", "mode": "auto",
                               "skills": [], "context_window": 123_456}),
            ),
        );
        assert_eq!(s.model, "m2");
        assert_eq!(s.mode, "auto");
        assert_eq!(s.context_window, 123_456);

        // …while an ordinary update from the dead turn still is swallowed.
        let before = s.transcript.len();
        update(
            &mut s,
            Msg::Update(json!({"type": "assistant_delta", "text": "ghost"})),
        );
        assert_eq!(s.transcript.len(), before);
    }

    #[test]
    fn settings_reloaded_applies_vim_mode_and_density_and_shows_warnings() {
        let mut s = State::test_default();
        assert!(s.vim_mode, "test default is vim on");
        update(
            &mut s,
            Msg::SettingsReloaded {
                vim_mode: false,
                density: hotl_theme::Density::Compact,
                measure: 80,
                warnings: vec!["unknown density 'wat' — using comfortable".into()],
            },
        );
        assert!(!s.vim_mode);
        assert_eq!(s.density, hotl_theme::Density::Compact);
        assert_eq!(s.measure, 80);
        assert!(last_notice(&s).contains("unknown density"));
        // The editor holds its own copy; a stale one would leave modal keys
        // live after vim mode was turned off.
        s.editor.set_text("abc");
        assert_eq!(s.editor.text(), "abc");
    }

    #[test]
    fn a_known_skill_name_after_slash_prompts_for_that_skill() {
        let mut s = State::test_default();
        s.skills = vec!["brainstorming".into(), "superpowers:brainstorming".into()];

        let cmds = type_and_submit(&mut s, "/brainstorming redesign the skill system");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(p.text.contains("`brainstorming`"), "{}", p.text);
        assert!(
            p.text.contains("ARGUMENTS: redesign the skill system"),
            "the argument rides along: {}",
            p.text
        );
        assert_eq!(s.phase, Phase::Sampling { ticks: 0 });

        // Qualified names resolve too, and take no argument fine.
        let mut s = State::test_default();
        s.skills = vec!["superpowers:brainstorming".into()];
        let cmds = type_and_submit(&mut s, "/superpowers:brainstorming");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(!p.text.contains("ARGUMENTS"), "{}", p.text);
    }

    #[test]
    fn a_skill_invocation_carries_its_attached_images() {
        let mut s = State::new(false, "m".into());
        s.skills = vec!["brainstorming".into()];
        type_str(&mut s, "/brainstorming ");
        update(&mut s, Msg::Paste("/tmp/mockup.png".into()));
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find_map(|c| match c {
            Cmd::SendPrompt(p) => Some(Cmd::SendPrompt(p.clone())),
            _ => None,
        }) else {
            panic!("a skill desugars to a prompt: {cmds:?}");
        };
        assert_eq!(p.images.len(), 1, "the mockup must ride along");
        assert_eq!(p.images[0].path, "/tmp/mockup.png");
        assert!(
            p.text.contains("[Image #1]"),
            "and its marker stays inline: {}",
            p.text
        );
        assert!(
            p.text.starts_with("Load the skill `brainstorming`"),
            "{}",
            p.text
        );
    }

    #[test]
    fn a_builtin_wins_over_a_skill_of_the_same_name() {
        let mut s = State::test_default();
        s.skills = vec!["rename".into()];
        let cmds = type_and_submit(&mut s, "/rename fix-auth");
        assert!(
            matches!(&cmds[..], [Cmd::Rename(n), _] if n == "fix-auth"),
            "got {cmds:?}"
        );
    }

    #[test]
    fn a_prompt_turn_persists_to_history_the_literal_text() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "fix the bug");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::AppendHistory(t) if t == "fix the bug")),
            "got {cmds:?}"
        );
    }

    #[test]
    fn slash_commands_and_steers_do_not_persist_to_disk_history() {
        // A slash command never starts a turn → nothing to persist.
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/rename foo");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::AppendHistory(_))));

        // A skill invocation desugars to a prompt, but the *literal* was a
        // slash command — it is not written to the on-disk history either.
        let mut s = State::test_default();
        s.skills = vec!["brainstorming".into()];
        let cmds = type_and_submit(&mut s, "/brainstorming redesign");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SendPrompt(_))));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::AppendHistory(_))));

        // A steer (typed mid-turn) uses SendSteer, not SendPrompt → not persisted.
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        let cmds = type_and_submit(&mut s, "wait, use X");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SendSteer(_))));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::AppendHistory(_))));
    }

    #[test]
    fn named_session_titles_carry_the_name() {
        let mut s = State::test_default();
        s.session_name = Some("fix-auth".into());
        let cmds = type_and_submit(&mut s, "hello");
        assert!(
            matches!(&cmds[..], [Cmd::AppendHistory(_), Cmd::SendPrompt(_), Cmd::SetTitle(t)] if t == "hotl · fix-auth — working"),
            "got {cmds:?}"
        );
    }

    /// Press, drag to `(col, row)`, release — one whole mouse gesture.
    fn drag_to(s: &mut State, col: u16, row: u16) -> Vec<Cmd> {
        update(s, Msg::SelectStart { col: 2, row: 1 });
        update(s, Msg::SelectExtend { col, row });
        update(s, Msg::SelectEnd)
    }

    #[test]
    fn a_finished_drag_asks_the_runtime_to_copy() {
        let mut s = State::test_default();
        let cmds = drag_to(&mut s, 9, 3);
        assert!(
            matches!(&cmds[..], [Cmd::CopySelection(sel)] if sel.anchor == (2, 1) && sel.head == (9, 3)),
            "got {cmds:?}"
        );
    }

    #[test]
    fn a_finished_drag_leaves_the_highlight_up() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        assert!(
            s.selection.is_some(),
            "the copied region stays visible until the next action"
        );
    }

    #[test]
    fn a_click_that_never_dragged_copies_nothing() {
        let mut s = State::test_default();
        update(&mut s, Msg::SelectStart { col: 2, row: 1 });
        let cmds = update(&mut s, Msg::SelectEnd);
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(s.selection.is_none(), "a bare click leaves nothing painted");
    }

    #[test]
    fn a_keypress_clears_the_selection_and_the_notice() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        update(&mut s, Msg::Copied { lines: 3 });
        press(&mut s, KeyCode::Char('x'));
        assert!(s.selection.is_none());
        assert!(s.copy_notice.is_none());
    }

    #[test]
    fn streaming_updates_do_not_clear_a_live_drag() {
        let mut s = State::test_default();
        update(&mut s, Msg::SelectStart { col: 2, row: 1 });
        upd(
            &mut s,
            json!({"type": "text_delta", "text": "still writing"}),
        );
        update(&mut s, Msg::SelectExtend { col: 9, row: 3 });
        let cmds = update(&mut s, Msg::SelectEnd);
        assert!(
            matches!(&cmds[..], [Cmd::CopySelection(_)]),
            "a drag must survive arriving tokens, got {cmds:?}"
        );
    }

    #[test]
    fn a_copy_of_nothing_raises_no_notice() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        update(&mut s, Msg::Copied { lines: 0 });
        assert_eq!(s.copy_notice, None);
    }

    #[test]
    fn a_copy_records_the_line_count_for_the_hint() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        update(&mut s, Msg::Copied { lines: 3 });
        assert_eq!(s.copy_notice, Some(3));
    }
}
