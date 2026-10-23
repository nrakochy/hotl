//! The single `EngineEvent` → JSON renderer.
//!
//! There used to be three (`agent.rs::render_json`, `acp.rs::update_payload`,
//! `attach.rs::render_update`) and they had already drifted: the headless one
//! emitted a Rust `Debug` string for `outcome` while ACP emitted a structured
//! tag, and attach handled 4 of 13 variants (evaluation §7). One renderer, one
//! shape, one place to add a variant.
//!
//! Deliberately self-contained — it depends on `hotl-engine` and `serde_json`
//! and nothing else in this crate — so `tests/json_stream_schema.rs` can pull
//! it in by path the way the other integration tests reach `acp.rs`
//! (`crates/hotl` is a binary crate with no lib target).
//!
//! INVARIANT: every `EngineEvent` variant produces a `type`-tagged object, and
//! no payload is a `Debug` rendering. Enforced by
//! `tests/json_stream_schema.rs::every_frame_is_tagged_and_versioned`.

use hotl_engine::{EngineEvent, Outcome};
use hotl_provider::catalog;
use hotl_types::{Todo, TokenUsage};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Value};

/// Stable schema version of the `-p --json` event stream (an MD Tier-1
/// contract; bump only on a breaking change to a frame's shape).
///
/// **v2** — `turn_done.outcome` changed from a Rust `Debug` string
/// (`"Done { text: \"…\" }"`) to the tagged object `{"kind": …, …}`, and
/// `thinking_delta` gained `text`. Pinned by `tests/json_stream_schema.rs`.
///
/// Image-bearing user prompts (`Item::User { images }`) were assessed and
/// need no bump: prompts are inputs, never stream frames — no `EngineEvent`
/// carries an `Item`, so nothing in this vocabulary changed.
pub const JSON_STREAM_SCHEMA_VERSION: u32 = 2;

/// One `-p --json` frame (and, via `update_frame`, every ACP update body that
/// mirrors one). Field order is the wire order: under `serde_json`'s
/// `preserve_order` (on in the default build) a struct serializes in
/// declaration order, so each variant lists its keys as the old literals did
/// (0076 D2). Omitted-not-null fields are `Option` + `skip_serializing_if`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// Streamed answer text.
    TextDelta { text: String },
    /// Streamed reasoning text.
    ThinkingDelta { text: String },
    /// A tool call started; `id` is the provider's `tool_use` id.
    ToolStart {
        id: String,
        name: String,
        summary: String,
    },
    /// A tool call finished; `lines`/`bytes` count the result the model got.
    ToolDone {
        id: String,
        name: String,
        ok: bool,
        lines: u64,
        bytes: u64,
    },
    /// The goal evaluator is judging the turn just finished.
    GoalEvaluating { turn: u32 },
    /// A call waits for a concurrency permit behind `ahead` others.
    ToolQueued {
        id: String,
        name: String,
        summary: String,
        ahead: usize,
    },
    /// A fold of `items` history items is starting.
    Compacting { items: usize },
    /// Liveness of a running tool: the tail of its output so far.
    ToolProgress {
        id: String,
        name: String,
        tail: String,
        lines: u64,
        bytes: u64,
    },
    /// The model is writing tool-call input: `calls` calls, `bytes` so far.
    ToolInputProgress { calls: u32, bytes: u64 },
    /// A call was denied before it ran.
    ToolDenied { id: String, name: String },
    /// A call ran without an ask because an allow rule matched.
    ToolAutoAllowed {
        id: String,
        name: String,
        rule: String,
    },
    /// A protected-class call ran or was refused under bypass, with a notice.
    ToolFlagged {
        id: String,
        name: String,
        summary: String,
        why: String,
        denied: bool,
    },
    /// A sub-agent's tool activity; `ok` only on the done phase.
    ChildTool {
        parent_id: String,
        id: String,
        name: String,
        summary: String,
        phase: ChildPhase,
        #[serde(skip_serializing_if = "Option::is_none")]
        ok: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
    },
    /// A sub-agent's own words.
    ChildText { parent_id: String, text: String },
    /// One turn's delegation summary.
    Delegation {
        subagent_runs: u32,
        duplicate_work_paths: u32,
        false_completions: u32,
        usage: TokenUsage,
        tool_calls: u64,
    },
    /// A cache keep-alive ping was billed.
    CacheWarm {
        usage: TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    /// A request is being retried after `delay_ms`.
    Retrying {
        attempt: u32,
        max: u32,
        reason: String,
        delay_ms: u64,
        scope: RetryScopeTag,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        #[schemars(default)]
        discarded_partial: bool,
    },
    /// The turn moved to a fallback model.
    FallbackModel { model: String },
    /// A prompt was queued behind the running turn.
    PromptQueued,
    /// Session spend crossed `pct` of `max_cost_usd`.
    BudgetNotice {
        pct: u8,
        used_usd: f64,
        cap_usd: f64,
    },
    /// A fold finished.
    Compacted {
        degraded: bool,
        usage: TokenUsage,
        estimate: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    /// A requested fold had nothing to fold.
    FoldSkipped { reason: String },
    /// The session's model changed; `context_tokens` is the cold-cache rebuild.
    ModelChanged {
        model: String,
        previous: String,
        context_tokens: u64,
        context_window: u64,
    },
    /// The context ladder cleared `count` old tool results.
    Cleared { count: usize },
    /// The todo list changed.
    TodosChanged { items: Vec<Todo> },
    /// The model presented a plan.
    PlanPresented {
        summary: String,
        nodes: Vec<Todo>,
        path: Option<String>,
    },
    /// The goal was set (or cleared: `null`).
    GoalChanged { goal: Option<String> },
    /// A memory note was written.
    MemoryChanged {
        note: String,
        date: String,
        bytes: usize,
        path: String,
    },
    /// A background job started.
    JobStarted {
        id: Option<String>,
        job: String,
        pid: u32,
        command: String,
        detached: bool,
    },
    /// A background job ended.
    JobDone {
        id: Option<String>,
        job: String,
        rc: String,
        secs: u64,
        stopped_by: Option<String>,
    },
    /// The session got a generated title (omitted when none was usable).
    SessionTitled {
        usage: TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// A `/btw` side question was answered.
    AsideResult {
        question: String,
        text: String,
        usage: TokenUsage,
    },
    /// A `/btw` side question failed.
    AsideFailed { question: String, reason: String },
    /// The goal evaluator's verdict; `usage` is the goal loop's spend.
    GoalVerdict {
        verdict: GoalVerdictTag,
        reason: String,
        turns: u32,
        usage: TokenUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        evidence: Option<Vec<String>>,
    },
    /// The loop ledger's per-turn profile.
    LedgerReport {
        sample_count: usize,
        overhead_p50_ns: u64,
        overhead_p99_ns: u64,
        max_rss_bytes: u64,
        phase_deltas: Vec<hotl_engine::PhaseDeltaSummary>,
    },
    /// The turn ended (`-p --json` only; ACP carries it as the prompt result).
    TurnDone {
        outcome: OutcomeFrame,
        usage: UsageFrame,
        #[serde(skip_serializing_if = "Option::is_none")]
        mispredictions: Option<u32>,
    },
    /// Headless has no human: an ask was denied.
    AskDenied { summary: String },
    /// Headless has no human: a question went unanswered.
    QuestionNoHuman {
        header: String,
        prompt: String,
        options: Vec<hotl_types::QuestionOption>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChildPhase {
    Start,
    Done,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetryScopeTag {
    Sample,
    Summarize,
    GoalEval,
    Title,
    Aside,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalVerdictTag {
    NotYet,
    Met,
    Impossible,
    EvalFailed,
    Stalled,
    #[serde(rename = "error")]
    Errored,
}

/// A turn outcome, tagged by `kind`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeFrame {
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
    /// `message` names the knob, for clients that show an outcome's text.
    DenialSpiral {
        consecutive: u32,
        total: u32,
        message: String,
    },
    Budget {
        budget: String,
        used: f64,
        cap: f64,
        message: String,
    },
    Error {
        message: String,
    },
}

/// `TokenUsage` plus the derived `hit_ratio` and `cost_usd`, each omitted
/// when there is nothing to report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UsageFrame {
    #[serde(flatten)]
    pub usage: TokenUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// One `-p --json` line: a frame plus the stream's schema version. The
/// version is last, as the old renderer appended it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct JsonStreamLine {
    #[serde(flatten)]
    pub frame: Frame,
    /// Always the current `JSON_STREAM_SCHEMA_VERSION`.
    pub schema_version: u32,
}

/// Streamable events, as clients see them. `None` = not a stream frame
/// (`Ask`/`Question` are round-trips; `TurnDone` is a result) — those callers
/// build their own envelope.
pub fn update_frame(event: &EngineEvent) -> Option<Value> {
    Frame::stream(event).map(|f| serde_json::to_value(f).expect("a frame always serializes"))
}

impl Frame {
    /// The stream frame for an event; `None` for the round-trips and the
    /// turn result (see [`json_frame`]).
    pub fn stream(event: &EngineEvent) -> Option<Frame> {
        Some(match event {
            EngineEvent::TextDelta(t) => Frame::TextDelta { text: t.clone() },
            EngineEvent::ThinkingDelta(t) => Frame::ThinkingDelta { text: t.clone() },
            EngineEvent::ToolStart { id, name, summary } => Frame::ToolStart {
                id: id.clone(),
                name: name.clone(),
                summary: summary.clone(),
            },
            EngineEvent::ToolDone {
                id,
                name,
                ok,
                lines,
                bytes,
            } => Frame::ToolDone {
                id: id.clone(),
                name: name.clone(),
                ok: *ok,
                lines: *lines,
                bytes: *bytes,
            },
            EngineEvent::GoalEvaluating { turn } => Frame::GoalEvaluating { turn: *turn },
            EngineEvent::ToolQueued {
                id,
                name,
                summary,
                ahead,
            } => Frame::ToolQueued {
                id: id.clone(),
                name: name.clone(),
                summary: summary.clone(),
                ahead: *ahead,
            },
            EngineEvent::Compacting { items } => Frame::Compacting { items: *items },
            EngineEvent::ToolProgress {
                id,
                name,
                tail,
                lines,
                bytes,
            } => Frame::ToolProgress {
                id: id.clone(),
                name: name.clone(),
                tail: tail.clone(),
                lines: *lines,
                bytes: *bytes,
            },
            EngineEvent::ToolInputProgress { calls, bytes } => Frame::ToolInputProgress {
                calls: *calls,
                bytes: *bytes,
            },
            EngineEvent::ToolDenied { id, name } => Frame::ToolDenied {
                id: id.clone(),
                name: name.clone(),
            },
            EngineEvent::ToolAutoAllowed { id, name, rule } => Frame::ToolAutoAllowed {
                id: id.clone(),
                name: name.clone(),
                rule: rule.clone(),
            },
            EngineEvent::ToolFlagged {
                id,
                name,
                summary,
                why,
                denied,
            } => Frame::ToolFlagged {
                id: id.clone(),
                name: name.clone(),
                summary: summary.clone(),
                why: why.clone(),
                denied: *denied,
            },
            EngineEvent::ChildTool {
                parent_id,
                id,
                name,
                summary,
                ok,
                tokens,
            } => Frame::ChildTool {
                parent_id: parent_id.clone(),
                id: id.clone(),
                name: name.clone(),
                summary: summary.clone(),
                phase: if ok.is_some() {
                    ChildPhase::Done
                } else {
                    ChildPhase::Start
                },
                ok: *ok,
                tokens: *tokens,
            },
            EngineEvent::ChildText { parent_id, text } => Frame::ChildText {
                parent_id: parent_id.clone(),
                text: text.clone(),
            },
            EngineEvent::Delegation {
                subagent_runs,
                duplicate_work_paths,
                false_completions,
                usage,
                tool_calls,
            } => Frame::Delegation {
                subagent_runs: *subagent_runs,
                duplicate_work_paths: *duplicate_work_paths,
                false_completions: *false_completions,
                usage: *usage,
                tool_calls: *tool_calls,
            },
            EngineEvent::CacheWarmed { usage, cost_usd } => Frame::CacheWarm {
                usage: *usage,
                cost_usd: *cost_usd,
            },
            EngineEvent::Retrying {
                attempt,
                max,
                reason,
                delay_ms,
                status,
                request_id,
                scope,
                discarded_partial,
            } => Frame::Retrying {
                attempt: *attempt,
                max: *max,
                reason: reason.clone(),
                delay_ms: *delay_ms,
                scope: retry_scope_tag(*scope),
                status: *status,
                request_id: request_id.clone(),
                discarded_partial: *discarded_partial,
            },
            EngineEvent::FallbackModel { model } => Frame::FallbackModel {
                model: model.clone(),
            },
            EngineEvent::PromptQueued => Frame::PromptQueued,
            EngineEvent::BudgetNotice {
                pct,
                used_usd,
                cap_usd,
            } => Frame::BudgetNotice {
                pct: *pct,
                used_usd: *used_usd,
                cap_usd: *cap_usd,
            },
            EngineEvent::Compacted {
                degraded,
                focus,
                usage,
                estimate,
            } => Frame::Compacted {
                degraded: *degraded,
                usage: *usage,
                estimate: *estimate,
                focus: focus.clone(),
            },
            EngineEvent::FoldSkipped { reason } => Frame::FoldSkipped {
                reason: reason.clone(),
            },
            EngineEvent::ModelChanged {
                model,
                previous,
                context_tokens,
                context_window,
            } => Frame::ModelChanged {
                model: model.clone(),
                previous: previous.clone(),
                context_tokens: *context_tokens,
                context_window: *context_window,
            },
            EngineEvent::Cleared { count } => Frame::Cleared { count: *count },
            EngineEvent::TodosChanged { items } => Frame::TodosChanged {
                items: items.clone(),
            },
            EngineEvent::PlanPresented {
                summary,
                nodes,
                path,
            } => Frame::PlanPresented {
                summary: summary.clone(),
                nodes: nodes.clone(),
                path: path.clone(),
            },
            EngineEvent::GoalChanged { condition } => Frame::GoalChanged {
                goal: condition.clone(),
            },
            EngineEvent::MemoryChanged {
                note,
                date,
                bytes,
                path,
            } => Frame::MemoryChanged {
                note: note.clone(),
                date: date.clone(),
                bytes: *bytes,
                path: path.clone(),
            },
            EngineEvent::JobStarted {
                id,
                job,
                pid,
                command,
                detached,
            } => Frame::JobStarted {
                id: id.clone(),
                job: job.clone(),
                pid: *pid,
                command: command.clone(),
                detached: *detached,
            },
            EngineEvent::JobDone {
                id,
                job,
                rc,
                secs,
                stopped_by,
            } => Frame::JobDone {
                id: id.clone(),
                job: job.clone(),
                rc: rc.clone(),
                secs: *secs,
                stopped_by: stopped_by.clone(),
            },
            EngineEvent::SessionTitled { title, usage } => Frame::SessionTitled {
                usage: *usage,
                title: title.clone(),
            },
            EngineEvent::AsideDone {
                question,
                text,
                usage,
            } => Frame::AsideResult {
                question: question.clone(),
                text: text.clone(),
                usage: *usage,
            },
            EngineEvent::AsideFailed { question, reason } => Frame::AsideFailed {
                question: question.clone(),
                reason: reason.clone(),
            },
            EngineEvent::GoalVerdict {
                verdict,
                reason,
                turns,
                usage,
                evidence,
            } => Frame::GoalVerdict {
                verdict: goal_verdict_tag(*verdict),
                reason: reason.clone(),
                turns: *turns,
                usage: *usage,
                // Omitted, not `[]`, when nothing was observed.
                evidence: (!evidence.is_empty()).then(|| evidence.clone()),
            },
            EngineEvent::LedgerReport(summary) => Frame::LedgerReport {
                sample_count: summary.sample_count,
                overhead_p50_ns: summary.overhead_p50_ns,
                overhead_p99_ns: summary.overhead_p99_ns,
                max_rss_bytes: summary.max_rss_bytes,
                phase_deltas: summary.phase_deltas.clone(),
            },
            EngineEvent::Ask { .. }
            | EngineEvent::Question { .. }
            // `EgressAsk` is a round-trip like `Ask`, and the JSON stream is
            // the headless surface, which has no human to answer it.
            | EngineEvent::EgressAsk { .. }
            | EngineEvent::TurnDone { .. } => return None,
        })
    }
}

/// The `goal_verdict` frame's stable wire tag — never a `Debug` rendering.
/// Which ladder a `retrying` frame came from (0061 T15).
fn retry_scope_tag(scope: hotl_engine::RetryScope) -> RetryScopeTag {
    match scope {
        hotl_engine::RetryScope::Sample => RetryScopeTag::Sample,
        hotl_engine::RetryScope::Summarize => RetryScopeTag::Summarize,
        hotl_engine::RetryScope::GoalEval => RetryScopeTag::GoalEval,
        hotl_engine::RetryScope::Title => RetryScopeTag::Title,
        hotl_engine::RetryScope::Aside => RetryScopeTag::Aside,
    }
}

fn goal_verdict_tag(verdict: hotl_engine::GoalVerdictKind) -> GoalVerdictTag {
    match verdict {
        hotl_engine::GoalVerdictKind::NotYet => GoalVerdictTag::NotYet,
        hotl_engine::GoalVerdictKind::Met => GoalVerdictTag::Met,
        hotl_engine::GoalVerdictKind::Impossible => GoalVerdictTag::Impossible,
        hotl_engine::GoalVerdictKind::EvalFailed => GoalVerdictTag::EvalFailed,
        hotl_engine::GoalVerdictKind::Stalled => GoalVerdictTag::Stalled,
        hotl_engine::GoalVerdictKind::Errored => GoalVerdictTag::Errored,
    }
}

/// A turn outcome as a tagged object. The headless stream used
/// `format!("{outcome:?}")` instead — unparseable, and un-versionable, since a
/// field rename in `Outcome` would have changed the wire silently.
pub fn outcome_frame(outcome: &Outcome) -> Value {
    serde_json::to_value(OutcomeFrame::of(outcome)).expect("an outcome always serializes")
}

impl OutcomeFrame {
    pub fn of(outcome: &Outcome) -> OutcomeFrame {
        match outcome {
            Outcome::Done { text } => OutcomeFrame::Done { text: text.clone() },
            Outcome::Cancelled => OutcomeFrame::Cancelled,
            Outcome::TurnLimit => OutcomeFrame::TurnLimit,
            Outcome::Refused => OutcomeFrame::Refused,
            Outcome::DoomLoop { pattern } => OutcomeFrame::DoomLoop {
                pattern: pattern.clone(),
            },
            Outcome::ToolFailureBudget { tool } => {
                OutcomeFrame::ToolFailureBudget { tool: tool.clone() }
            }
            // Both carry a `message` as well as their numbers: every client
            // that reads an outcome's text then shows the refusal that names
            // the knob, not a bare kind word.
            Outcome::DenialSpiral { consecutive, total } => OutcomeFrame::DenialSpiral {
                consecutive: *consecutive,
                total: *total,
                message: format!(
                    "{consecutive} calls denied in a row ({total} this turn) — change what \
                     you are asking for, or relax the rule that is denying it"
                ),
            },
            Outcome::Budget { kind, used, cap } => OutcomeFrame::Budget {
                budget: kind.clone(),
                used: *used,
                cap: *cap,
                message: hotl_engine::budget_refusal(kind, *used, *cap),
            },
            Outcome::Error { message } => OutcomeFrame::Error {
                message: message.clone(),
            },
        }
    }
}

/// A turn's usage as JSON, `hit_ratio` and `cost_usd` added on top when there
/// is something to report (§S1 cache telemetry; Task 5 cost telemetry).
/// Single derivation site so headless `--json` and the ACP wire (`acp.rs`'s
/// `TurnDone` handling) can't drift on either formula. Both are omitted, not
/// `null`, when absent — a plain, uncached turn's usage bytes on an
/// uncatalogued model are unchanged from before these fields existed.
///
/// `model` prices the whole turn at the session's PRIMARY model. A turn can
/// aggregate samples served by a fallback model mid-turn (`FallbackModel`
/// events) at a different rate — this is accepted imprecision, not a bug:
/// the alternative (pricing per-sample) would need usage attribution this
/// wire shape doesn't carry.
pub fn usage_frame(model: &str, usage: &TokenUsage) -> Value {
    serde_json::to_value(UsageFrame::of(model, usage)).expect("usage always serializes")
}

impl UsageFrame {
    pub fn of(model: &str, usage: &TokenUsage) -> UsageFrame {
        UsageFrame {
            usage: *usage,
            hit_ratio: usage.hit_ratio(),
            cost_usd: catalog::cost_usd(model, usage),
        }
    }
}

/// One `-p --json` line: an update frame, or the turn-done/ask/question frames
/// only that stream emits — always version-stamped.
///
/// Pure and total: it returns a `Value` for *every* variant, including the two
/// that need side effects (`Ask` and `Question` must resolve their reply
/// channels). Those side effects stay in `agent.rs::render_json`; only the
/// shape lives here, which is what makes the whole stream pin-testable.
///
/// `model` is the session's primary model, needed only for the `TurnDone`
/// arm's `cost_usd` — see `usage_frame`.
pub fn json_frame(event: &EngineEvent, model: &str) -> Value {
    let frame = Frame::stream(event).unwrap_or_else(|| match event {
        EngineEvent::TurnDone {
            outcome,
            usage,
            mispredictions,
        } => Frame::TurnDone {
            outcome: OutcomeFrame::of(outcome),
            usage: UsageFrame::of(model, usage),
            // Omitted at zero rather than written as `0`/`null`.
            mispredictions: (*mispredictions > 0).then_some(*mispredictions),
        },
        EngineEvent::Ask { summary, .. } => Frame::AskDenied {
            summary: summary.clone(),
        },
        EngineEvent::Question { question, .. } => Frame::QuestionNoHuman {
            header: question.header.clone(),
            prompt: question.prompt.clone(),
            options: question.options.clone(),
        },
        _ => unreachable!("Frame::stream covers every other variant"),
    });
    serde_json::to_value(JsonStreamLine {
        frame,
        schema_version: JSON_STREAM_SCHEMA_VERSION,
    })
    .expect("a stream line always serializes")
}

/// The published schema of one `-p --json` line, `$id` set to its URL.
pub fn json_stream_schema() -> Value {
    let mut v = serde_json::to_value(schemars::schema_for!(JsonStreamLine))
        .expect("a schema always serializes");
    v["$id"] = json!(JSON_STREAM_SCHEMA_ID);
    v["title"] = json!("hotl -p --json stream line");
    v
}

/// Where the stream schema is published (0076 D3); bump the file with the major.
pub const JSON_STREAM_SCHEMA_ID: &str =
    "https://nrakochy.github.io/hotl/schema/json-stream-v2.schema.json";
