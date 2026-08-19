//! One turn: sample → tools → sample, until a terminal outcome.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::{retry, CachePolicy, ProviderError, SamplingRequest, StreamEvent, ToolDef};
use hotl_tools::rules::Verdict;
use hotl_tools::{Permission, ToolOutcome};
use hotl_types::{
    assistant_text, assistant_tool_uses, EntryPayload, Item, StopReason, SyntheticReason,
    TokenUsage, ToolResultItem, ToolUse,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actor::SharedDeps;
use crate::ledger::Phase;
use crate::{AskReply, EngineEvent, Outcome, SessionCmd, TurnEnd};

/// Compaction triggers when the estimated next request crosses this share of
/// the window (M2; the estimate overcounts, so the miss direction is early).
const COMPACT_TRIGGER: f64 = 0.8;
/// Speculative compaction starts here: the digest summarize fires in the
/// background so it is (usually) already done when [`COMPACT_TRIGGER`] hits,
/// and the fold needs no blocking model call.
const SPECULATE_TRIGGER: f64 = 0.6;
/// Wall-clock bound on waiting for a speculative digest at the fold. The task
/// has had whole samples to run in, so a digest that still isn't ready is a
/// stalled provider call, not a slow one — the inline summarize is then the
/// faster path *and* the interruptible one.
/// INVARIANT: no await in the turn path is unbounded or un-cancellable.
/// (Shadow snapshots no longer await at all — the `Snapshotter` trait is
/// sync-enqueue by construction, 0035.)
/// Enforced by `the_speculation_bound_abandons_a_stalled_digest` (the bound),
/// `hung_speculation_falls_back_to_the_inline_summarize` (end to end), and
/// `interrupt_lands_during_the_speculative_summarize` (the cancel race).
const SPECULATION_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether a projection must fold before it can be sampled. Two independent
/// pressures: tokens, and the image bytes the token estimate deliberately
/// under-charges at a flat 1600 each. Pure so both are testable without a
/// session behind them.
///
/// INVARIANT: this returns `true` whenever image bytes cross
/// `IMAGE_B64_BUDGET`, regardless of the token estimate. Enforced by
/// `image_bytes_past_the_budget_force_a_fold_at_a_trivial_token_estimate`.
/// Termination is NOT guaranteed here: a fold can hand back a tail still
/// over budget (`compaction::plan`'s minimal-tail fallback); the real
/// backstop against looping is `MAX_COMPACT_STREAK` in `crate::actor`.
fn must_compact(estimate: u64, window: u64, image_bytes: usize) -> bool {
    estimate > (window as f64 * COMPACT_TRIGGER) as u64
        || image_bytes > hotl_types::IMAGE_B64_BUDGET
}

/// Whether the speculative digest should fire early (M2): tokens past
/// [`SPECULATE_TRIGGER`], or image bytes past the same trigger ratio applied
/// to [`hotl_types::IMAGE_B64_BUDGET`] (0.6 / 0.8 = 0.75 of the budget) —
/// mirrors `must_compact`'s two pressures so byte-heavy sessions get the same
/// speculative head start token-heavy ones do. Pure for the same reason
/// `must_compact` is.
///
/// INVARIANT: fires strictly before `must_compact` on the byte axis, same as
/// it already does on the token axis (`SPECULATE_TRIGGER < COMPACT_TRIGGER`):
/// 0.75 * IMAGE_B64_BUDGET < IMAGE_B64_BUDGET. Enforced by
/// `image_bytes_past_the_speculate_threshold_fire_early_at_a_trivial_token_estimate`.
fn should_speculate(estimate: u64, window: u64, image_bytes: usize) -> bool {
    estimate > (window as f64 * SPECULATE_TRIGGER) as u64
        || image_bytes as f64
            > hotl_types::IMAGE_B64_BUDGET as f64 * (SPECULATE_TRIGGER / COMPACT_TRIGGER)
}

/// Shared per-prompt budget for "intercept the end-turn, inject a reminder,
/// continue" gates (index E4): the TodoGate and the `Stop` hook veto
/// (`Turn::consult_stop`) both draw from this same counter (see
/// `Turn::turn_extensions`) rather than each having its own, so composing
/// gates can't multiply worst-case turn extensions — a turn is extended at
/// most `TURN_EXTENSION_MAX` times total, ever, regardless of which gate(s)
/// fired on any given pass. Capped above any single gate's own bound — it's
/// the *combined* ceiling.
///
/// The budget is per **prompt**, not per `drive()` call: it rides
/// [`crate::TurnContinuation`], so a compaction mid-turn no longer refills it
/// (before T2-2 that claim was false — every fold reset the counter).
/// INVARIANT: a turn is extended at most this many times per prompt, folds
/// included. Enforced by
/// `an_always_block_stop_hook_composed_with_the_todo_gate_never_exceeds_the_combined_cap`
/// (the combined cap) and `max_turns_is_enforced_across_a_compaction` (that
/// per-turn counters survive a fold).
const TURN_EXTENSION_MAX: u32 = 3;
/// The TodoGate's own bound within the shared budget (01 §agent-loop's
/// `max_fires_per_prompt`).
/// INVARIANT: the gate never blocks an unattended (`Auto`/`DontAsk`) run beyond
/// this — a gate that can wedge one is a bug. Enforced by
/// `the_gate_fires_at_most_twice_then_lets_the_turn_end`.
const TODO_GATE_MAX: u32 = 2;
/// Truncation-recovery budget, separate from `TURN_EXTENSION_MAX` — that
/// budget's invariant is "end-turn intercept gates", and truncation recovery
/// must not starve the TodoGate (or vice versa).
const MAX_TOKENS_CONTINUE_MAX: u32 = 3;

pub(crate) async fn run(
    shared: Arc<SharedDeps>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
    cont: crate::TurnContinuation,
    admission: Option<crate::Admission>,
) {
    let mut turn = Turn::new(shared, cmd_tx.clone(), events, cancel, cont);
    if let Some(admission) = admission {
        turn.admit(admission).await;
    }
    let end = turn.drive().await;
    // Barrier (c) (commit-protocol.md §Pipelined commits): "a turn does not
    // finish while it still holds an unresolved claim on the log." Every exit
    // path from `drive` passes through here, cancellation and panic-free
    // errors included.
    let end = seal_end(end, turn.pipeline.drain().await);
    let usage = turn.usage;
    // Flush the ledger (§S1) on the existing event channel — never the
    // canon log — before telling the actor the turn is over.
    let report = turn.ledger.summary(crate::ledger::max_rss_bytes());
    let _ = turn.events.send(EngineEvent::LedgerReport(report)).await;
    let _ = cmd_tx.send(SessionCmd::TurnFinished { end, usage }).await;
}

/// Reconcile a turn's end with what barrier (c) found. A commit that failed
/// after the turn thought it was finished invalidates the end reason: the log
/// does not contain what that reason names, which is the same rule
/// [`Turn::handle_doom_loop`] already applies inline ("a `DoomLoop` outcome
/// would name a batch the log will never carry"). Two ends survive: one that
/// is already an error — keep the first cause — and a cancellation, because
/// Cancelled ≠ Failed. A `Compact` end needs no override: the fold runs
/// against the sealed log next and fails there, with its own message.
fn seal_end(end: TurnEnd, commit: Commit) -> TurnEnd {
    if commit.ok() {
        return end;
    }
    match end {
        TurnEnd::Outcome(Outcome::Cancelled) => TurnEnd::Outcome(Outcome::Cancelled),
        TurnEnd::Outcome(Outcome::Error { message }) => {
            TurnEnd::Outcome(Outcome::Error { message })
        }
        TurnEnd::Outcome(_) => TurnEnd::Outcome(commit.outcome()),
        compact @ TurnEnd::Compact { .. } => compact,
    }
}

/// One gated call: ready to execute with its (possibly hook-rewritten or
/// human-edited) input, or already answered without running.
enum Gate {
    Ready {
        input: Value,
        summary: String,
        /// The plan-0022 per-command credential-read grant, if the human gave
        /// it. `execute` scopes it around the single `Tool::run` future, which
        /// is what makes "per command" structural rather than a convention.
        secret_reads: bool,
        /// The plan-0026 shown-hosts registration for this call. Held until
        /// `execute` finishes, so the egress ask can tell "the human saw this
        /// host" from "something reached out on its own". Dropped with the
        /// gate on every path, including denials and panics.
        shown: Option<hotl_tools::net::ShownHostsGuard>,
    },
    /// Answered without running. `chargeable` is false for outcomes the model
    /// cannot fix by trying again — a human denial, a hook block, an
    /// interrupted turn — so they never draw down the retry budget and never
    /// get the last-chance `<system-hint>` that contradicts the message (T3-1).
    Resolved {
        outcome: ToolOutcome,
        chargeable: bool,
    },
}

/// What [`Turn::approve_input`] produces: the reply, plus whether a *human*
/// produced it.
///
/// The second field exists because `Verdict::Auto` and `Verdict::Ask` both
/// yield `AskReply::Allow`, and plan 0026's shown-hosts rule turns on the
/// difference. Reported, never inferred: anything that reconstructs "was there
/// a human?" downstream — checking the mode, checking whether an event was
/// emitted — is the bug that turns every allow-rule into a standing egress
/// grant (0026 watch-out 11).
struct Approval {
    reply: AskReply,
    by_human: bool,
}

impl Approval {
    /// A rule answered. An allow-rule is not a human and must never count as
    /// one.
    fn by_rule(reply: AskReply) -> Self {
        Self {
            reply,
            by_human: false,
        }
    }
}

/// What [`Turn::execute`] produces: the result, plus whether it counts against
/// the per-tool failure budget.
struct Executed {
    outcome: ToolOutcome,
    chargeable: bool,
}

/// Unresolved commit tickets a turn may hold before it must wait on the
/// oldest (commit-protocol.md §Pipelined commits). Counted in **tickets**,
/// not entries: a proposal yields exactly one ticket today, and a `Group`
/// will yield one for several entries (S2c) without changing this number.
///
/// The bound does two jobs — backpressure, and capping how wrong a turn can
/// be about durability: a sealed log is detected within at most this many
/// commits, not at turn end. That is why it is small and fixed rather than
/// tuned.
const ACK_WINDOW: usize = 16;

/// Why a proposal did or didn't land. `propose` returned a bare `bool`, and both
/// failure modes rendered as "session log is sealed" — so an ordinary shutdown
/// was reported to the user as log corruption (T3-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Commit {
    Committed,
    /// The actor refused the append: the log is sealed.
    Sealed,
    /// A compaction or branch move superseded this turn while its commits
    /// were in flight (commit-protocol.md §conflict table, Abort). The bytes
    /// already forwarded are canon; the turn's claim on them is not.
    Aborted,
    /// A pipelined proposal's ticket reached a caller that does not wait on
    /// tickets, so nothing ever confirmed the commit. An internal routing
    /// bug, not a disk one — and never reported as success, because "an
    /// unwaited commit" and "a durable commit" are exactly what
    /// fsync-before-ack keeps apart.
    Unconfirmed,
    /// `PreparedPayload::rules_epoch` predated the actor's current rules
    /// (commit-protocol.md §Proposal payloads' guard) and the one retry in
    /// `Turn::propose` also came back stale — vanishingly unlikely (the
    /// epoch only ever advances, and the retry reads it fresh), surfaced as
    /// a failure rather than looping.
    StaleEpoch,
    /// The actor is gone (channel closed / reply dropped): normal shutdown.
    Gone,
}

impl Commit {
    /// The actor's reply, `None` if it never answered. A `Ticket` is not a
    /// commit yet — it is settled at a barrier, by
    /// [`TicketPipeline::resolve_oldest`] — so it must never reach here, and
    /// the arm below is a guard rather than a translation.
    fn from_reply(reply: Option<crate::ProposeReply>) -> Self {
        match reply {
            Some(crate::ProposeReply::Committed) => Commit::Committed,
            Some(crate::ProposeReply::Sealed) => Commit::Sealed,
            Some(crate::ProposeReply::StaleEpoch) => Commit::StaleEpoch,
            Some(crate::ProposeReply::Ticket(_)) => {
                // Dropping the ticket here would report an unwaited commit as
                // durable — the one thing fsync-before-ack forbids. Loud in
                // tests, safe in release.
                debug_assert!(
                    false,
                    "a ticket is not a commit: a Pipelined reply must go through TicketPipeline"
                );
                Commit::Unconfirmed
            }
            None => Commit::Gone,
        }
    }

    fn ok(self) -> bool {
        matches!(self, Commit::Committed)
    }

    /// Keep the first failure: a barrier reports why the *oldest* thing that
    /// went wrong went wrong, not the last.
    fn and(self, next: Commit) -> Commit {
        if self.ok() {
            next
        } else {
            self
        }
    }

    /// How a turn ends when a barrier reports this. **Cancelled ≠ Failed**:
    /// an abort is a clean supersession by a compaction or branch move, and
    /// nothing in the UI may report it as a failure.
    fn outcome(self) -> Outcome {
        match self {
            Commit::Aborted => Outcome::Cancelled,
            other => Outcome::Error {
                message: other.message().into(),
            },
        }
    }

    /// Every failure string is a prompt: it tells the reader what to do next.
    fn message(self) -> &'static str {
        match self {
            Commit::Committed => "committed",
            Commit::Aborted => "the turn was superseded and will be restarted",
            Commit::Unconfirmed => {
                "an internal error left a commit unconfirmed — nothing can be trusted \
                 as recorded; start a new session to keep working"
            }
            Commit::Sealed => {
                "the session log is sealed — nothing further can be recorded; \
                 start a new session to keep working"
            }
            Commit::StaleEpoch => {
                "the masking rules changed twice while committing this entry; \
                 start a new session to keep working"
            }
            Commit::Gone => "the session is shutting down",
        }
    }
}

/// Payloads at or above this size (by [`payload_size_estimate`], checked
/// BEFORE any work runs) have both serialization and masking moved onto
/// `tokio::task::spawn_blocking` (commit-protocol.md §Proposal payloads;
/// speed-architecture design's "CpuPool past the 1ms threshold", Rule 0 —
/// the workspace has no CpuPool, so `spawn_blocking` is the sanctioned
/// stand-in). Both steps, not just masking: for the spec's own worst case
/// (ten tool results at ~19K tokens each, ~570KB), `serde_json::to_string`
/// alone is ms-scale and blocking the turn task's async executor for it
/// defeats half the point of offloading — T3-16 names masking's
/// O(pairs × len) cost, but that was never a claim that serialization is
/// free at this size, only that it's *cheaper than masking*, which still
/// leaves it too expensive to run inline unblocked past this threshold.
const PREPARE_OFFLOAD_BYTES: usize = 256 * 1024;

/// Cheap, allocation-free lower-bound estimate of `payload`'s eventual
/// serialized size — used only to decide whether `prepare_entry` offloads
/// onto `spawn_blocking`, never to size anything durable. Deliberately not a
/// full `serde_json::to_string` call: that IS the cost the offload decision
/// has to be made *before* paying, so the estimate must be cheaper than the
/// thing it estimates. An under-estimate is safe — the worst case is a large
/// payload staying inline, which is correct, just not optimally fast; the
/// only kind that can realistically cross the 256KiB threshold is `Item`
/// (a tool result or a long assistant/user message), so every other kind
/// estimates at 0 and always takes the inline path.
fn payload_size_estimate(payload: &EntryPayload) -> usize {
    match payload {
        EntryPayload::Item { item } => item_size_estimate(item),
        _ => 0,
    }
}

fn item_size_estimate(item: &Item) -> usize {
    match item {
        Item::User { text, .. } | Item::System { text } => text.len(),
        Item::ToolResults { results } => results.iter().map(|r| r.content.len()).sum(),
        Item::Assistant { blocks } => blocks.iter().map(value_size_estimate).sum(),
        Item::Unknown => 0,
    }
}

/// Sum of string byte lengths in a `serde_json::Value` tree — an
/// under-estimate of its eventual JSON size (it ignores structural
/// punctuation, key names, and escaping expansion), which is exactly the
/// safe direction for [`payload_size_estimate`]'s purpose.
fn value_size_estimate(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Array(items) => items.iter().map(value_size_estimate).sum(),
        Value::Object(map) => map.values().map(value_size_estimate).sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

/// `prepare_entry` failed to produce a `PreparedEntry`. Both arms are
/// effectively unreachable in practice (see call sites), but a proposal
/// build must not panic the turn task, so they are surfaced as `Commit::Gone`
/// rather than unwrapped.
#[derive(Debug)]
pub(crate) enum PrepareError {
    /// `serde_json::to_string` on an `EntryPayload` errored — none of this
    /// crate's payload shapes contain non-finite floats or non-string map
    /// keys, so this should never actually happen (hence the `debug_assert`
    /// at the one call site that observes this variant specifically, in
    /// `Turn::propose_at_epoch`). Neither call site inspects the underlying
    /// `serde_json::Error` (both just fall back to `Commit::Gone`), so it is
    /// not carried — see `Commit::message`'s `StaleEpoch`/`Gone` text for
    /// what the user actually sees.
    Serialize,
    /// The `spawn_blocking` task preparing a large payload panicked or was
    /// aborted (a runtime shutting down mid-proposal) — unlike `Serialize`,
    /// this CAN legitimately happen on a real shutdown race, so it is never
    /// asserted against.
    Offload,
}

impl From<serde_json::Error> for PrepareError {
    fn from(_: serde_json::Error) -> Self {
        PrepareError::Serialize
    }
}

/// Build one entry's `PreparedPayload` — the turn-side half of
/// commit-protocol.md §Proposal payloads. A free function (not `&Turn`), so
/// it is directly unit-testable (masking correctness, the epoch stamp, the
/// offload threshold) without spinning up a session.
pub(crate) async fn prepare_entry(
    payload: &EntryPayload,
    masker: &Arc<hotl_store::Masker>,
    rules_epoch: u32,
) -> Result<crate::PreparedEntry, PrepareError> {
    // The exact value the turn already built in memory — kept for the
    // actor's live projection, never re-parsed from the bytes below (that
    // would just move T3-16's per-entry cost back onto the actor).
    // Unconditional: every `Item` entry needs its own owned copy for the
    // projection regardless of size or which branch below runs, so there is
    // no smaller class of payload this could be deferred for.
    let item = match payload {
        EntryPayload::Item { item } => Some(item.clone()),
        _ => None,
    };
    let kind = hotl_store::EntryKind::from(payload);
    let bytes = if payload_size_estimate(payload) >= PREPARE_OFFLOAD_BYTES {
        // Large enough that BOTH the serde pass and the masking scan are
        // worth moving off the turn task's async context — the whole
        // prepare runs on spawn_blocking, not just masking, so a
        // >-threshold tool result never blocks the executor for either
        // step. `payload.clone()` here is the one real cost of this path
        // (an EntryPayload::Item clone on top of the `item` clone above),
        // bounded to exactly the payloads already crossing the threshold —
        // eliminating it would need `item`/`payload` to share ownership
        // (e.g. an `Arc<Item>`), which is a larger type change than this
        // fix warrants.
        let masker = Arc::clone(masker);
        let owned = payload.clone();
        tokio::task::spawn_blocking(move || -> Result<hotl_store::MaskedBytes, PrepareError> {
            let json = hotl_store::serialize_payload(&owned)?;
            Ok(hotl_store::mask_json(&masker, json))
        })
        .await
        .map_err(|_| PrepareError::Offload)??
    } else {
        let json = hotl_store::serialize_payload(payload)?;
        hotl_store::mask_json(masker, json)
    };
    Ok(crate::PreparedEntry::new(
        hotl_store::PreparedPayload::new(bytes, kind, rules_epoch),
        item,
    ))
}

/// Chunk a batch for execution: contiguous runs of parallel-safe calls form
/// one chunk (they may overlap); every other call is its own single-entry
/// chunk, keeping strict source order around anything mutating or unknown.
fn parallel_chunks<'a>(uses: &'a [ToolUse], registry: &hotl_tools::Registry) -> Vec<&'a [ToolUse]> {
    let safe = |tu: &ToolUse| registry.get(&tu.name).is_some_and(|t| t.parallel_safe());
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < uses.len() {
        let mut end = start + 1;
        if safe(&uses[start]) {
            while end < uses.len() && safe(&uses[end]) {
                end += 1;
            }
        }
        chunks.push(&uses[start..end]);
        start = end;
    }
    chunks
}

/// Fire the compaction summarize against the current snapshot in the
/// background. The plan is fixed now; items appended later simply extend the
/// verbatim tail the fold keeps. `None` when there is nothing to fold yet.
fn spawn_speculation(
    shared: &Arc<SharedDeps>,
    snapshot: &Arc<Vec<Arc<Item>>>,
    cancel: &CancellationToken,
) -> Option<tokio::task::JoinHandle<Option<crate::SpecDigest>>> {
    let tail_budget = (shared.config.context_window as f64 * crate::actor::TAIL_RATIO) as u64;
    let plan = hotl_context::compaction::plan(snapshot, tail_budget, hotl_types::IMAGE_B64_BUDGET)?;
    let shared = Arc::clone(shared);
    let snapshot = Arc::clone(snapshot);
    let cancel = cancel.clone();
    Some(tokio::spawn(async move {
        let folded = &snapshot[plan.prefix_end..plan.kept_from];
        // A cancelled turn stops paying for a digest nobody will fold.
        let text = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            text = crate::actor::summarize(&shared, folded) => text,
        }?;
        Some(crate::SpecDigest {
            prefix_end: plan.prefix_end,
            kept_from: plan.kept_from,
            text,
        })
    }))
}

/// Race a speculative digest against the turn's cancellation and a wall-clock
/// bound, aborting the task on either. Abandoning the digest is a handled path
/// — `actor::compact` falls back to the inline summarize — so this never waits
/// on a stalled provider call. Split out from [`Turn::take_speculation`] so the
/// race is unit-testable without a session behind it.
async fn await_speculation(
    handle: tokio::task::JoinHandle<Option<crate::SpecDigest>>,
    cancel: &CancellationToken,
    wait: std::time::Duration,
) -> Option<crate::SpecDigest> {
    // Cloned out first: `handle` itself moves into the select arm below.
    let abort = handle.abort_handle();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { abort.abort(); None }
        _ = tokio::time::sleep(wait) => { abort.abort(); None }
        digest = handle => digest.ok().flatten(),
    }
}

/// The turn's bounded window of unresolved commit tickets
/// (commit-protocol.md §Pipelined commits). A turn pushes a ticket and goes
/// back to work; the three hard barriers — before a tool batch, before the
/// sample-boundary snapshot refresh, and at turn end — drain it whole.
///
/// INVARIANT: the window never exceeds [`ACK_WINDOW`] tickets, and a turn
/// never finishes holding one. Enforced by
/// `the_ticket_window_waits_on_the_oldest_before_it_overflows` and barrier
/// (c) in [`run`].
#[derive(Default)]
struct TicketPipeline {
    tickets: VecDeque<crate::CommitTicket>,
    /// `(id, seq)` of the newest ticket this turn submitted — the identity
    /// and commit order the actor assigned **eagerly**, before the write.
    /// `id` is the `expected_leaf` an optimistic dispatch is adopted
    /// against; `seq` is the `my_ack_seq` the refresh waits for. Kept after
    /// a drain empties `tickets`: the refresh runs *after* barrier (b), so
    /// the numbers have to outlive the tickets that carried them.
    last: Option<(String, u64)>,
}

impl TicketPipeline {
    fn is_empty(&self) -> bool {
        self.tickets.is_empty()
    }

    fn len(&self) -> usize {
        self.tickets.len()
    }

    /// The `seq` the sample-boundary refresh waits for, or `None` when this
    /// turn has committed nothing yet.
    fn ack_seq(&self) -> Option<u64> {
        self.last.as_ref().map(|(_, seq)| *seq)
    }

    /// The id of this turn's newest committed unit — an optimistic
    /// dispatch's `expected_leaf`.
    fn leaf(&self) -> Option<&str> {
        self.last.as_ref().map(|(id, _)| id.as_str())
    }

    /// Push a ticket, waiting on the oldest first once the window is full —
    /// the backpressure half of the bound.
    async fn submit(&mut self, ticket: crate::CommitTicket) -> Commit {
        let mut commit = Commit::Committed;
        while self.len() >= ACK_WINDOW {
            commit = commit.and(self.resolve_oldest().await);
        }
        self.last = Some((ticket.id.clone(), ticket.seq));
        self.tickets.push_back(ticket);
        commit
    }

    /// A barrier: resolve every outstanding ticket, oldest first, and report
    /// the first failure.
    async fn drain(&mut self) -> Commit {
        let mut commit = Commit::Committed;
        while !self.tickets.is_empty() {
            commit = commit.and(self.resolve_oldest().await);
        }
        commit
    }

    async fn resolve_oldest(&mut self) -> Commit {
        let Some(ticket) = self.tickets.pop_front() else {
            return Commit::Committed;
        };
        match ticket.ack.await {
            Ok(Ok(_)) => Commit::Committed,
            Ok(Err(crate::CommitFailed::LogSealed)) => Commit::Sealed,
            Ok(Err(crate::CommitFailed::Aborted)) => Commit::Aborted,
            // The actor dropped the ticket: an ordinary shutdown, never log
            // corruption (T3-5).
            Err(_) => Commit::Gone,
        }
    }
}

/// A next-sample request built and **sent before the boundary group it
/// follows is durable** (commit-protocol.md §Causal groups (b)). The stream
/// is held un-adopted: its events are buffered here and never forwarded, so
/// nothing it produces can reach the transcript, the projection or the event
/// channel until the refresh proves the leaf. A mispredict drops this whole
/// value — which is the cancellation: the buffer dies with it (ephemeral by
/// §Commit granularity, deltas that never reached a `BlockEnd` proposal) and
/// the stream itself is closed.
struct Speculation {
    /// Id of the last entry of the turn's own committed group. The ticket
    /// carried it eagerly, at issue — which is exactly what buys the early
    /// build, and exactly what does NOT buy the adoption (§Causal groups:
    /// "Knowing the id early buys the speculative build, never the
    /// adoption").
    expected_leaf: String,
    stream: SyncStream,
    /// Events pulled while the boundary group was still committing — the
    /// overlap this whole mechanism exists for.
    buffered: Vec<Result<StreamEvent, ProviderError>>,
    /// Set once a terminal event (`Completed`, an error, or end-of-stream)
    /// has been buffered: there is nothing further to overlap.
    terminal: bool,
}

/// A provider stream parked in a `Turn` field.
///
/// `BoxStream` is `Send` but not `Sync`, and a `Turn` must stay `Sync`: the
/// spawned turn future holds `&self` across awaits in several places
/// (`emit`, `gate`, `execute`), and `tokio::spawn` needs that future `Send`.
/// The mutex is never contended — every access goes through `&mut self` —
/// so it is only ever taken by `get_mut`/`into_inner`, which cannot block.
struct SyncStream(std::sync::Mutex<BoxStream<'static, Result<StreamEvent, ProviderError>>>);

impl SyncStream {
    fn new(stream: BoxStream<'static, Result<StreamEvent, ProviderError>>) -> Self {
        Self(std::sync::Mutex::new(stream))
    }

    fn get_mut(&mut self) -> &mut BoxStream<'static, Result<StreamEvent, ProviderError>> {
        self.0
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn into_inner(self) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        self.0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Speculation {
    /// Pull events into the buffer until the stream reaches its terminal
    /// event. Cancel-safe: each event is buffered the moment it arrives, so
    /// dropping this future (the refresh finished first, or the provider
    /// stalled) keeps everything already pulled and leaves the rest in the
    /// stream.
    async fn fill(&mut self) {
        while !self.terminal {
            match self.stream.get_mut().next().await {
                Some(event) => {
                    self.terminal = matches!(event, Ok(StreamEvent::Completed { .. }) | Err(_));
                    self.buffered.push(event);
                }
                None => self.terminal = true,
            }
        }
    }

    /// Adopt: the buffered prefix, then the rest of the same stream. The
    /// consumer drives this to end-of-stream exactly as it would a fresh
    /// one. The request itself is not carried out — it was handed to the
    /// provider at dispatch and nothing downstream reads it again.
    fn adopt(self) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        Box::pin(futures_util::stream::iter(self.buffered).chain(self.stream.into_inner()))
    }
}

/// A sample's terminal result, or why it couldn't produce one.
enum SampleEnd {
    Completed {
        stop: StopReason,
        blocks: Vec<Value>,
    },
    Cancelled,
    /// Availability-class failure: eligible for a model fallback.
    Unavailable(String),
    /// The next request won't fit (threshold or provider overflow): the turn
    /// ends and the actor compacts, then respawns a continuation.
    ContextFull,
    Fatal(String),
}

struct Turn {
    shared: Arc<SharedDeps>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
    tool_defs: Arc<[ToolDef]>,
    models: Vec<String>,
    model_idx: usize,
    /// The trailing tool-call signatures the doom-loop detector reads —
    /// bounded to [`DOOM_WINDOW`], not the whole turn.
    call_sigs: VecDeque<CallSig>,
    /// Flagged decisions already notified this prompt (0037 D5) — the floor
    /// still evaluates every call; only the notice fan-out is coalesced.
    flagged: HashSet<FlagKey>,
    consecutive_failures: HashMap<String, u32>,
    usage: TokenUsage,
    /// (provider-reported tokens, projection length) at the last completed
    /// sample — the anchor for context estimates (A12b).
    anchor: Option<(u64, usize)>,
    samples: u32,
    /// Subdir hints already injected (per turn) + the latest snapshot for
    /// cross-turn dedup against the projection.
    injected_hints: HashSet<String>,
    last_snapshot: Option<crate::actor::Snapshot>,
    /// In-flight speculative compaction digest: fired once the estimate
    /// crosses [`SPECULATE_TRIGGER`], consumed when the turn ends in
    /// `Compact`, aborted otherwise.
    speculation: Option<tokio::task::JoinHandle<Option<crate::SpecDigest>>>,
    /// Shared per-prompt "reminder and continue" budget (index E4) — see
    /// [`TURN_EXTENSION_MAX`]. Carried across a compaction respawn (one logical
    /// *prompt*, not one `drive()` call); the TodoGate is the only consumer
    /// today, bounded further by [`TODO_GATE_MAX`].
    turn_extensions: u32,
    /// Truncation-recovery continues spent ([`MAX_TOKENS_CONTINUE_MAX`]);
    /// crosses folds like `turn_extensions`.
    max_tokens_continues: u32,
    /// Steps spent against `EngineConfig::max_turns`. A `Turn` field rather
    /// than a `drive()` local precisely so it can cross a fold (T2-2).
    spent: i64,
    /// Completed samples since the last fold — the "intervening progress" the
    /// compaction streak is defined against (T2-3).
    samples_since_compact: u32,
    /// Loop-overhead instrument (§S1) — one per `Turn`, flushed once as an
    /// `EngineEvent::LedgerReport` when [`run`] returns.
    ledger: crate::ledger::LoopLedger,
    /// Commit tickets this turn is still carrying (§Pipelined commits).
    /// Deliberately NOT carried across a compaction respawn: barrier (c)
    /// empties it before the turn reports, so a continuation starts clean.
    pipeline: TicketPipeline,
    /// The read side of the actor's published head (§Read invariant): the
    /// sample-boundary refresh, and the only thing this turn ever reads.
    head: tokio::sync::watch::Receiver<Arc<crate::actor::ProjectionHead>>,
    /// The un-adopted next sample, when this turn dispatched one
    /// optimistically at the last boundary (§Causal groups (b)).
    speculative: Option<Speculation>,
    /// Items this turn committed since `last_snapshot` was granted, in
    /// commit order — the tail a speculative build predicts the next head
    /// will carry. Under leaf equality that prediction is exact, which is
    /// what makes the adopted request byte-identical to the sequential
    /// rebuild.
    projected_tail: Vec<Arc<Item>>,
}

impl Drop for Turn {
    /// A speculation the fold never consumed has nothing to serve — stop it on
    /// *every* exit path, including an unwind (a panicked turn would otherwise
    /// leak a live provider call, T1-5).
    /// INVARIANT: no speculation outlives its turn. Enforced by
    /// `interrupt_lands_during_the_speculative_summarize` on the cancel path;
    /// structural on the unwind path.
    fn drop(&mut self) {
        if let Some(handle) = self.speculation.take() {
            handle.abort();
        }
    }
}

impl Turn {
    fn new(
        shared: Arc<SharedDeps>,
        cmd_tx: mpsc::Sender<SessionCmd>,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
        cont: crate::TurnContinuation,
    ) -> Self {
        // `models` is rebuilt from config — only the *index* carries, so a
        // config change between folds is still honored.
        let mut models = vec![shared.config.model.clone()];
        models.extend(shared.config.fallback_models.iter().cloned());
        let head = shared.head();
        Self {
            tool_defs: shared.registry.defs().into(),
            shared,
            cmd_tx,
            events,
            cancel,
            models,
            model_idx: cont.model_idx,
            call_sigs: cont.call_sigs,
            flagged: cont.flagged,
            consecutive_failures: cont.consecutive_failures,
            usage: TokenUsage::default(),
            // `anchor`/`injected_hints`/`last_snapshot` deliberately do NOT
            // carry: the fold rewrites the projection, so a stale anchor would
            // mis-slice it and a hint whose item was just folded away *should*
            // be re-injected. Not carrying the anchor costs nothing: the
            // pre-anchor fallback is the projection's O(1) running sum.
            anchor: None,
            samples: 0,
            injected_hints: HashSet::new(),
            last_snapshot: None,
            speculation: None,
            turn_extensions: cont.turn_extensions,
            max_tokens_continues: cont.max_tokens_continues,
            spent: cont.spent,
            // A continuation starts a fresh progress count: the value it
            // inherited was already read by `try_compact`, and re-carrying it
            // would let one productive stretch excuse every later fold.
            samples_since_compact: 0,
            // Deliberately NOT carried across a compaction respawn: each
            // `Turn` task flushes its own report when it ends (see `run`).
            ledger: crate::ledger::LoopLedger::new(),
            pipeline: TicketPipeline::default(),
            head,
            speculative: None,
            projected_tail: Vec::new(),
        }
    }

    /// The state the continuation inherits.
    fn continuation(&mut self) -> crate::TurnContinuation {
        crate::TurnContinuation {
            spent: self.spent,
            model_idx: self.model_idx,
            call_sigs: std::mem::take(&mut self.call_sigs),
            flagged: std::mem::take(&mut self.flagged),
            consecutive_failures: std::mem::take(&mut self.consecutive_failures),
            turn_extensions: self.turn_extensions,
            max_tokens_continues: self.max_tokens_continues,
            samples_since_compact: self.samples_since_compact,
        }
    }

    async fn drive(&mut self) -> TurnEnd {
        // A negative bound is the opt-in "no cap" posture (`EngineConfig::
        // max_turns`): the loop then only ever leaves through one of the
        // branches below — done, cancelled, compaction, refusal, tool budget
        // — so cancellation and the context-full path stay the real floor.
        let bound = self.shared.config.max_turns;
        while bound < 0 || self.spent < bound {
            self.spent += 1;
            // `BoundaryEnd` (§S1) closes the ledger slot `sample()` opened
            // with `BoundaryStart` — stamped at every way this iteration can
            // end (return or `continue`), never by falling off the loop body,
            // since a `return` from inside `while` skips straight past it.
            let (stop, blocks) = match self.sample().await {
                SampleEnd::Completed { stop, blocks } => (stop, blocks),
                SampleEnd::Cancelled => {
                    self.ledger.stamp(Phase::BoundaryEnd);
                    return TurnEnd::Outcome(Outcome::Cancelled);
                }
                SampleEnd::ContextFull => {
                    self.ledger.stamp(Phase::BoundaryEnd);
                    return TurnEnd::Compact {
                        spec: self.take_speculation().await,
                        cont: Box::new(self.continuation()),
                    };
                }
                SampleEnd::Unavailable(_) if self.model_idx + 1 < self.models.len() => {
                    self.model_idx += 1;
                    self.emit(EngineEvent::FallbackModel {
                        model: self.models[self.model_idx].clone(),
                    })
                    .await;
                    self.ledger.stamp(Phase::BoundaryEnd);
                    continue;
                }
                SampleEnd::Unavailable(m) | SampleEnd::Fatal(m) => {
                    self.ledger.stamp(Phase::BoundaryEnd);
                    return TurnEnd::Outcome(Outcome::Error { message: m });
                }
            };
            match stop {
                StopReason::ToolUse => {
                    let outcome = self.run_tool_phase(&blocks).await;
                    self.ledger.stamp(Phase::BoundaryEnd);
                    if let Some(outcome) = outcome {
                        return TurnEnd::Outcome(outcome);
                    }
                }
                StopReason::Refusal => {
                    self.ledger.stamp(Phase::BoundaryEnd);
                    return TurnEnd::Outcome(Outcome::Refused);
                }
                StopReason::MaxTokens => {
                    // Truncated AFTER complete tool calls: run them — the
                    // results are the natural continuation.
                    if !assistant_tool_uses(&blocks).is_empty() {
                        let outcome = self.run_tool_phase(&blocks).await;
                        self.ledger.stamp(Phase::BoundaryEnd);
                        if let Some(outcome) = outcome {
                            return TurnEnd::Outcome(outcome);
                        }
                    } else if self.max_tokens_continues < MAX_TOKENS_CONTINUE_MAX {
                        self.max_tokens_continues += 1;
                        let commit = self
                            .inject_reminder(
                                "Your previous response was cut off at the \
                                 output-token limit. Continue exactly where \
                                 you left off."
                                    .into(),
                            )
                            .await;
                        if !commit.ok() {
                            self.ledger.stamp(Phase::BoundaryEnd);
                            return TurnEnd::Outcome(commit.outcome());
                        }
                        self.ledger.stamp(Phase::BoundaryEnd);
                        continue;
                    } else {
                        // Bounded: accept the truncated text as done rather
                        // than looping forever on an output the cap can't fit.
                        self.ledger.stamp(Phase::BoundaryEnd);
                        return TurnEnd::Outcome(Outcome::Done {
                            text: assistant_text(&blocks),
                        });
                    }
                }
                StopReason::PauseTurn => {
                    // Server-side pause: re-sending with the trailing
                    // assistant turn resumes it. Bounded by max_turns.
                    self.ledger.stamp(Phase::BoundaryEnd);
                    continue;
                }
                _ => {
                    // Done branch (index E4): the TodoGate and a `Stop` hook
                    // veto both intercept "the model just stopped" and can
                    // inject-and-continue — evaluated in FIXED order
                    // (TodoGate first, the model's own bookkeeping; `on_stop`
                    // second, owner policy gets the last word) against the
                    // SAME pre-increment `turn_extensions` snapshot, so
                    // either (or both) firing in this pass costs exactly one
                    // unit of the shared combined budget, never one each.
                    let text = assistant_text(&blocks);
                    let todo_fires = self.todo_gate_should_fire();
                    let stop_reason = self.consult_stop(&text).await;
                    if todo_fires || stop_reason.is_some() {
                        self.turn_extensions += 1;
                        let commit = self.inject_gate_nudge(todo_fires, stop_reason).await;
                        if !commit.ok() {
                            // The reminder never landed: looping would burn a
                            // sample on a nudge the model will never see (T3-6).
                            self.ledger.stamp(Phase::BoundaryEnd);
                            return TurnEnd::Outcome(commit.outcome());
                        }
                        self.ledger.stamp(Phase::BoundaryEnd);
                        continue;
                    }
                    self.ledger.stamp(Phase::BoundaryEnd);
                    return TurnEnd::Outcome(Outcome::Done { text });
                }
            }
        }
        TurnEnd::Outcome(Outcome::TurnLimit)
    }

    /// The bounded "finish your work" nudge (01 §agent-loop's TodoGate,
    /// verbatim shape): true only when the model just answered with no tool
    /// calls, the todo reminder it sampled against still lists open work,
    /// and both the gate's own bound and the shared per-prompt budget have
    /// room left. Never true past [`TODO_GATE_MAX`] fires — the gate must
    /// never wedge an unattended (`Auto`/`DontAsk`) run.
    fn todo_gate_should_fire(&self) -> bool {
        // The room left is whichever bound is tighter — today that's always
        // `TODO_GATE_MAX` (it's the smaller constant), but writing it this
        // way keeps the check correct if a second gate ever starts drawing
        // down the same shared `turn_extensions` counter ahead of this one.
        self.turn_extensions < TODO_GATE_MAX.min(TURN_EXTENSION_MAX)
            && self
                .last_snapshot
                .as_ref()
                .is_some_and(|s| unfinished_todos(&s.tail))
    }

    /// `Stop` hook veto (Task 4, tech-debt #10): consulted only when the
    /// shared [`TURN_EXTENSION_MAX`] budget still has room and hooks are
    /// configured — a `Block{reason}` asks the turn to continue;
    /// `Allow`/no-hooks/no-room all resolve to `None` (never extends). Bounded
    /// by the SAME counter the TodoGate draws down, so a hook that always
    /// blocks can compose with the TodoGate without multiplying worst-case
    /// turn extensions (index E4).
    async fn consult_stop(&self, outcome_text: &str) -> Option<String> {
        if self.turn_extensions >= TURN_EXTENSION_MAX {
            return None;
        }
        // §S1 HookRouter gate: a masked-off (or hook-less) session skips the
        // call (and its timeout registration) entirely.
        crate::hooks::hook_gate!(
            self.shared.hooks,
            self.shared.hook_mask(),
            crate::hooks::EventMask::STOP,
            |hooks| {
                // A stop hook command runs outside any batch and may write
                // the workspace: taint any capture whose staging overlaps it
                // (0035 decision 7).
                if let Some(snapshots) = &self.shared.snapshots {
                    snapshots.mutation_started();
                }
                match crate::hooks::call_stop(hooks, outcome_text).await {
                    crate::hooks::StopDecision::Block { reason } => Some(reason),
                    crate::hooks::StopDecision::Allow => None,
                }
            },
            else None
        )
    }

    /// Commit the Done-branch gate nudge(s) as ONE tagged `SystemReminder`
    /// user item (Innovation #7 — one injection per commit point): when both
    /// the TodoGate and a Stop-hook `Block` land in the same continuation,
    /// their sections ride inside a single `<system-reminder>`, in fixed
    /// order (TodoGate section first, Stop section second), never as two
    /// adjacent items.
    async fn inject_gate_nudge(&mut self, todo_fires: bool, stop_reason: Option<String>) -> Commit {
        let mut body = String::new();
        if todo_fires {
            body.push_str(
                "You still have open items on your todo list. Continue working on them, \
                 or call todo_write to mark them completed or drop them, before ending \
                 your turn.",
            );
        }
        if let Some(reason) = stop_reason {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&reason);
        }
        self.inject_reminder(body).await
    }

    /// Commit one tagged `SystemReminder` user item at the sample boundary —
    /// the shared shape under the gate nudge and truncation recovery.
    async fn inject_reminder(&mut self, body: String) -> Commit {
        self.propose_pipelined(
            vec![EntryPayload::Item {
                item: Item::User {
                    text: format!("<system-reminder>{body}</system-reminder>"),
                    synthetic: Some(SyntheticReason::SystemReminder),
                    images: Vec::new(),
                },
            }],
            crate::SampleStage::AtBoundary,
        )
        .await
    }

    /// One provider sample against a fresh snapshot; commits the assistant
    /// item + usage on completion.
    async fn sample(&mut self) -> SampleEnd {
        self.ledger.start_sample();
        self.ledger.stamp(Phase::BoundaryStart);
        let (commit, head) = self.boundary().await;
        if !commit.ok() {
            // The speculative stream (if any) is dropped with `self` at the
            // end of the turn; it was never adopted, so it produced nothing.
            self.speculative = None;
            return match commit {
                Commit::Aborted => SampleEnd::Cancelled,
                other => SampleEnd::Fatal(other.message().into()),
            };
        }
        let Some(head) = head else {
            return SampleEnd::Fatal("session closed".into());
        };
        let snapshot = head.snapshot();
        self.ledger.stamp(Phase::SnapshotReady);
        self.samples += 1;
        // The adoption rule, in one line (commit-protocol.md §Causal groups):
        // adopt iff the refreshed head's leaf equals `expected_leaf`. Leaf
        // equality is the proof that nothing intervened — no steer, no queued
        // prompt, no todo write, no compaction, no branch move. Anything else
        // cancels (by dropping) and the turn rebuilds through the sequential
        // path.
        let adopted = self
            .speculative
            .take()
            .filter(|spec| head.leaf() == Some(spec.expected_leaf.as_str()))
            .map(Speculation::adopt);
        let stream = match adopted {
            Some(adopted) => {
                // The adopted request was built (and its threshold checked)
                // at the last boundary, so `build_request` never runs for
                // this sample — but the fold's head start still has to.
                let estimate = self.estimate_tokens(&snapshot);
                let image_bytes = hotl_context::tokens::image_b64_bytes(&snapshot.durable);
                self.maybe_speculate_digest(&snapshot, estimate, image_bytes);
                adopted
            }
            None => {
                let request = match self.build_request(&snapshot) {
                    Ok(request) => request,
                    Err(end) => return end,
                };
                self.shared.provider.stream(request)
            }
        };
        self.ledger.stamp(Phase::RequestBuilt);
        self.last_snapshot = Some(snapshot.clone());
        self.projected_tail.clear();

        let (stop, usage, mut blocks) = match self.collect_stream(stream).await {
            Ok(completed) => completed,
            Err(end) => return end,
        };
        if stop == StopReason::MaxTokens {
            prune_unsigned_trailing_thinking(&mut blocks);
        }
        self.usage += usage;
        // A completed sample is the "intervening progress" the compaction
        // streak is defined against (T2-3).
        self.samples_since_compact += 1;
        // Anchor: what the provider says this request cost, plus its output —
        // the base cost of the next request before any new items.
        let reported = usage.input_tokens
            + usage.cache_read_input_tokens
            + usage.cache_creation_input_tokens
            + usage.output_tokens;
        // `+ 1` for the assistant item this sample produced, which `reported`
        // already covers through `output_tokens`. The position indexes the
        // *durable* projection, which is now the only list it could index —
        // the ephemeral tail is a separate channel and can never slip into
        // the count (T2-1).
        self.anchor = Some((reported, snapshot.durable.len() + 1));
        let assistant = Item::Assistant {
            blocks: blocks.clone(),
        };
        self.projected_tail.push(Arc::new(assistant.clone()));
        self.ledger.stamp(Phase::BatchProposed);
        // The `Completed` boundary is ONE causal event — the assistant item
        // and the usage it was billed at — so it commits as one `Group`:
        // chained parent→child, one writer message, one `sync_data` (2→1),
        // one ticket (commit-protocol.md §Causal groups). Pipelined, which
        // is where S2b's structural win goes live: the tool phase that
        // follows waits on this commit at barrier (a) instead of the turn
        // blocking on its fsync here.
        let commit = self
            .propose_pipelined(
                vec![
                    EntryPayload::Item { item: assistant },
                    EntryPayload::Usage { usage },
                ],
                // This commit IS what closes the sample: the stream is
                // drained and the blocks are final.
                crate::SampleStage::AtBoundary,
            )
            .await;
        self.ledger.stamp(Phase::WatermarkDurable);
        if !commit.ok() {
            return SampleEnd::Fatal(commit.message().into());
        }
        SampleEnd::Completed { stop, blocks }
    }

    /// Extract this sample's tool calls and dispatch them. The doom-loop
    /// guard now lives at the top of [`Turn::run_tool_batch`] itself (§S1
    /// diet 2) — folded in at dispatch time rather than a separate
    /// pre-dispatch pass here, since a call's args are already final the
    /// moment `uses` exists.
    async fn run_tool_phase(&mut self, blocks: &[Value]) -> Option<Outcome> {
        let uses = assistant_tool_uses(blocks);
        self.run_tool_batch(&uses).await
    }

    /// Fold this batch's new call signatures into the trailing window and
    /// check it (§S1 diet 2: `fold_call_sigs` — at dispatch time, not a
    /// batch pass between the tool join and the next sample).
    fn fold_doom_window(&mut self, uses: &[ToolUse]) -> Option<String> {
        fold_call_sigs(&mut self.call_sigs, uses);
        detect_doom_loop(self.call_sigs.make_contiguous())
    }

    /// A detected pattern: hard-stop in an unattended mode, else ask
    /// whether to continue. `Some(outcome)` ends the turn (doom-aborted, or
    /// a commit failure reporting why); `None` means the human allowed it —
    /// the window is cleared so continuing doesn't immediately re-trigger
    /// the same warning on the very next batch.
    async fn handle_doom_loop(&mut self, uses: &[ToolUse], pattern: String) -> Option<Outcome> {
        // Bypass and DontAsk are the unattended postures: nobody is watching,
        // and the doom guard is a malfunction brake, not a permission —
        // stop the turn instead of asking a question no one will answer.
        // Plan is orthogonal and does not participate: plan+ask still has a
        // human at the keyboard.
        // INVARIANT: an unattended mode never emits an unanswerable Ask.
        // Enforced by `dont_ask_mode_hard_stops_on_a_doom_loop`.
        let unattended = matches!(
            self.shared.effective_mode(),
            hotl_tools::rules::PermissionMode::Bypass | hotl_tools::rules::PermissionMode::DontAsk
        );
        let stop = if unattended {
            true
        } else {
            let cont = self
                .ask(
                    format!("the agent keeps repeating: {pattern} — let it continue?"),
                    None,
                )
                .await;
            !matches!(cont, AskReply::Allow | AskReply::AllowEdited { .. })
        };
        if stop {
            let commit = self
                .abort_batch(uses, "Stopped: a repeating tool-call loop was detected.")
                .await;
            if !commit.ok() {
                // A `DoomLoop` outcome would name a batch the log will
                // never carry; report why the record failed instead (T3-6).
                return Some(commit.outcome());
            }
            return Some(Outcome::DoomLoop { pattern });
        }
        self.call_sigs.clear();
        None
    }

    /// Execute the batch with results paired in source order. Doom-loop
    /// detection for this batch's own new calls happens first — see
    /// [`Turn::fold_doom_window`]/[`Turn::handle_doom_loop`] — before either
    /// dispatch path below runs anything, so a detected pattern never
    /// executes the call that completed it. A single-tool batch — the
    /// majority case — runs inline, skipping the chunking and `join_all`
    /// machinery below (§S1 diet (1)). Everything else is split into
    /// chunks: contiguous runs of parallel-safe calls (pure reads, isolated
    /// children) execute concurrently; everything else runs alone, in
    /// order. Gating stays serial — asks are one-at-a-time human moments.
    /// A batch that actually mutates ends with one detached quiet-window
    /// snapshot plus a taint signal at its first mutating execute, so
    /// `hotl undo` can restore batch-end state (M3b, 0035).
    async fn run_tool_batch(&mut self, uses: &[ToolUse]) -> Option<Outcome> {
        // Barrier (a) (commit-protocol.md §Pipelined commits): write-ahead
        // semantics — no external side effect until the `tool_use` blocks
        // that caused it are durable. It is absolute for anything this
        // process cannot take back (§The side-effect ruling), and it covers
        // BOTH dispatch paths below, because it sits ahead of both.
        let commit = self.pipeline.drain().await;
        if !commit.ok() {
            return Some(commit.outcome());
        }
        if let Some(pattern) = self.fold_doom_window(uses) {
            if let Some(outcome) = self.handle_doom_loop(uses, pattern).await {
                return Some(outcome);
            }
            // The human allowed continuing past the warning; `handle_doom_
            // loop` already cleared the window. Fall through and dispatch
            // this same batch normally.
        }
        // In-batch identical-call dedup (0037 D6), after the doom fold (the
        // window and its backfill keep seeing the model's real traffic),
        // before chunking: N literally identical read-only parallel-safe
        // calls run once, and the outcome fans out to every id below.
        let deduped = dedup_batch(uses, &self.shared.registry);
        let run_uses: &[ToolUse] = deduped
            .as_ref()
            .map_or(uses, |(unique, _)| unique.as_slice());
        // Per-batch taint signal (0035 decision 7): fires once, after a
        // gate resolves to Ready, before the first non-read-only execute —
        // a minutes-long ask or a fully-denied batch never taints, and a
        // batch where nothing mutating ran takes no snapshot at all.
        let mut mutation_signaled = false;
        let mut results = Vec::with_capacity(run_uses.len());
        let mut budget_blown: Option<String> = None;
        self.ledger.stamp(Phase::ToolsSpawned);
        // Barrier (a) again, as an assertion rather than a wait: the inline
        // path is the one the spec names explicitly, and this is what makes a
        // future proposal site between the barrier and dispatch fail loudly.
        debug_assert!(
            self.pipeline.is_empty(),
            "barrier (a): no tool may run while a commit is unresolved"
        );
        if let [only] = run_uses {
            // §S1 diet (1): a single-tool batch is the majority case — skip
            // `parallel_chunks` and `join_all` entirely and run the one call
            // inline. Same gate → execute → outcome-handling body the
            // multi-tool branch below runs per call (via `finish_call`), same
            // `self.cancel` race inside `gate`/`execute` — there is just
            // nothing to wrap it in a concurrency combinator for.
            if self.cancel.is_cancelled() {
                results.push(pair(only, "Not executed (turn stopped).", true));
            } else {
                let gate = self.gate(only).await;
                self.signal_mutation(only, &gate, &mut mutation_signaled);
                let executed = self.execute(only, gate).await;
                results.push(self.finish_call(only, executed, &mut budget_blown).await);
            }
        } else {
            for chunk in parallel_chunks(run_uses, &self.shared.registry) {
                if self.cancel.is_cancelled() || budget_blown.is_some() {
                    for tu in chunk {
                        results.push(pair(tu, "Not executed (turn stopped).", true));
                    }
                    continue;
                }
                let mut gates = Vec::with_capacity(chunk.len());
                for tu in chunk {
                    gates.push(self.gate(tu).await);
                }
                for (tu, gate) in chunk.iter().zip(&gates) {
                    self.signal_mutation(tu, gate, &mut mutation_signaled);
                }
                // A chunk is one serial call or a run of parallel-safe calls
                // that overlap; join_all returns outcomes in source order
                // either way.
                let outcomes = futures_util::future::join_all(
                    chunk
                        .iter()
                        .zip(gates)
                        .map(|(tu, gate)| self.execute(tu, gate)),
                )
                .await;
                for (tu, executed) in chunk.iter().zip(outcomes) {
                    results.push(self.finish_call(tu, executed, &mut budget_blown).await);
                }
            }
        }
        self.ledger.stamp(Phase::ToolsJoined);
        // Fan the deduplicated outcomes back out: the provider requires one
        // result per `tool_use_id`, so every duplicate id gets its own
        // `ToolResultItem` carrying the shared outcome (0037 D6). Before the
        // backfill, which aligns against the window's per-original-call fold.
        if let Some((_, fan)) = &deduped {
            results = fan_results(uses, fan, &results);
        }
        // Results are final: stamp them onto this batch's window entries so
        // the next dispatch's doom check can tell repetition from polling.
        backfill_result_hashes(&mut self.call_sigs, &results);
        // The quiet-window snapshot (0035 decision 1): one detached enqueue
        // at batch end — between here and the next batch's first execute only
        // the user mutates the tree, so this capture doubles as the next
        // batch's pre-image. Skipped when nothing mutating actually ran.
        if mutation_signaled {
            if let Some(snapshots) = &self.shared.snapshots {
                snapshots.snapshot(format!("state after batch {}", self.samples));
            }
        }
        let cancelled = self.cancel.is_cancelled();
        let mut entries = vec![EntryPayload::Item {
            item: Item::ToolResults { results },
        }];
        entries.extend(
            self.subdir_hints(uses)
                .into_iter()
                .map(|item| EntryPayload::Item { item }),
        );
        // The tail the next head will carry if nothing intervenes — recorded
        // before `entries` moves into the proposal.
        self.projected_tail
            .extend(entries.iter().filter_map(|e| match e {
                EntryPayload::Item { item } => Some(Arc::new(item.clone())),
                _ => None,
            }));
        // `restamp`, not `stamp`: this sample already proposed its own
        // assistant+usage entry in `sample()` — the tool-results commit here
        // is the sample's REAL final propose, so it must overwrite that
        // earlier stamp rather than lose to first-stamp-wins (§S1 fix — see
        // `batch_proposed_and_watermark_durable_track_each_samples_own_final_commit`).
        self.ledger.restamp(Phase::BatchProposed);
        let commit = self
            .propose_pipelined(entries, crate::SampleStage::AtBoundary)
            .await;
        self.ledger.restamp(Phase::WatermarkDurable);
        if !commit.ok() {
            return Some(commit.outcome());
        }
        if cancelled {
            return Some(Outcome::Cancelled);
        }
        let outcome = budget_blown.map(|tool| Outcome::ToolFailureBudget { tool });
        if outcome.is_none() {
            // This is the sample boundary §Causal groups (b) names: the
            // group carrying the blocks and results is forwarded, its id is
            // in hand, and the next request can go out now — under no claim
            // — while that group is still reaching the disk.
            self.speculate();
        }
        outcome
    }

    /// The outcome-handling tail every concurrency wrapper in
    /// [`Turn::run_tool_batch`] runs once per already-executed call, in
    /// source order: evict an oversized result, score it against the
    /// failure budget, and build its `ToolResultItem`. Shared so the inline
    /// single-tool path (diet 1) and a `join_all`'d multi-tool chunk commit
    /// identical result shapes from identical `Executed` values — only how
    /// `gate`+`execute` got run (one await vs. concurrently) differs.
    async fn finish_call(
        &mut self,
        tu: &ToolUse,
        mut executed: Executed,
        budget_blown: &mut Option<String>,
    ) -> ToolResultItem {
        self.maybe_evict(tu, &mut executed.outcome).await;
        let (content, failed) = self.apply_failure_budget(tu, executed, budget_blown);
        ToolResultItem {
            tool_use_id: tu.id.clone(),
            content,
            is_error: failed,
        }
    }

    /// Track per-tool consecutive failures; warn once, at one failure left
    /// (a per-failure countdown reads as give-up pressure on every sample),
    /// and flag the budget when it hits zero.
    fn apply_failure_budget(
        &mut self,
        tu: &ToolUse,
        executed: Executed,
        budget_blown: &mut Option<String>,
    ) -> (String, bool) {
        let Executed {
            outcome,
            chargeable,
        } = executed;
        let mut content = outcome.content;
        if outcome.is_error && !chargeable {
            // Not a malfunction and not retryable: it neither draws down the
            // budget nor earns a hint contradicting its own message.
            // It must not *clear* the counter either — a denial cannot launder
            // a genuinely failing tool's streak (T3-1).
            return (content, true);
        }
        if outcome.is_error {
            let n = self
                .consecutive_failures
                .entry(tu.name.clone())
                .or_insert(0);
            *n += 1;
            let left = self.shared.config.tool_failure_budget.saturating_sub(*n);
            if left == 1 {
                content.push_str(
                    "\n<system-hint>this tool has failed repeatedly; one more \
                     consecutive failure ends the turn — try a different \
                     approach</system-hint>",
                );
            }
            // A parallel chunk may keep reporting after the first blow —
            // the outcome names the tool that blew the budget first.
            if left == 0 && budget_blown.is_none() {
                *budget_blown = Some(tu.name.clone());
            }
        } else {
            // Missing key == zero; keeps the map bounded to failing tools.
            self.consecutive_failures.remove(&tu.name);
        }
        (content, outcome.is_error)
    }

    /// PreToolUse hook (M5) → permission (allow-rules first, then the human).
    /// A hook `Deny` skips execution; a `Rewrite` swaps the input and
    /// re-enters the permission gate with it. Runs serially per call, before
    /// any execution in its chunk — the ask is a one-at-a-time human moment.
    /// (`&mut self` for the flag memo; the serial gating order is what makes
    /// that borrow free.)
    async fn gate(&mut self, tu: &ToolUse) -> Gate {
        let Some(tool) = self.shared.registry.get(&tu.name) else {
            // Chargeable: an invalid tool name is a model mistake the model can
            // fix, and repeating it is what the budget exists to stop.
            return Gate::Resolved {
                outcome: unknown_tool(&self.tool_defs, &tu.name),
                chargeable: true,
            };
        };
        // PreToolUse: a wrap-style intercept may block or rewrite the call.
        // §S1 HookRouter gate: masked-off (or hook-less) sessions skip the
        // cap copy, the call, and the post-await cancel recheck entirely.
        let mut input = tu.input.clone();
        crate::hooks::hook_gate!(
            self.shared.hooks,
            self.shared.hook_mask(),
            crate::hooks::EventMask::PRE_TOOL,
            |hooks| {
                let view = crate::hooks::cap_tool_input(&input);
                match crate::hooks::call_pre_tool(hooks, &tu.name, &view, &self.cancel).await {
                    crate::hooks::PreToolDecision::Continue => {}
                    crate::hooks::PreToolDecision::Deny { message } => {
                        self.emit(EngineEvent::ToolDenied {
                            id: tu.id.clone(),
                            name: tu.name.clone(),
                        })
                        .await;
                        return Gate::Resolved {
                            outcome: ToolOutcome::err(format!(
                                "A hook blocked this tool call: {message}"
                            )),
                            chargeable: false,
                        };
                    }
                    crate::hooks::PreToolDecision::Rewrite { input: rewritten } => {
                        input = crate::hooks::restore_capped(&input, rewritten)
                    }
                }
                // The hook may have been abandoned by an interrupt rather
                // than answered; a cancelled turn executes nothing further.
                if self.cancel.is_cancelled() {
                    return Gate::Resolved {
                        outcome: ToolOutcome::err("Not executed (turn stopped)."),
                        chargeable: false,
                    };
                }
            },
            else {}
        );
        let (summary, why, class) = match tool.permission(&input) {
            Permission::None => (None, None, None),
            Permission::Ask { summary } => (Some(summary), None, None),
            Permission::AskProtected {
                summary,
                why,
                class,
            } => (Some(summary), Some(why), Some(class)),
        };
        // A gate-free tool has no permission summary; let it name the call
        // (`skill <name>`) so its card is not a bare, identical `skill`.
        let display = hotl_types::sanitize::safe_summary(
            &summary
                .clone()
                .or_else(|| tool.display_summary(&input))
                .unwrap_or_else(|| tu.name.clone()),
        );
        let mut secret_reads = false;
        let mut shown = None;
        if let Some(summary) = summary {
            let Approval { reply, by_human } =
                self.approve_input(tu, &input, summary, why, class).await;
            // Plan 0026: register the hosts the human actually read, and only
            // those. `display` is `safe_summary(summary)` — byte-for-byte what
            // `Turn::ask` renders — so the invariant is checkable rather than
            // asserted: hotl skips the egress ask only for hosts that were on
            // screen when the human said yes.
            //
            // `by_human` is *reported* by `approve_input`, never inferred:
            // `Verdict::Auto` and `Verdict::Ask` both return `AskReply::Allow`,
            // and anything that reconstructs "was there a human?" downstream is
            // the bug that turns every allow-rule into a standing egress grant.
            //
            // `AllowEdited` registers nothing: the human edited the input after
            // reading the summary, so the summary is no longer evidence of what
            // is about to run.
            if by_human && matches!(reply, AskReply::Allow | AskReply::AllowWithSecretReads) {
                shown = Some(hotl_tools::net::show_hosts(
                    hotl_tools::net::hosts_shown_in(&display),
                ));
            }
            match reply {
                AskReply::Allow => {}
                // Plan 0022: approved *and* the credential read-deny lifted,
                // for this call only. Carried on the Gate rather than in the
                // tool input — the model writes the input, so a grant living
                // there would be self-granted.
                AskReply::AllowWithSecretReads => secret_reads = true,
                AskReply::AllowEdited { input: edited } => input = edited, // §2b
                AskReply::Respond { content } => {
                    // §2b: the human answered as the tool — skip execution.
                    self.emit(EngineEvent::ToolDone {
                        id: tu.id.clone(),
                        name: tu.name.clone(),
                        ok: true,
                    })
                    .await;
                    return Gate::Resolved {
                        outcome: ToolOutcome::ok(content),
                        chargeable: true,
                    };
                }
                AskReply::Deny { message } => {
                    self.emit(EngineEvent::ToolDenied {
                        id: tu.id.clone(),
                        name: tu.name.clone(),
                    })
                    .await;
                    return Gate::Resolved {
                        outcome: match message {
                            Some(m) => ToolOutcome::err(format!("The user declined this tool call: {m}")),
                            None => ToolOutcome::err(
                                "The user declined this tool call. Ask what they'd like to do instead, or proceed another way.",
                            ),
                        },
                        chargeable: false,
                    };
                }
            }
        } else if let Some(rule) = self.shared.rules.denied(&tu.name, &input) {
            // Vuln 6: a `Permission::None` tool (read/glob/grep in-workspace)
            // skips the ask path above, but a `[[deny]]` on it must still
            // refuse — deny is a "never" the gate owes regardless of how
            // low-risk the tool's own permission is.
            self.emit(EngineEvent::ToolDenied {
                id: tu.id.clone(),
                name: tu.name.clone(),
            })
            .await;
            return Gate::Resolved {
                outcome: ToolOutcome::err(format!(
                    "a deny rule refused this call ({rule}); do not retry it"
                )),
                chargeable: false,
            };
        }
        Gate::Ready {
            input,
            summary: display,
            secret_reads,
            shown,
        }
    }

    /// Execute an approved call: ToolStart → run → PostToolUse hook →
    /// ToolDone. `&self` only, so approved parallel-safe calls in one chunk
    /// can run concurrently; a call the gate already resolved passes through.
    async fn execute(&self, tu: &ToolUse, gate: Gate) -> Executed {
        // Exhaustive by construction: a new `Gate` variant is a compile error
        // here, not a runtime panic on a supervised task (T1-5).
        // `_shown` is held, never read: dropping it at the end of `execute` is
        // the whole lifetime management for the 0026 shown-hosts rule, and the
        // egress ask reads the registry it feeds, not this binding.
        let (input, summary, secret_reads, _shown) = match gate {
            Gate::Ready {
                input,
                summary,
                secret_reads,
                shown,
            } => (input, summary, secret_reads, shown),
            Gate::Resolved {
                outcome,
                chargeable,
            } => {
                return Executed {
                    outcome,
                    chargeable,
                }
            }
        };
        self.emit(EngineEvent::ToolStart {
            id: tu.id.clone(),
            name: tu.name.clone(),
            summary,
        })
        .await;
        let Some(tool) = self.shared.registry.get(&tu.name) else {
            // Gate checked; defensive. Chargeable for the same reason the gate's
            // own `unknown_tool` is: a bad name is a model mistake it can fix.
            return Executed {
                outcome: unknown_tool(&self.tool_defs, &tu.name),
                chargeable: true,
            };
        };
        // Plan 0022: the grant lives in a task-local scoped around exactly
        // this future, so `sandbox::build_command` sees it for this spawn and
        // no other. A later call in the same turn re-enters with `false`.
        // 0039: the call id rides the same shape, so `spawn` can stamp
        // forwarded child events with its own card's id.
        let mut outcome = hotl_tools::CURRENT_CALL_ID
            .scope(
                tu.id.clone(),
                hotl_tools::sandbox::SECRET_READS
                    .scope(secret_reads, tool.run(input, self.cancel.clone())),
            )
            .await;
        // PostToolUse: a node-style proposal may replace a successful result.
        // §S1 HookRouter gate: a masked-off (or hook-less) session never
        // reaches `call_post_tool` — no cap copy, no timeout/cancel race.
        if !outcome.is_error {
            crate::hooks::hook_gate!(
                self.shared.hooks,
                self.shared.hook_mask(),
                crate::hooks::EventMask::POST_TOOL,
                |hooks| {
                    if let Some(replacement) = crate::hooks::call_post_tool(
                        hooks,
                        &tu.name,
                        &outcome.content,
                        &self.cancel,
                    )
                    .await
                    {
                        outcome.content = replacement;
                    }
                },
                else {}
            );
        }
        self.emit(EngineEvent::ToolDone {
            id: tu.id.clone(),
            name: tu.name.clone(),
            ok: !outcome.is_error,
        })
        .await;
        // A tool that actually ran and failed is exactly what the budget is for.
        Executed {
            outcome,
            chargeable: true,
        }
    }

    /// Allow-rules (deny-first, sandbox-gated, protected carve-out) or the
    /// ask, evaluated against `input` (which a PreToolUse hook may have
    /// rewritten — a rewritten call re-enters the gate, never bypasses it).
    async fn approve_input(
        &mut self,
        tu: &ToolUse,
        input: &Value,
        summary: String,
        why: Option<String>,
        class: Option<hotl_tools::ProtectedClass>,
    ) -> Approval {
        let tool = self.shared.registry.get(&tu.name);
        let facts = hotl_tools::rules::CallFacts {
            sandbox_enforced: self.shared.sandbox_enforced,
            protected: class,
            read_only: tool.as_ref().is_some_and(|t| t.read_only()),
            edits_files: tool.as_ref().is_some_and(|t| t.edits_files()),
        };
        match self.shared.rules.evaluate(
            self.shared.effective_mode(),
            self.shared.effective_plan(),
            &tu.name,
            input,
            facts,
        ) {
            Verdict::Auto { rule } => {
                self.emit(EngineEvent::ToolAutoAllowed {
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                    rule,
                })
                .await;
                Approval::by_rule(AskReply::Allow)
            }
            Verdict::Deny { rule } => Approval::by_rule(AskReply::Deny {
                message: Some(format!(
                    "a deny rule refused this call ({rule}); do not retry it"
                )),
            }),
            // The bypass floor's two non-blocking answers (0036): the gate
            // resolves the call itself and `ToolFlagged` is the notification
            // that replaces the ask. `why` never is None for a protected
            // class; the summary is the fallback, not an unwrap. The notice
            // is coalesced per turn (0037 D5) — six page reads of one outside
            // file are one boundary decision, not six ⚑ lines — while the
            // evaluation and the resolution stay per-call.
            Verdict::AutoFlagged { rule: _ } => {
                let why = why.unwrap_or_else(|| summary.clone());
                if self.first_flag(tu, input, class, false) {
                    self.emit(EngineEvent::ToolFlagged {
                        id: tu.id.clone(),
                        name: tu.name.clone(),
                        summary,
                        why,
                        denied: false,
                    })
                    .await;
                }
                Approval::by_rule(AskReply::Allow)
            }
            Verdict::DenyFlagged { rule } => {
                let why = why.unwrap_or_else(|| summary.clone());
                if self.first_flag(tu, input, class, true) {
                    self.emit(EngineEvent::ToolFlagged {
                        id: tu.id.clone(),
                        name: tu.name.clone(),
                        summary,
                        why: why.clone(),
                        denied: true,
                    })
                    .await;
                }
                Approval::by_rule(AskReply::Deny {
                    message: Some(format!(
                        "refused without prompting ({rule}): {why}. The session boundary is \
                         the working directory; do not retry — work inside it, or tell the \
                         user what you need and why so they can grant it (an allow rule, a \
                         [sandbox].writable entry, or ask mode)."
                    )),
                })
            }
            // The only path through `Turn::ask`, and therefore the only path
            // on which a human read the summary.
            Verdict::Ask => Approval {
                reply: self.ask(summary, why).await,
                by_human: true,
            },
        }
    }

    /// The per-turn flag-notice memo (0037 D5): true exactly once per
    /// distinct flagged decision per prompt. Carried across compaction folds
    /// (one logical prompt), reset by a new prompt — the human may have
    /// looked away, so nothing stays buried across a session. Display-only:
    /// the caller resolves the call identically whether or not this fires.
    fn first_flag(
        &mut self,
        tu: &ToolUse,
        input: &Value,
        class: Option<hotl_tools::ProtectedClass>,
        denied: bool,
    ) -> bool {
        self.flagged.insert(FlagKey::new(tu, input, class, denied))
    }

    /// Complete protocol pairing for a batch that will not execute.
    async fn abort_batch(&mut self, uses: &[ToolUse], message: &str) -> Commit {
        let results = uses.iter().map(|tu| pair(tu, message, true)).collect();
        // Same reasoning as `run_tool_batch`'s propose (§S1 fix): this is
        // the doom-loop-terminated sample's real final commit, so it must
        // overwrite `sample()`'s earlier stamp via `restamp`.
        self.ledger.restamp(Phase::BatchProposed);
        let commit = self
            .propose_pipelined(
                vec![EntryPayload::Item {
                    item: Item::ToolResults { results },
                }],
                crate::SampleStage::AtBoundary,
            )
            .await;
        self.ledger.restamp(Phase::WatermarkDurable);
        commit
    }

    /// Pre-flight: the compaction threshold check (M2), then the request with
    /// the MOIM turn-context attached. Past the speculation threshold the
    /// digest summarize starts here, overlapping the samples still to come.
    fn build_request(
        &mut self,
        snapshot: &crate::actor::Snapshot,
    ) -> Result<SamplingRequest, SampleEnd> {
        let window = self.shared.config.context_window.max(1);
        let estimate = self.estimate_tokens(snapshot);
        let image_bytes = hotl_context::tokens::image_b64_bytes(&snapshot.durable);
        if must_compact(estimate, window, image_bytes) {
            return Err(SampleEnd::ContextFull);
        }
        self.maybe_speculate_digest(snapshot, estimate, image_bytes);
        Ok(self.compose_request(snapshot, estimate, self.samples))
    }

    /// Fire the speculative compaction digest once the estimate crosses
    /// [`SPECULATE_TRIGGER`] (M2). Called once per sample on **both** paths:
    /// an adopted optimistic dispatch never reaches `build_request`, and the
    /// fold must still get its head start.
    fn maybe_speculate_digest(
        &mut self,
        snapshot: &crate::actor::Snapshot,
        estimate: u64,
        image_bytes: usize,
    ) {
        let window = self.shared.config.context_window.max(1);
        if self.speculation.is_none()
            && !self.shared.config.compaction_reset
            && should_speculate(estimate, window, image_bytes)
        {
            // Durable only, deliberately: `actor::compact` plans and applies
            // against `head.items()`, so a speculative plan's `prefix_end` /
            // `kept_from` are only the same indices if it was planned against
            // the same list. (Before the split they were not — the reminder
            // rode the snapshot and could itself be chosen as `kept_from`.)
            self.speculation = spawn_speculation(&self.shared, &snapshot.durable, &self.cancel);
        }
    }

    /// The speculative twin of [`Turn::build_request`]: the same compose,
    /// against the *predicted* snapshot and the sample number that snapshot
    /// will belong to. `None` when the predicted request would cross the
    /// compaction trigger — the next sample is going to fold rather than
    /// sample, and a speculative call there would be pure waste (and, being
    /// a real billed call, waste of the kind §The side-effect ruling makes
    /// us account for).
    ///
    /// Deliberately does NOT fire the speculative *compaction* digest: that
    /// is `build_request`'s, once, on the sequential path.
    fn speculative_request(&self, snapshot: &crate::actor::Snapshot) -> Option<SamplingRequest> {
        let window = self.shared.config.context_window.max(1);
        let estimate = self.estimate_tokens(snapshot);
        if must_compact(
            estimate,
            window,
            hotl_context::tokens::image_b64_bytes(&snapshot.durable),
        ) {
            return None;
        }
        Some(self.compose_request(snapshot, estimate, self.samples + 1))
    }

    /// Build the request itself. Everything here is a pure function of
    /// `snapshot`, `estimate` and `sample_no` — which is what makes the
    /// adopted request byte-identical to the sequential rebuild under leaf
    /// equality (spec case 9). The one exception is the MOIM turn-context
    /// block, whose timestamp is read at build time: it is the sanctioned
    /// ephemeral suffix, regenerated on both paths and never persisted.
    ///
    /// The snapshot's two channels stay two channels all the way to the wire:
    /// `durable` is the only thing a provider may mark, `tail` is serialized
    /// after every marker. This is also where `EngineConfig::cache_static`
    /// (still a bool this phase) becomes a [`CachePolicy`], carrying
    /// `EngineConfig::cache_ttl` straight through as `prefix_ttl` — the
    /// serializer, not this layer, decides which markers actually honor it.
    fn compose_request(
        &self,
        snapshot: &crate::actor::Snapshot,
        estimate: u64,
        sample_no: u32,
    ) -> SamplingRequest {
        let window = self.shared.config.context_window.max(1);
        let used_pct = self
            .shared
            .config
            .show_context_pct
            .then(|| (estimate.saturating_mul(100) / window).min(100) as u8);
        let turn_context = hotl_context::turn_context(
            self.shared.clock.now_ms(),
            &self.shared.cwd,
            used_pct,
            sample_no,
        );
        SamplingRequest {
            model: self.models[self.model_idx].clone(),
            max_tokens: self.shared.config.max_tokens,
            system: Arc::clone(&self.shared.system),
            items: Arc::clone(&snapshot.durable),
            ephemeral_tail: Arc::clone(&snapshot.tail),
            tools: Arc::clone(&self.tool_defs),
            thinking: self.shared.config.thinking,
            effort: self.shared.effective_effort(),
            cache: if self.shared.config.cache_static {
                CachePolicy::Static {
                    prefix_ttl: self.shared.config.cache_ttl,
                }
            } else {
                CachePolicy::Off
            },
            turn_context: Some(turn_context),
        }
    }

    /// Drain the provider stream (cancel-biased), forwarding deltas. Takes
    /// the stream rather than the request because an adopted speculative
    /// sample arrives as a stream that is already part-consumed — the
    /// buffered prefix chained onto the rest (§Causal groups (b)).
    async fn collect_stream(
        &mut self,
        stream: BoxStream<'static, Result<StreamEvent, ProviderError>>,
    ) -> Result<(StopReason, TokenUsage, Vec<Value>), SampleEnd> {
        let mut stream = stream;
        let mut completed = None;
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(SampleEnd::Cancelled),
                next = stream.next() => match next {
                    Some(Ok(event)) => {
                        self.ledger.stamp(Phase::FirstByte);
                        if let StreamEvent::Completed { stop, usage, blocks } = event {
                            self.ledger.stamp(Phase::LastBlockEnd);
                            completed = Some((stop, usage, blocks));
                        } else {
                            self.forward(event).await;
                        }
                    }
                    // An error event is terminal for the provider's own
                    // generator, so draining to end-of-stream here costs one
                    // more poll — and it is what tells a stream it was
                    // consumed rather than abandoned mid-flight.
                    Some(Err(e)) if retry::is_context_overflow(&e) => {
                        drain_to_end(&mut stream).await;
                        return Err(SampleEnd::ContextFull);
                    }
                    Some(Err(e)) if retry::is_availability(&e) => {
                        drain_to_end(&mut stream).await;
                        return Err(SampleEnd::Unavailable(e.to_string()));
                    }
                    Some(Err(e)) => {
                        drain_to_end(&mut stream).await;
                        return Err(SampleEnd::Fatal(e.to_string()));
                    }
                    None => break,
                }
            }
        }
        completed.ok_or_else(|| SampleEnd::Fatal("stream ended without completion".into()))
    }

    /// Anchored context estimate for the next request: the provider-reported
    /// cost of the last sample plus the (overcounting) estimate of everything
    /// appended since, plus the ephemeral tail.
    ///
    /// The tail is a visible, named term rather than something riding inside
    /// the anchored slice — but it is the *same* term: `estimate_items` is a
    /// sum over items, so `durable[len..] ++ tail` and `durable[len..]` plus
    /// `tail` are equal by construction. The totals are identical to what a
    /// flat snapshot produced.
    fn estimate_tokens(&self, snapshot: &crate::actor::Snapshot) -> u64 {
        anchored_estimate(
            self.anchor,
            self.shared.system_estimate,
            &snapshot.durable,
            snapshot.durable_estimate,
        ) + hotl_context::tokens::estimate_items(&snapshot.tail)
    }

    /// Just-in-time nested AGENTS.md injection (M2), deduped per turn and
    /// against the projection (hints from earlier turns are in the snapshot).
    fn subdir_hints(&mut self, uses: &[ToolUse]) -> Vec<Item> {
        let mut out = Vec::new();
        for tu in uses {
            let Some(path) = tu.input.get("path").and_then(Value::as_str) else {
                continue;
            };
            let Some((marker, item)) =
                hotl_context::nested_instructions(&self.shared.cwd, Path::new(path))
            else {
                continue;
            };
            if self.injected_hints.contains(&marker) || self.in_projection(&marker) {
                continue;
            }
            self.injected_hints.insert(marker);
            out.push(item);
        }
        out
    }

    fn in_projection(&self, marker: &str) -> bool {
        let Some(snapshot) = &self.last_snapshot else {
            return false;
        };
        // Durable only: a hint is deduped against what is actually committed,
        // and nothing ephemeral is ever a `SubdirInstructions` item anyway.
        snapshot.durable.iter().any(|i| {
            matches!(
                &**i,
                Item::User { text, synthetic: Some(hotl_types::SyntheticReason::SubdirInstructions), .. }
                    if text.contains(marker)
            )
        })
    }

    /// The per-batch taint signal (0035 decision 7): a `Gate::Resolved` call
    /// never executes, so it cannot mutate; `Tool::read_only()` is the
    /// tool's own answer, and an unknown tool counts as mutating — the
    /// conservative direction, deliberately (T3-2). Sync and O(1): the
    /// snapshotter only bumps a generation counter.
    fn signal_mutation(&self, tu: &ToolUse, gate: &Gate, signaled: &mut bool) {
        if *signaled || matches!(gate, Gate::Resolved { .. }) {
            return;
        }
        if self
            .shared
            .registry
            .get(&tu.name)
            .is_some_and(|t| t.read_only())
        {
            return;
        }
        if let Some(snapshots) = &self.shared.snapshots {
            snapshots.mutation_started();
        }
        *signaled = true;
    }

    /// Evict an oversized *successful* tool result to a masked blob (T4),
    /// replacing it in-context with a head preview + a read-it pointer. The
    /// deliberate 3-chars/token overcount (M2) evicts a bit early. A failed
    /// blob write leaves the full content in place — eviction is an
    /// optimization, never a data-loss path.
    async fn maybe_evict(&self, tu: &ToolUse, outcome: &mut ToolOutcome) {
        let threshold = self.shared.config.evict_threshold_tokens;
        if threshold == 0 || outcome.is_error {
            return;
        }
        if hotl_context::tokens::estimate_text(&outcome.content) <= threshold {
            return;
        }
        // Cut the preview before the content moves; on any failure the
        // content comes back — eviction is never a data-loss path.
        let content = std::mem::take(&mut outcome.content);
        let total = content.len();
        let head = clip(&content, 2048).to_string();
        let (tx, rx) = oneshot::channel();
        let cmd = SessionCmd::WriteBlob {
            tool_use_id: tu.id.clone(),
            content,
            reply: tx,
        };
        if let Err(mpsc::error::SendError(cmd)) = self.cmd_tx.send(cmd).await {
            if let SessionCmd::WriteBlob { content, .. } = cmd {
                outcome.content = content;
            }
            return;
        }
        match rx.await {
            Ok(Ok(path)) => {
                outcome.content = format!(
                    "{head}\n<evicted total_bytes={total} file=\"{path}\">Full output saved. \
                     Read it with the read tool ({path}); use offset to page.</evicted>"
                );
            }
            // Blob write failed: the actor handed the content back.
            Ok(Err(content)) => outcome.content = content,
            // Actor gone mid-write (session closing): keep the preview.
            Err(_) => outcome.content = head,
        }
    }

    /// The speculative digest, if one was fired and finished in time.
    async fn take_speculation(&mut self) -> Option<crate::SpecDigest> {
        let handle = self.speculation.take()?;
        await_speculation(handle, &self.cancel, SPECULATION_WAIT).await
    }

    /// The sample boundary: barrier (b)'s ticket drain, then the refresh —
    /// with an un-adopted speculative stream pulled **concurrently**. That
    /// overlap is what §Causal groups (b) buys: the boundary group's fsync
    /// and the next sample's first bytes are in flight at the same time.
    ///
    /// The refresh is what the boundary waits on, never the stream: a
    /// stalled provider must not wedge a turn, so filling the buffer only
    /// ever gets the time the refresh takes.
    async fn boundary(&mut self) -> (Commit, Option<Arc<crate::actor::ProjectionHead>>) {
        // Disjoint field borrows, so the two futures below can run at once.
        let Turn {
            pipeline,
            speculative,
            head,
            ..
        } = self;
        let refresh = async {
            // Barrier (b) (commit-protocol.md §Pipelined commits): what the
            // next sample is built from must be a projection that already
            // contains everything this turn committed. The drain IS the
            // barrier — a ticket is the only place a sealed log is reported,
            // so waiting on the published head alone would wait forever on
            // one — and the watch wait below FOLLOWS it, never replaces it.
            let commit = pipeline.drain().await;
            // The wait FOLLOWS the drain and never substitutes for it: on a
            // sealed log the published head stops advancing at precisely the
            // moment the news is sitting in an unread ticket, so a turn that
            // waited here would wait forever (§Read invariant).
            if !commit.ok() {
                return (commit, None);
            }
            let head = match pipeline.ack_seq() {
                Some(my_ack_seq) => head
                    .wait_for(|head| head.epoch() >= my_ack_seq)
                    .await
                    .ok()
                    .map(|head| Arc::clone(&head)),
                // Nothing committed yet this turn (its first sample):
                // whatever the actor last published already contains the
                // prompt that spawned this turn — appended, applied and
                // published before the spawn.
                None => Some(Arc::clone(&head.borrow())),
            };
            (commit, head)
        };
        let mut refresh = std::pin::pin!(refresh);
        match speculative.as_mut() {
            Some(spec) => tokio::select! {
                refreshed = &mut refresh => refreshed,
                // The buffer filled first: stop overlapping and finish the
                // refresh, which is the only thing adoption may be gated on.
                _ = spec.fill() => refresh.await,
            },
            None => refresh.await,
        }
    }

    /// Optimistic next-sample dispatch (commit-protocol.md §Causal groups
    /// (b)): the boundary group is forwarded and its id is already known, so
    /// build the next request from the blocks and results in hand and send
    /// it — un-adopted, under no claim at all. The refresh at the next
    /// boundary decides whether it counts.
    ///
    /// Only ever called where the turn is about to sample again, and only
    /// under `AckMode::Pipelined` (a `Sync` proposal issues no ticket, so
    /// there is no `expected_leaf` to adopt against — which is also what
    /// keeps the `Sync` comparison run speculation-free).
    fn speculate(&mut self) {
        // A turn that has spent its step budget ends at the top of the next
        // `drive` iteration without sampling, so a dispatch here would be a
        // billed call for a sample that never happens. Mirrors `drive`'s own
        // loop condition, negative = unlimited included.
        let bound = self.shared.config.max_turns;
        let will_sample_again = bound < 0 || self.spent < bound;
        if self.cancel.is_cancelled() || self.speculative.is_some() || !will_sample_again {
            return;
        }
        let Some(predicted) = self.predicted_snapshot() else {
            return;
        };
        self.dispatch_speculative(predicted);
    }

    /// 0033 Task 10: the admission handoff. Park the prompt's ticket (the
    /// window is empty — never blocks) and dispatch sample 1 speculatively;
    /// the first `boundary()` then does everything else unmodified — waits
    /// this turn's own `ack_seq`, refreshes the head (which applied the
    /// prompt on ack), and the existing leaf-equality filter in `sample()`
    /// adopts or drops.
    async fn admit(&mut self, admission: crate::Admission) {
        let _ = self.pipeline.submit(admission.ticket).await;
        self.dispatch_speculative(admission.predicted);
    }

    /// The shared dispatch tail of [`Turn::speculate`] and the admission
    /// path: build the predicted request and put it on the wire un-adopted.
    /// `expected_leaf` is the pipeline's newest committed unit in both —
    /// at admission, that names the prompt entry itself.
    fn dispatch_speculative(&mut self, predicted: crate::actor::Snapshot) {
        let Some(expected_leaf) = self.pipeline.leaf().map(str::to_string) else {
            return;
        };
        let Some(request) = self.speculative_request(&predicted) else {
            return;
        };
        let stream = self.shared.provider.stream(request);
        self.speculative = Some(Speculation {
            expected_leaf,
            stream: SyncStream::new(stream),
            buffered: Vec::new(),
            terminal: false,
        });
    }

    /// What the next head will contain if nothing intervenes: the durable
    /// projection this sample was built from, plus everything this turn
    /// committed since. Under leaf equality this is not a guess: no other
    /// entry can have landed, so it is the same list the sequential rebuild
    /// would read.
    ///
    /// The ephemeral tail is *carried*, not re-derived: todos can only have
    /// changed by way of a durable `Todos` entry, which would move the leaf
    /// and refuse the adoption. With the two channels separate there is no
    /// splice to undo and redo — the prediction is an append, which is what
    /// the projection is.
    fn predicted_snapshot(&self) -> Option<crate::actor::Snapshot> {
        let last = self.last_snapshot.as_ref()?;
        let mut durable = Vec::with_capacity(last.durable.len() + self.projected_tail.len());
        durable.extend(last.durable.iter().cloned());
        durable.extend(self.projected_tail.iter().cloned());
        Some(crate::actor::Snapshot {
            durable: Arc::new(durable),
            // A handful of items at most — summed on demand, not tracked.
            durable_estimate: last.durable_estimate
                + self
                    .projected_tail
                    .iter()
                    .map(|i| hotl_context::tokens::estimate_item(i))
                    .sum::<u64>(),
            tail: Arc::clone(&last.tail),
        })
    }

    /// Serialize + mask every entry (commit-protocol.md §Proposal payloads)
    /// and commit them via [`SessionCmd::ProposePrepared`] — the type-level
    /// single-serialization-site enforcement point (task 8 requirement 4):
    /// this is the ONLY place a `Turn` reaches the log, and it can only
    /// reach it with `PreparedPayload`, never a raw `EntryPayload`.
    async fn propose(&self, entries: Vec<EntryPayload>, stage: crate::SampleStage) -> Commit {
        let epoch = self.shared.rules_epoch();
        let commit = self.propose_at_epoch(&entries, epoch, stage).await;
        if commit != Commit::StaleEpoch {
            return commit;
        }
        // Rare (today unreachable in production — `rules_epoch` is constant
        // for the life of a session): the masking rules' epoch advanced
        // between build and send. Re-mask under the now-current epoch and
        // resend once (commit-protocol.md: "the turn re-masks and
        // re-proposes ... rare, correct"). `entries` was only ever borrowed
        // by `propose_at_epoch`, so this costs nothing on the common path.
        let epoch = self.shared.rules_epoch();
        self.propose_at_epoch(&entries, epoch, stage).await
    }

    async fn propose_at_epoch(
        &self,
        entries: &[EntryPayload],
        epoch: u32,
        stage: crate::SampleStage,
    ) -> Commit {
        match self.send(entries, epoch, crate::AckMode::Sync, stage).await {
            Ok(reply) => Commit::from_reply(Some(reply)),
            Err(commit) => commit,
        }
    }

    /// [`Turn::propose`]'s pipelined twin (commit-protocol.md §Pipelined
    /// commits): the actor answers with a ticket the moment it forwards, and
    /// the turn goes back to work. Durability is settled at the next barrier
    /// — which is where a sealed log is reported, at most [`ACK_WINDOW`]
    /// commits later.
    ///
    /// Only proposals the protocol lets run ahead reach here: the
    /// `Completed`-boundary assistant+usage `Group` (S2c), the tool-results
    /// batch entry (plus any subdir-instruction entries riding with it) and
    /// the Done-branch gate nudge. The ask journal stays `Sync`, because §2b
    /// requires the ask to be durable *before* it surfaces.
    ///
    /// `stage` is the proposer's declaration that its sample had closed —
    /// what the actor's held-steer release checks (see
    /// [`crate::SampleStage`]). Every site here passes `AtBoundary` today;
    /// a future intra-sample commit must pass `InSample` and will then trip
    /// that assertion rather than silently invert a held steer.
    async fn propose_pipelined(
        &mut self,
        entries: Vec<EntryPayload>,
        stage: crate::SampleStage,
    ) -> Commit {
        if self.shared.config.ack_mode == crate::AckMode::Sync {
            return self.propose(entries, stage).await;
        }
        let epoch = self.shared.rules_epoch();
        let commit = self.submit_pipelined(&entries, epoch, stage).await;
        if commit != Commit::StaleEpoch {
            return commit;
        }
        let epoch = self.shared.rules_epoch();
        self.submit_pipelined(&entries, epoch, stage).await
    }

    async fn submit_pipelined(
        &mut self,
        entries: &[EntryPayload],
        epoch: u32,
        stage: crate::SampleStage,
    ) -> Commit {
        match self
            .send(entries, epoch, crate::AckMode::Pipelined, stage)
            .await
        {
            Ok(crate::ProposeReply::Ticket(ticket)) => self.pipeline.submit(ticket).await,
            Ok(other) => Commit::from_reply(Some(other)),
            Err(commit) => commit,
        }
    }

    /// Prepare every entry and hand the proposal to the actor. `Err` is a
    /// build or send failure, never the actor's own answer.
    async fn send(
        &self,
        entries: &[EntryPayload],
        epoch: u32,
        mode: crate::AckMode,
        stage: crate::SampleStage,
    ) -> Result<crate::ProposeReply, Commit> {
        let masker = self.shared.masker();
        let mut prepared = Vec::with_capacity(entries.len());
        for payload in entries {
            match prepare_entry(payload, masker, epoch).await {
                Ok(entry) => prepared.push(entry),
                Err(PrepareError::Serialize) => {
                    // Not expected — see `PrepareError::Serialize`'s doc —
                    // so surface it loudly in dev/test builds rather than
                    // just silently dropping the entry and reporting a
                    // generic shutdown to the user.
                    debug_assert!(
                        false,
                        "EntryPayload serialization must not fail for hotl's own payload shapes"
                    );
                    return Err(Commit::Gone);
                }
                // `Offload` CAN legitimately happen on a real shutdown race
                // (the spawn_blocking task got aborted) — not asserted.
                Err(PrepareError::Offload) => return Err(Commit::Gone),
            }
        }
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCmd::ProposePrepared {
                proposal: crate::EntryProposal::of(prepared),
                stage,
                mode,
                reply: tx,
            })
            .await
            .is_err()
        {
            return Err(Commit::Gone);
        }
        rx.await.map_err(|_| Commit::Gone)
    }

    /// Ask the human via the event channel; a dropped reply means deny.
    /// The ask is committed durably *before* it surfaces (§2b `pending_ask`)
    /// and its resolution *after* (`ask_resolved`) — so a process that dies
    /// mid-ask leaves a dangling record that resume re-surfaces. The log
    /// records are best-effort: a sealed log never blocks the ask itself.
    async fn ask(&self, summary: String, why: Option<String>) -> AskReply {
        // Chokepoint (Vuln 8): `summary`/`why` are model-influenced text about
        // to be rendered into the human's y/N prompt. Flatten each to a single
        // control-free line here — once, for every tool — so none can smuggle a
        // terminal escape or a line-erase into the approval the human acts on.
        let summary = hotl_types::sanitize::safe_summary(&summary);
        let why = why.map(|w| hotl_types::sanitize::safe_summary(&w));
        let id = hotl_types::new_ulid();
        // INVARIANT: the ask itself never blocks on the log — a sealed log
        // degrades the audit record, never the human moment. Enforced by
        // construction (both results are deliberately discarded).
        let _ = self
            .propose(
                vec![EntryPayload::PendingAsk {
                    id: id.clone(),
                    summary: summary.clone(),
                    protected_why: why.clone(),
                }],
                // An ask only ever fires from inside a tool phase, i.e.
                // after the sample that requested the call has closed.
                crate::SampleStage::AtBoundary,
            )
            .await;
        // Notification (tier-1 gap #7, the `hotl watch`/desktop seam): the
        // agent is blocked on a human, right before the ask actually
        // surfaces. Fire-and-forget — never awaited on this hot path.
        // §S1 HookRouter gate: a masked-off (or hook-less) session skips
        // even the `summary.clone()` this would otherwise pay for.
        crate::hooks::hook_gate!(
            self.shared.hooks,
            self.shared.hook_mask(),
            crate::hooks::EventMask::NOTIFICATION,
            |hooks| {
                crate::hooks::notify(
                    hooks,
                    &self.shared.notifications,
                    crate::hooks::NotificationKind::Blocked,
                    summary.clone(),
                );
            },
            else {}
        );
        let (tx, rx) = oneshot::channel();
        let event = EngineEvent::Ask {
            summary,
            protected_why: why,
            reply: tx,
        };
        let reply = if self.events.send(event).await.is_err() {
            AskReply::Deny { message: None }
        } else {
            // Race the human's reply against the interrupt token: an ask must
            // never pin a turn the user has already cancelled (the batch loop
            // sees the cancellation at the next chunk boundary and ends).
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => AskReply::Deny {
                    message: Some("the user interrupted the turn".into()),
                },
                reply = rx => reply.unwrap_or(AskReply::Deny { message: None }),
            }
        };
        let allowed = matches!(
            reply,
            AskReply::Allow
                | AskReply::AllowWithSecretReads
                | AskReply::AllowEdited { .. }
                | AskReply::Respond { .. }
        );
        let _ = self
            .propose(
                vec![EntryPayload::AskResolved { id, allowed }],
                crate::SampleStage::AtBoundary,
            )
            .await;
        reply
    }

    async fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event).await;
    }

    /// Forward a provider delta as a surface event.
    async fn forward(&self, event: StreamEvent) {
        let mapped = match event {
            StreamEvent::TextDelta { text, .. } => EngineEvent::TextDelta(text),
            StreamEvent::ThinkingDelta { text, .. } => EngineEvent::ThinkingDelta(text),
            StreamEvent::Retrying { attempt, reason } => EngineEvent::Retrying { attempt, reason },
            _ => return,
        };
        self.emit(mapped).await;
    }
}

/// Poll a stream that has already reported its terminal error to its end,
/// bounded so a misbehaving provider cannot turn "finish the stream" into a
/// second unbounded await.
pub(crate) async fn drain_to_end(
    stream: &mut BoxStream<'static, Result<StreamEvent, ProviderError>>,
) {
    for _ in 0..DRAIN_MAX {
        if stream.next().await.is_none() {
            return;
        }
    }
}

/// Events [`drain_to_end`] will poll past a terminal error before giving up.
const DRAIN_MAX: usize = 64;

/// Anchored context estimate for the next request: the provider-reported cost
/// of the last sample plus the (overcounting) estimate of everything appended
/// since. Falls back to the projection's O(1) running sum before the first
/// sample (0033 Task 7 — `durable_estimate` equals a full walk of `durable`
/// by construction). Takes the **durable** projection — the ephemeral tail is
/// added by the one caller, [`Turn::estimate_tokens`], as its own term.
/// INVARIANT: everything committed after the anchored sample is counted exactly
/// once. Enforced by `the_anchor_counts_tool_results_even_with_todos_active` and
/// `compaction_still_triggers_with_todos_active`.
fn anchored_estimate<I: std::borrow::Borrow<Item>>(
    anchor: Option<(u64, usize)>,
    system_estimate: u64,
    durable: &[I],
    durable_estimate: u64,
) -> u64 {
    use hotl_context::tokens;
    match anchor {
        Some((reported, len)) if durable.len() >= len => {
            reported + tokens::estimate_items(&durable[len..])
        }
        _ => system_estimate + durable_estimate,
    }
}

/// A head slice on a char boundary (never mid-UTF-8) — the eviction preview.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn pair(tu: &ToolUse, message: &str, is_error: bool) -> ToolResultItem {
    ToolResultItem {
        tool_use_id: tu.id.clone(),
        content: message.to_string(),
        is_error,
    }
}

/// In-batch identical-call dedup (0037 D6): calls in one batch with identical
/// `(name, input)` whose tool is `read_only && parallel_safe` execute once.
/// Returns the unique calls plus, per original call, the index of the unique
/// call that answers it — or `None` when nothing is duplicated (the common
/// case stays allocation-free). Mutating tools never dedup: duplicate
/// mutations are the model's to own and the doom guard's to catch. Identity
/// is [`CallSig`]'s own spelling, so "identical" here and in the doom
/// detector can never drift apart.
fn dedup_batch(
    uses: &[ToolUse],
    registry: &hotl_tools::Registry,
) -> Option<(Vec<ToolUse>, Vec<usize>)> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut unique: Vec<ToolUse> = Vec::with_capacity(uses.len());
    let mut fan: Vec<usize> = Vec::with_capacity(uses.len());
    let mut any_dup = false;
    for tu in uses {
        let dedupable = registry
            .get(&tu.name)
            .is_some_and(|t| t.read_only() && t.parallel_safe());
        let sig = format!("{}({})", tu.name, tu.input);
        match seen.get(&sig) {
            Some(&at) if dedupable => {
                fan.push(at);
                any_dup = true;
            }
            _ => {
                if dedupable {
                    seen.insert(sig, unique.len());
                }
                fan.push(unique.len());
                unique.push(tu.clone());
            }
        }
    }
    any_dup.then_some((unique, fan))
}

/// Expand a deduplicated batch's results back to one `ToolResultItem` per
/// original call, in source order — the wire requires a result for every id.
fn fan_results(uses: &[ToolUse], fan: &[usize], unique: &[ToolResultItem]) -> Vec<ToolResultItem> {
    uses.iter()
        .zip(fan)
        .map(|(tu, &at)| ToolResultItem {
            tool_use_id: tu.id.clone(),
            content: unique[at].content.clone(),
            is_error: unique[at].is_error,
        })
        .collect()
}

/// Whether the todo reminder in a snapshot's **ephemeral tail** (see
/// [`crate::actor::Snapshot`] — the tail holds the rendered reminder, and only
/// when the list is non-empty) lists any `pending`/`in_progress` work. Reads
/// the render text directly rather than round-tripping through `Vec<Todo>`:
/// it's exactly what the model just sampled against, so the gate's read
/// matches what produced the reply.
fn unfinished_todos<I: std::borrow::Borrow<Item>>(tail: &[I]) -> bool {
    matches!(
        tail.last().map(std::borrow::Borrow::borrow),
        Some(Item::User {
            text,
            synthetic: Some(SyntheticReason::Todos),
            ..
        }) if text.contains("[ ]") || text.contains("[~]")
    )
}

fn unknown_tool(defs: &[ToolDef], name: &str) -> ToolOutcome {
    let available: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
    ToolOutcome::err(format!(
        "Unknown tool `{name}`. Available tools: {}.",
        available.join(", ")
    ))
}

/// Doom-loop lookback: max period (3) × required repeats (3). The detector
/// only ever reads this many trailing signatures.
const DOOM_WINDOW: usize = 9;

/// One tool-call signature: a hash for cheap equality plus the display text
/// the ask embeds. The display rides along (bounded by [`DOOM_WINDOW`])
/// because the repeating block can span batches — it can't always be
/// re-derived from the current batch's tool uses.
#[derive(Debug)]
pub(crate) struct CallSig {
    hash: u64,
    display: String,
    /// Hash of the completed call's result content, backfilled by
    /// [`backfill_result_hashes`] once the batch joins; `None` while the call
    /// is still in flight (a just-dispatched batch, by construction).
    result_hash: Option<u64>,
}

impl CallSig {
    fn new(tu: &ToolUse) -> Self {
        let display = format!("{}({})", tu.name, tu.input);
        Self {
            hash: content_hash(&display),
            display,
            result_hash: None,
        }
    }
}

/// One flagged decision, keying the per-turn notice memo (0037 D5). `denied`
/// is part of the key so a `DenyFlagged` is never buried by an earlier
/// `AutoFlagged` on the same target (they are different decisions a human
/// reviews differently).
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct FlagKey {
    name: String,
    class: Option<hotl_tools::ProtectedClass>,
    /// The call's canonical path (or url) when the input carries one — page
    /// reads of one file share a key while distinct files don't — else the
    /// whole input, so unrelated calls of a pathless tool never coalesce.
    target: String,
    denied: bool,
}

impl FlagKey {
    fn new(
        tu: &ToolUse,
        input: &Value,
        class: Option<hotl_tools::ProtectedClass>,
        denied: bool,
    ) -> Self {
        let target = input
            .get("path")
            .or_else(|| input.get("url"))
            .and_then(Value::as_str)
            .map(|p| {
                // Canonical so two spellings of one file are one decision; the
                // raw string when it can't resolve (the target of a refused
                // write may not exist). A memo key, never displayed.
                dunce::canonicalize(p)
                    .map(|c| c.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| p.to_string())
            })
            .unwrap_or_else(|| input.to_string());
        Self {
            name: tu.name.clone(),
            class,
            target,
            denied,
        }
    }
}

fn content_hash(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Fold a batch's new call signatures into the trailing window, evicting
/// from the front past [`DOOM_WINDOW`] (§S1 diet 2). A call's args are
/// final the moment it exists in `uses` — there is nothing to gain by
/// waiting for dispatch or a result — so `Turn::fold_doom_window` runs this
/// right at dispatch time instead of a separate pass in the caller. Kept
/// free of `Turn` so the fold order is unit-testable without an engine
/// harness (`fold_call_sigs_preserves_source_order_and_evicts_from_the_front`).
fn fold_call_sigs(window: &mut VecDeque<CallSig>, uses: &[ToolUse]) {
    window.extend(uses.iter().map(CallSig::new));
    while window.len() > DOOM_WINDOW {
        window.pop_front();
    }
}

/// Stamp a joined batch's window entries with their result hashes. The
/// window only evicts from the front, so the batch's sigs are its last
/// `results.len()` entries in order — align tails (a batch longer than
/// [`DOOM_WINDOW`] kept only its own tail at fold time).
fn backfill_result_hashes(window: &mut VecDeque<CallSig>, results: &[ToolResultItem]) {
    let n = results.len().min(window.len());
    let skip = window.len() - n;
    for (sig, r) in window
        .iter_mut()
        .skip(skip)
        .zip(&results[results.len() - n..])
    {
        sig.result_hash = Some(content_hash(&r.content));
    }
}

/// Repeating suffix patterns over tool-call signatures: any period p ≤ 3
/// whose block repeats 3× at the tail (a repetition detector).
fn detect_doom_loop(sigs: &[CallSig]) -> Option<String> {
    const REPEATS: usize = 3;
    for period in 1..=3usize {
        let need = period * REPEATS;
        if sigs.len() < need {
            continue;
        }
        let tail = &sigs[sigs.len() - need..];
        let block = &tail[..period];
        // Hash first (cheap), then the display string it already carries: a
        // 64-bit collision must not terminate a turn that was working (T3-8).
        // INVARIANT: only genuinely identical calls count as a repetition.
        // Enforced by `a_hash_collision_alone_is_not_a_doom_loop`.
        let same = |a: &CallSig, b: &CallSig| a.hash == b.hash && a.display == b.display;
        let identical_calls = tail
            .chunks(period)
            .all(|c| c.iter().zip(block).all(|(a, b)| same(a, b)));
        // Same call, same answer, no progress — the two *completed* repeat
        // blocks must agree elementwise before the just-dispatched third
        // (results unknown by construction) condemns the turn. Legitimate
        // polling repeats a call whose results keep changing; any `None` or
        // mismatch is conservative toward allowing, with `max_turns` as the
        // runaway backstop (0030 Task 7).
        let no_progress = tail[..period]
            .iter()
            .zip(&tail[period..2 * period])
            .all(|(a, b)| a.result_hash.is_some() && a.result_hash == b.result_hash);
        if identical_calls && no_progress {
            return Some(
                block
                    .iter()
                    .map(|s| s.display.as_str())
                    .collect::<Vec<_>>()
                    .join(" → "),
            );
        }
    }
    None
}

/// A MaxTokens stream can end inside a thinking block, leaving it unsigned;
/// echoed verbatim next request that is a 400. Signed blocks always pass.
fn prune_unsigned_trailing_thinking(blocks: &mut Vec<Value>) {
    if blocks.last().is_some_and(|b| {
        b.get("type").and_then(Value::as_str) == Some("thinking") && b.get("signature").is_none()
    }) {
        blocks.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sig(name: &str, input: Value) -> CallSig {
        CallSig::new(&ToolUse {
            id: "t".into(),
            name: name.into(),
            input,
        })
    }

    #[test]
    fn prune_drops_only_an_unsigned_trailing_thinking_block() {
        let unsigned = json!({"type": "thinking", "thinking": "partial"});
        let signed = json!({"type": "thinking", "thinking": "done", "signature": "sig"});
        let text = json!({"type": "text", "text": "hi"});

        let mut blocks = vec![text.clone(), unsigned.clone()];
        prune_unsigned_trailing_thinking(&mut blocks);
        assert_eq!(blocks, vec![text.clone()]);

        let mut blocks = vec![text.clone(), signed.clone()];
        prune_unsigned_trailing_thinking(&mut blocks);
        assert_eq!(blocks, vec![text.clone(), signed]);

        // Non-thinking last block survives even when truncated.
        let mut blocks = vec![unsigned, text.clone()];
        prune_unsigned_trailing_thinking(&mut blocks);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1], text);
    }

    /// `sig` with a completed result — the shape a prior batch's entries
    /// have once `backfill_result_hashes` ran.
    fn done(name: &str, input: Value, result: &str) -> CallSig {
        let mut s = sig(name, input);
        s.result_hash = Some(content_hash(result));
        s
    }

    #[test]
    fn doom_detector_finds_periods() {
        // The last repeat block is the just-dispatched batch: in flight,
        // results unknown. The completed prior blocks carry identical results.
        let a = || done("read", json!({"path":"x"}), "same body");
        let b = || done("bash", json!({"command":"ls"}), "same listing");
        let a_now = || sig("read", json!({"path":"x"}));
        let b_now = || sig("bash", json!({"command":"ls"}));
        assert!(detect_doom_loop(&[a(), a(), a_now()]).is_some());
        let sigs = vec![a(), b(), a(), b(), a_now(), b_now()];
        assert!(detect_doom_loop(&sigs).is_some());
        assert!(detect_doom_loop(&[a(), a(), b_now()]).is_none());
        assert!(detect_doom_loop(&[a(), a()]).is_none());
        // The ask still shows the human-readable signatures.
        let pattern = detect_doom_loop(&[a(), a(), a_now()]).unwrap();
        assert_eq!(pattern, "read({\"path\":\"x\"})");
    }

    /// 0030 Task 7: identical calls whose completed results kept changing are
    /// polling — progress, not a loop. Only "same call, same answer" fires.
    #[test]
    fn changing_results_are_polling_not_a_doom_loop() {
        let poll = |r: &str| done("bash", json!({"command":"curl status"}), r);
        let now = || sig("bash", json!({"command":"curl status"}));
        assert!(detect_doom_loop(&[poll("queued"), poll("running"), now()]).is_none());
        // An all-in-flight triple (one batch of three) cannot fire either.
        assert!(detect_doom_loop(&[now(), now(), now()]).is_none());
        // Control: identical answers still condemn the third dispatch.
        assert!(detect_doom_loop(&[poll("queued"), poll("queued"), now()]).is_some());
    }

    #[test]
    fn backfill_stamps_the_windows_tail_in_order() {
        let mut window: VecDeque<CallSig> = VecDeque::new();
        fold_call_sigs(&mut window, &uses(&[("read", json!({"path": "old"}))]));
        window[0].result_hash = Some(1);
        fold_call_sigs(
            &mut window,
            &uses(&[
                ("read", json!({"path": "a"})),
                ("bash", json!({"command": "b"})),
            ]),
        );
        let results: Vec<ToolResultItem> = ["ra", "rb"]
            .iter()
            .map(|c| ToolResultItem {
                tool_use_id: "t".into(),
                content: (*c).into(),
                is_error: false,
            })
            .collect();
        backfill_result_hashes(&mut window, &results);
        assert_eq!(window[0].result_hash, Some(1), "prior entries untouched");
        assert_eq!(window[1].result_hash, Some(content_hash("ra")));
        assert_eq!(window[2].result_hash, Some(content_hash("rb")));

        // A batch longer than the window: only its tail survived the fold,
        // so the result slice's tail aligns with the window's tail.
        let mut window: VecDeque<CallSig> = VecDeque::new();
        let big: Vec<(&str, Value)> = (0..DOOM_WINDOW + 2).map(|i| ("t", json!(i))).collect();
        fold_call_sigs(&mut window, &uses(&big));
        assert_eq!(window.len(), DOOM_WINDOW);
        let results: Vec<ToolResultItem> = (0..DOOM_WINDOW + 2)
            .map(|i| ToolResultItem {
                tool_use_id: "t".into(),
                content: format!("r{i}"),
                is_error: false,
            })
            .collect();
        backfill_result_hashes(&mut window, &results);
        // Window entry 0 is call #2 (the first two were evicted) → result r2.
        assert_eq!(window[0].result_hash, Some(content_hash("r2")));
        assert_eq!(
            window[DOOM_WINDOW - 1].result_hash,
            Some(content_hash(&format!("r{}", DOOM_WINDOW + 1)))
        );
    }

    fn uses(pairs: &[(&str, Value)]) -> Vec<ToolUse> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, (name, input))| ToolUse {
                id: i.to_string(),
                name: (*name).into(),
                input: input.clone(),
            })
            .collect()
    }

    /// §S1 diet 2: `fold_call_sigs` is what `Turn::fold_doom_window` calls
    /// at dispatch time instead of the old pre-dispatch batch pass. Pins two
    /// properties across a multi-tool batch: entries land in source order
    /// (never reordered, e.g. by a concurrent fold), and the window still
    /// evicts from the front once a fold crosses `DOOM_WINDOW` — including
    /// evicting an entire *prior* batch's entries when a later batch is big
    /// enough to push them out.
    #[test]
    fn fold_call_sigs_preserves_source_order_and_evicts_from_the_front() {
        let mut window: VecDeque<CallSig> = VecDeque::new();
        let batch1 = uses(&[
            ("read", json!({"path": "a"})),
            ("bash", json!({"command": "ls"})),
        ]);
        fold_call_sigs(&mut window, &batch1);
        let displays: Vec<&str> = window.iter().map(|s| s.display.as_str()).collect();
        assert_eq!(
            displays,
            vec![r#"read({"path":"a"})"#, r#"bash({"command":"ls"})"#]
        );

        // A second batch exactly `DOOM_WINDOW` long pushes the combined
        // fold to `DOOM_WINDOW` + 2: trimming must evict batch1's two
        // entries first (oldest first) and leave batch2 intact, in order.
        let batch2 = uses(
            &(0..DOOM_WINDOW)
                .map(|i| ("t", json!(i)))
                .collect::<Vec<_>>(),
        );
        fold_call_sigs(&mut window, &batch2);
        assert_eq!(window.len(), DOOM_WINDOW);
        let displays: Vec<String> = window.iter().map(|s| s.display.clone()).collect();
        let expected: Vec<String> = (0..DOOM_WINDOW).map(|i| format!("t({i})")).collect();
        assert_eq!(displays, expected);
    }

    /// Sets its flag when dropped — proof that an aborted task's future was
    /// actually torn down, not merely abandoned by the `select!`.
    struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The paused clock is legal here: these drive `await_speculation` alone,
    /// with no actor and therefore no writer-thread ack for the clock to
    /// auto-advance past (0011's standing constraint).
    #[tokio::test(start_paused = true)]
    async fn the_speculation_bound_abandons_a_stalled_digest() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = DropFlag(Arc::clone(&dropped));
        let handle = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
            None
        });
        let got = await_speculation(handle, &CancellationToken::new(), SPECULATION_WAIT).await;
        assert!(got.is_none(), "a stalled digest must be abandoned");
        tokio::task::yield_now().await;
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the abandoned speculation must be aborted, not left burning a provider call"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_turn_abandons_its_speculation_without_waiting_out_the_bound() {
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
            None
        });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let start = tokio::time::Instant::now();
        let got = await_speculation(handle, &cancel, SPECULATION_WAIT).await;
        assert!(got.is_none());
        assert_eq!(
            start.elapsed(),
            std::time::Duration::ZERO,
            "an interrupt must not wait out the speculation bound"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_speculation_still_reaches_the_fold() {
        let handle = tokio::spawn(async {
            Some(crate::SpecDigest {
                prefix_end: 1,
                kept_from: 4,
                text: "DIGEST".into(),
            })
        });
        // Let the task complete before the race, so the assertion is about the
        // happy path rather than about which arm happened to be ready first.
        tokio::task::yield_now().await;
        let got = await_speculation(handle, &CancellationToken::new(), SPECULATION_WAIT)
            .await
            .expect("a finished digest must reach the fold");
        assert_eq!(got.text, "DIGEST");
        assert_eq!((got.prefix_end, got.kept_from), (1, 4));
    }

    /// T3-8: `CallSig` already carries the exact display string, so a 64-bit
    /// `DefaultHasher` collision must not terminate a turn that was working.
    #[test]
    fn a_hash_collision_alone_is_not_a_doom_loop() {
        // Completed entries share a result so only call identity is under test.
        let collide = |display: &str, in_flight: bool| CallSig {
            hash: 7,
            display: display.into(),
            result_hash: (!in_flight).then_some(9),
        };
        assert!(
            detect_doom_loop(&[collide("a", false), collide("b", false), collide("c", true)])
                .is_none()
        );
        assert!(
            detect_doom_loop(&[collide("a", false), collide("a", false), collide("a", true)])
                .is_some()
        );
    }

    fn todos_item(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: Some(SyntheticReason::Todos),
            images: Vec::new(),
        }
    }

    /// T3-5: `propose` returned a bare `bool`, and both failure modes rendered
    /// as "session log is sealed" — so an ordinary shutdown was reported to the
    /// user as log corruption.
    #[test]
    fn a_dropped_actor_is_not_reported_as_a_sealed_log() {
        assert_eq!(
            Commit::from_reply(Some(crate::ProposeReply::Committed)),
            Commit::Committed
        );
        assert_eq!(
            Commit::from_reply(Some(crate::ProposeReply::Sealed)),
            Commit::Sealed
        );
        assert_eq!(Commit::from_reply(None), Commit::Gone);
        assert!(Commit::Sealed.message().contains("sealed"));
        assert!(!Commit::Gone.message().contains("sealed"));
        assert!(Commit::Gone.message().contains("shutting down"));
        assert!(Commit::Committed.ok() && !Commit::Sealed.ok() && !Commit::Gone.ok());
    }

    fn tool_results(content: &str) -> Item {
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: "t1".into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn the_anchor_counts_tool_results_even_with_todos_active() {
        // D = 1 durable item at sample 1, with a todo list active. The anchor
        // indexes the durable projection, which the ephemeral reminder is no
        // longer part of — so the tool results committed between the two
        // samples stay inside the anchored slice (T2-1).
        let prompt = Item::User {
            text: "go".into(),
            synthetic: None,
            images: Vec::new(),
        };
        let sample1 = [prompt.clone()];
        let anchor = Some((500, sample1.len() + 1));

        let big = "x".repeat(3_000);
        let sample2 = vec![
            prompt,
            Item::Assistant {
                blocks: vec![json!({"type": "text", "text": "ok"})],
            },
            tool_results(&big),
        ];
        let estimate = anchored_estimate(
            anchor,
            hotl_context::tokens::estimate_text("sys"),
            &sample2,
            hotl_context::tokens::estimate_items(&sample2),
        );
        assert!(
            estimate >= 500 + 1_000,
            "tool results must be inside the estimate, got {estimate}"
        );
    }

    /// The tail is a separate term, and it is the SAME term: splitting a flat
    /// snapshot into durable + tail must not move the total by a token, on
    /// either the anchored or the pre-first-sample path.
    #[test]
    fn splitting_the_tail_out_leaves_the_estimate_unchanged() {
        use hotl_context::tokens;
        let durable = vec![
            Item::User {
                text: "go".into(),
                synthetic: None,
                images: Vec::new(),
            },
            tool_results(&"x".repeat(600)),
        ];
        let reminder = todos_item("<todos>\n[~] wire the gate\n</todos>");
        let flat: Vec<Item> = durable.iter().cloned().chain([reminder.clone()]).collect();
        let tail = vec![reminder];

        for anchor in [None, Some((500, 1)), Some((500, 2))] {
            assert_eq!(
                anchored_estimate(
                    anchor,
                    tokens::estimate_text("sys"),
                    &durable,
                    tokens::estimate_items(&durable),
                ) + tokens::estimate_items(&tail),
                anchored_estimate(
                    anchor,
                    tokens::estimate_text("sys"),
                    &flat,
                    tokens::estimate_items(&flat),
                ),
                "anchor {anchor:?}"
            );
        }
    }

    #[test]
    fn unfinished_todos_reads_only_the_tagged_last_item() {
        assert!(!unfinished_todos::<Item>(&[]));
        // Untagged last item (an ordinary reply) never counts, even if it
        // happens to contain the marker text.
        assert!(!unfinished_todos(&[Item::User {
            text: "[ ] not a todo reminder".into(),
            synthetic: None,
            images: Vec::new()
        }]));
        assert!(unfinished_todos(&[todos_item("<todos>\n[ ] a\n</todos>")]));
        assert!(unfinished_todos(&[todos_item("<todos>\n[~] a\n</todos>")]));
        assert!(!unfinished_todos(&[todos_item("<todos>\n[x] a\n</todos>")]));
    }

    // --- Task 8 (S2a PreparedPayload) --------------------------------

    #[tokio::test]
    async fn prepare_entry_masks_and_stamps_the_epoch() {
        std::env::set_var("HOTL_T8_TURN_TOKEN", "sk-super-secret-turntest-1");
        let masker = Arc::new(hotl_store::Masker::from_env());
        let payload = EntryPayload::Item {
            item: Item::User {
                text: "key: sk-super-secret-turntest-1".into(),
                synthetic: None,
                images: Vec::new(),
            },
        };
        let prepared = prepare_entry(&payload, &masker, 7).await.expect("prepare");
        assert_eq!(prepared.payload.rules_epoch(), 7);
        assert_eq!(prepared.payload.kind(), hotl_store::EntryKind::Item);
        let text = std::str::from_utf8(prepared.payload.bytes()).unwrap();
        assert!(
            !text.contains("sk-super-secret-turntest-1"),
            "secret must be masked before it ever reaches the actor: {text}"
        );
        assert!(matches!(prepared.item, Some(Item::User { .. })));
        std::env::remove_var("HOTL_T8_TURN_TOKEN");
    }

    #[tokio::test]
    async fn prepare_entry_has_no_item_for_non_item_kinds() {
        let masker = Arc::new(hotl_store::Masker::empty());
        let payload = EntryPayload::Usage {
            usage: TokenUsage::default(),
        };
        let prepared = prepare_entry(&payload, &masker, 0).await.expect("prepare");
        assert!(prepared.item.is_none());
        assert_eq!(prepared.payload.kind(), hotl_store::EntryKind::Usage);
    }

    #[tokio::test]
    async fn prepare_entry_clean_input_matches_raw_serialization_byte_for_byte() {
        let masker = Arc::new(
            hotl_store::Masker::empty().with_value("HOTL_T8_UNUSED", "not-present-anywhere-12345"),
        );
        let payload = EntryPayload::Usage {
            usage: TokenUsage::default(),
        };
        let prepared = prepare_entry(&payload, &masker, 0).await.expect("prepare");
        let raw = hotl_store::serialize_payload(&payload).unwrap();
        assert_eq!(prepared.payload.bytes().as_ref(), raw.as_bytes());
    }

    #[tokio::test]
    async fn prepare_entry_dirty_input_matches_masker_apply() {
        let masker = Arc::new(
            hotl_store::Masker::empty().with_value("HOTL_T8_DIRTY", "present-secret-value-67890"),
        );
        let payload = EntryPayload::Item {
            item: Item::User {
                text: "token present-secret-value-67890 here".into(),
                synthetic: None,
                images: Vec::new(),
            },
        };
        let prepared = prepare_entry(&payload, &masker, 0).await.expect("prepare");
        let raw = hotl_store::serialize_payload(&payload).unwrap();
        let expected = masker.apply(&raw);
        assert_eq!(prepared.payload.bytes().as_ref(), expected.as_bytes());
    }

    /// requirement 7: payloads at/above `PREPARE_OFFLOAD_BYTES` still
    /// produce byte-identical output — only *where* masking runs changes.
    #[tokio::test]
    async fn prepare_entry_offloads_large_payloads_with_identical_bytes() {
        let masker = Arc::new(hotl_store::Masker::empty());
        let big_text = "x".repeat(PREPARE_OFFLOAD_BYTES + 4096);
        let payload = EntryPayload::Item {
            item: Item::User {
                text: big_text,
                synthetic: None,
                images: Vec::new(),
            },
        };
        let raw = hotl_store::serialize_payload(&payload).unwrap();
        assert!(
            raw.len() >= PREPARE_OFFLOAD_BYTES,
            "fixture must actually cross the offload threshold"
        );
        let prepared = prepare_entry(&payload, &masker, 0).await.expect("prepare");
        assert_eq!(prepared.payload.bytes().as_ref(), raw.as_bytes());
    }

    /// IMPORTANT 2 (task 8 review): the offload decision has to be made
    /// BEFORE serialization, from an estimate — this pins that the
    /// estimator actually reflects the spec's own worst case (many tool
    /// results, none individually huge, together well past the threshold),
    /// not just a single large field.
    #[test]
    fn payload_size_estimate_sums_tool_result_content_across_the_batch() {
        // Each result alone is well under the threshold; only the sum
        // crosses it — proves the estimator aggregates across the batch
        // instead of looking at (say) just the largest result.
        let each = PREPARE_OFFLOAD_BYTES / 5;
        let payload = EntryPayload::Item {
            item: Item::ToolResults {
                results: (0..10)
                    .map(|i| ToolResultItem {
                        tool_use_id: format!("t{i}"),
                        content: "x".repeat(each),
                        is_error: false,
                    })
                    .collect(),
            },
        };
        assert!(
            each < PREPARE_OFFLOAD_BYTES,
            "fixture must keep each individual result under the threshold"
        );
        assert!(
            payload_size_estimate(&payload) >= PREPARE_OFFLOAD_BYTES,
            "ten results at PREPARE_OFFLOAD_BYTES/5 each must sum past the threshold"
        );
    }

    #[test]
    fn payload_size_estimate_is_zero_for_kinds_that_never_offload() {
        assert_eq!(
            payload_size_estimate(&EntryPayload::Usage {
                usage: TokenUsage::default()
            }),
            0
        );
    }

    /// The estimate is what gates offload — confirm the gate itself fires
    /// from the estimate alone, without needing to serialize first (unlike
    /// the pre-fix version, which checked `json.len()` after the inline
    /// `serde_json::to_string` had already run).
    #[tokio::test]
    async fn prepare_entry_offload_decision_uses_the_pre_serialization_estimate() {
        let masker = Arc::new(hotl_store::Masker::empty());
        let payload = EntryPayload::Item {
            item: Item::ToolResults {
                results: vec![ToolResultItem {
                    tool_use_id: "t1".into(),
                    content: "y".repeat(PREPARE_OFFLOAD_BYTES + 4096),
                    is_error: false,
                }],
            },
        };
        assert!(payload_size_estimate(&payload) >= PREPARE_OFFLOAD_BYTES);
        let raw = hotl_store::serialize_payload(&payload).unwrap();
        let prepared = prepare_entry(&payload, &masker, 3).await.expect("prepare");
        assert_eq!(prepared.payload.bytes().as_ref(), raw.as_bytes());
        assert_eq!(prepared.payload.rules_epoch(), 3);
    }

    #[test]
    fn commit_from_reply_distinguishes_stale_epoch_from_sealed() {
        assert_eq!(
            Commit::from_reply(Some(crate::ProposeReply::StaleEpoch)),
            Commit::StaleEpoch
        );
        assert_eq!(
            Commit::from_reply(Some(crate::ProposeReply::Sealed)),
            Commit::Sealed
        );
        assert_eq!(
            Commit::from_reply(Some(crate::ProposeReply::Committed)),
            Commit::Committed
        );
        assert_eq!(Commit::from_reply(None), Commit::Gone);
    }

    // --- Task 9 (S2b pipelined commits) ------------------------------

    /// A ticket plus the handle that decides its fate, so the window's
    /// behaviour can be driven without an actor behind it.
    fn ticket(
        seq: u64,
    ) -> (
        crate::CommitTicket,
        oneshot::Sender<Result<crate::CommitAck, crate::CommitFailed>>,
    ) {
        let (tx, rx) = oneshot::channel();
        (
            crate::CommitTicket {
                id: format!("id-{seq}"),
                seq,
                ack: rx,
            },
            tx,
        )
    }

    fn resolved(
        seq: u64,
        result: Result<crate::CommitAck, crate::CommitFailed>,
    ) -> crate::CommitTicket {
        let (ticket, tx) = ticket(seq);
        let _ = tx.send(result);
        ticket
    }

    /// commit-protocol.md §Pipelined commits: "a turn holding 16 unresolved
    /// tickets waits on the oldest before it may propose again". The window
    /// is the backpressure, so it can never grow past [`ACK_WINDOW`].
    #[tokio::test]
    async fn the_ticket_window_waits_on_the_oldest_before_it_overflows() {
        let mut pipeline = TicketPipeline::default();
        for seq in 0..ACK_WINDOW as u64 {
            let commit = pipeline
                .submit(resolved(seq, Ok(crate::CommitAck { offset: seq })))
                .await;
            assert_eq!(commit, Commit::Committed);
        }
        assert_eq!(pipeline.len(), ACK_WINDOW);

        let commit = pipeline
            .submit(resolved(99, Ok(crate::CommitAck { offset: 99 })))
            .await;
        assert_eq!(commit, Commit::Committed);
        assert_eq!(
            pipeline.len(),
            ACK_WINDOW,
            "the window is the bound, not a suggestion"
        );
    }

    /// The bound's second job: "it caps how wrong a turn can be about
    /// durability" — a sealed log is reported through the ticket, at the
    /// barrier that resolves it.
    #[tokio::test]
    async fn a_barrier_reports_the_first_failing_ticket_and_empties_the_window() {
        let mut pipeline = TicketPipeline::default();
        pipeline
            .submit(resolved(1, Ok(crate::CommitAck { offset: 1 })))
            .await;
        pipeline
            .submit(resolved(2, Err(crate::CommitFailed::LogSealed)))
            .await;
        pipeline
            .submit(resolved(3, Ok(crate::CommitAck { offset: 3 })))
            .await;

        assert_eq!(pipeline.drain().await, Commit::Sealed);
        assert!(
            pipeline.is_empty(),
            "a barrier resolves every ticket, failure included"
        );
    }

    /// Cancelled ≠ Failed: an `Aborted` ticket means a compaction or branch
    /// move superseded the turn, which is a clean termination, never an
    /// error outcome.
    #[tokio::test]
    async fn an_aborted_ticket_ends_the_turn_cancelled_not_failed() {
        let mut pipeline = TicketPipeline::default();
        pipeline
            .submit(resolved(1, Err(crate::CommitFailed::Aborted)))
            .await;
        let commit = pipeline.drain().await;
        assert_eq!(commit, Commit::Aborted);
        assert_eq!(commit.outcome(), Outcome::Cancelled);
    }

    /// A dropped actor is a shutdown, not log corruption (T3-5) — the same
    /// distinction `Commit::Gone` already draws for the `Sync` path.
    #[tokio::test]
    async fn a_dropped_ticket_channel_is_a_shutdown_not_a_seal() {
        let mut pipeline = TicketPipeline::default();
        let (ticket, tx) = ticket(1);
        drop(tx);
        pipeline.submit(ticket).await;
        assert_eq!(pipeline.drain().await, Commit::Gone);
    }

    /// A ticket is an unresolved claim, never a durable commit. Reporting one
    /// as `Committed` would be a fsync-before-ack hole: the turn would go on
    /// as though bytes it never waited for were on disk. Unreachable today —
    /// the actor branches on `AckMode` — so the guard is what keeps it that
    /// way through S2c, which rewrites exactly this routing.
    #[test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "drives a debug_assert, which release builds compile out"
    )]
    #[should_panic(expected = "a ticket is not a commit")]
    fn from_reply_refuses_to_treat_a_ticket_as_a_durable_commit() {
        let (_tx, rx) = oneshot::channel();
        let _ = Commit::from_reply(Some(crate::ProposeReply::Ticket(crate::CommitTicket {
            id: "x".into(),
            seq: 1,
            ack: rx,
        })));
    }

    /// …and in release, where the `debug_assert` is compiled out, the same
    /// misroute has to fail safe rather than silently succeed.
    #[test]
    fn an_unconfirmed_commit_is_never_ok() {
        assert!(!Commit::Unconfirmed.ok());
        assert!(matches!(
            Commit::Unconfirmed.outcome(),
            Outcome::Error { .. }
        ));
    }

    /// Barrier (c): a commit that failed after the turn thought it was
    /// finished replaces the end reason, because the log does not contain
    /// what that reason names — the same T3-6 rule
    /// `Turn::handle_doom_loop` already applies inline ("a `DoomLoop`
    /// outcome would name a batch the log will never carry"). The two
    /// exceptions are an end that is already an error (keep the first
    /// cause) and a cancellation (Cancelled ≠ Failed).
    #[test]
    fn a_failed_barrier_c_replaces_every_end_reason_it_invalidates() {
        let sealed = |outcome| match seal_end(TurnEnd::Outcome(outcome), Commit::Sealed) {
            TurnEnd::Outcome(o) => o,
            TurnEnd::Compact { .. } => unreachable!(),
        };
        for outcome in [
            Outcome::Done { text: "hi".into() },
            Outcome::TurnLimit,
            Outcome::Refused,
            Outcome::DoomLoop {
                pattern: "read".into(),
            },
            Outcome::ToolFailureBudget {
                tool: "bash".into(),
            },
        ] {
            assert!(
                matches!(sealed(outcome.clone()), Outcome::Error { .. }),
                "{outcome:?} names a record the log never took"
            );
        }
        assert_eq!(sealed(Outcome::Cancelled), Outcome::Cancelled);
        let first = Outcome::Error {
            message: "the original cause".into(),
        };
        assert_eq!(sealed(first.clone()), first);
    }

    #[test]
    fn a_clean_barrier_c_leaves_the_end_alone() {
        let end = seal_end(
            TurnEnd::Outcome(Outcome::Done { text: "hi".into() }),
            Commit::Committed,
        );
        assert!(matches!(end, TurnEnd::Outcome(Outcome::Done { .. })));
    }

    #[test]
    fn image_bytes_past_the_budget_force_a_fold_at_a_trivial_token_estimate() {
        let window = 200_000;
        // Tokens nowhere near the trigger, bytes over budget: only the byte guard
        // can catch this, and it is the case that bricks a session.
        assert!(must_compact(1, window, hotl_types::IMAGE_B64_BUDGET + 1));
        assert!(!must_compact(1, window, hotl_types::IMAGE_B64_BUDGET));
        // The token trigger is unchanged.
        assert!(must_compact(
            (window as f64 * COMPACT_TRIGGER) as u64 + 1,
            window,
            0
        ));
        assert!(!must_compact(
            (window as f64 * COMPACT_TRIGGER) as u64,
            window,
            0
        ));
    }

    #[test]
    fn image_bytes_past_the_speculate_threshold_fire_early_at_a_trivial_token_estimate() {
        let window = 200_000;
        let threshold =
            (hotl_types::IMAGE_B64_BUDGET as f64 * (SPECULATE_TRIGGER / COMPACT_TRIGGER)) as usize;
        // Tokens nowhere near the trigger, bytes past the scaled threshold: only
        // the byte guard can catch this — the case `maybe_speculate_digest`
        // used to miss, leaving byte-triggered folds with no head start.
        assert!(should_speculate(1, window, threshold + 1));
        assert!(!should_speculate(1, window, threshold));
        // The token trigger is unchanged.
        assert!(should_speculate(
            (window as f64 * SPECULATE_TRIGGER) as u64 + 1,
            window,
            0
        ));
        assert!(!should_speculate(
            (window as f64 * SPECULATE_TRIGGER) as u64,
            window,
            0
        ));
    }
}
