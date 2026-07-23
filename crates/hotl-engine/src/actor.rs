//! The session actor: sole committer, projection owner, turn scheduler.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use hotl_context::compaction;
use hotl_platform::Clock;
use hotl_provider::{Provider, SamplingRequest, StreamEvent};
use hotl_store::SessionLog;
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{assistant_text, EntryPayload, Item, SyntheticReason, TokenUsage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{turn, EngineConfig, EngineEvent, Outcome, SessionCmd, SessionDeps, TurnEnd};

/// Verbatim tail kept through a compaction, as a share of the window.
pub(crate) const TAIL_RATIO: f64 = 0.3;
const SUMMARIZE_ATTEMPTS: u32 = 2;
const SUMMARIZE_MAX_TOKENS: u32 = 2_000;
/// Compactions without an intervening completed sample before giving up —
/// prevents a fold-the-digest spiral when the tail alone overflows.
const MAX_COMPACT_STREAK: u32 = 2;

/// Dependencies shared with turn tasks. The log is *not* here: only the actor
/// loop writes it, so it lives as a local in [`run`].
pub(crate) struct SharedDeps {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub rules: Arc<Rules>,
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub system: Arc<str>,
    pub cwd: PathBuf,
    pub config: EngineConfig,
    pub snapshots: Option<Arc<dyn crate::Snapshotter>>,
    pub hooks: Option<Arc<dyn crate::hooks::Hooks>>,
}

impl SharedDeps {
    fn new(deps: SessionDeps) -> (Self, SessionLog) {
        let shared = Self {
            provider: deps.provider,
            registry: deps.registry,
            rules: deps.rules,
            sandbox_enforced: deps.sandbox_enforced,
            clock: deps.clock,
            system: deps.system.into(),
            cwd: deps.cwd,
            config: deps.config,
            snapshots: deps.snapshots,
            hooks: deps.hooks,
        };
        (shared, deps.log)
    }

    /// Durable append (flush inside `SessionLog`); false = log sealed.
    /// The failure surfaces to the user via the turn outcome, not stderr.
    fn append(&self, log: &mut SessionLog, payload: &EntryPayload) -> bool {
        log.append(payload, self.clock.now_ms()).is_ok()
    }
}

pub(crate) async fn run(
    mut deps: SessionDeps,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
    cmd_tx: mpsc::WeakSender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
) {
    // Resumed history is repaired on the way in: a log written by a build that
    // let a steer land mid-batch would otherwise fail every request forever.
    let mut items: Arc<Vec<Item>> =
        Arc::new(pair_tool_results(std::mem::take(&mut deps.initial_items)));
    let mut running = false;
    let mut queue: VecDeque<(String, Option<SyntheticReason>)> = VecDeque::new();
    // Steers that arrived while a tool batch was open, waiting for its results
    // to close the pairing before they can be appended.
    let mut held_steers: Vec<String> = Vec::new();
    let (shared, mut log) = SharedDeps::new(deps);
    let shared = Arc::new(shared);
    // Usage carried across compaction respawns within one logical turn.
    let mut carry_usage = TokenUsage::default();
    let mut compact_streak: u32 = 0;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCmd::Prompt(text) => {
                running = admit_prompt(
                    &shared,
                    &mut log,
                    &mut items,
                    &mut queue,
                    running,
                    text,
                    None,
                    &cmd_tx,
                    &events,
                    &current_turn,
                )
                .await;
            }
            SessionCmd::PromptTagged { text, synthetic } => {
                running = admit_prompt(
                    &shared,
                    &mut log,
                    &mut items,
                    &mut queue,
                    running,
                    text,
                    Some(synthetic),
                    &cmd_tx,
                    &events,
                    &current_turn,
                )
                .await;
            }
            SessionCmd::Continue => {
                if !running && crate::needs_continuation(&items) {
                    spawn_turn(&shared, &cmd_tx, &events, &current_turn);
                    running = true;
                }
            }
            SessionCmd::Steer(text) => {
                admit_steer(&shared, &mut log, &mut items, &mut held_steers, text)
            }
            SessionCmd::Rename(name) => {
                let _ = shared.append(&mut log, &EntryPayload::Rename { name });
            }
            SessionCmd::Snapshot { reply } => {
                let _ = reply.send(Arc::clone(&items));
            }
            SessionCmd::Propose { entries, reply } => {
                let committed = commit(&shared, &mut log, &mut items, entries);
                // The results a held steer was waiting on may have just landed.
                release_steers(&shared, &mut log, &mut items, &mut held_steers);
                let _ = reply.send(committed);
            }
            SessionCmd::WriteBlob {
                tool_use_id,
                content,
                reply,
            } => {
                let result = match log.write_blob(&tool_use_id, &content) {
                    Ok(path) => Ok(path.display().to_string()),
                    Err(_) => Err(content), // hand the content back — never lose it
                };
                let _ = reply.send(result);
            }
            SessionCmd::TurnFinished { end, usage } => {
                // The turn is over, so nothing will answer an open batch now.
                // Close it, then let held steers land before a queued prompt
                // starts the next turn behind them.
                close_open_batch(&shared, &mut log, &mut items);
                release_steers(&shared, &mut log, &mut items, &mut held_steers);
                on_turn_finished(
                    TurnFinishedCtx {
                        shared: &shared,
                        log: &mut log,
                        items: &mut items,
                        queue: &mut queue,
                        running: &mut running,
                        carry_usage: &mut carry_usage,
                        compact_streak: &mut compact_streak,
                        cmd_tx: &cmd_tx,
                        events: &events,
                        current_turn: &current_turn,
                    },
                    end,
                    usage,
                )
                .await;
            }
        }
    }
}

/// The mutable session state `on_turn_finished` threads back into the loop.
struct TurnFinishedCtx<'a> {
    shared: &'a Arc<SharedDeps>,
    log: &'a mut SessionLog,
    items: &'a mut Arc<Vec<Item>>,
    queue: &'a mut VecDeque<(String, Option<SyntheticReason>)>,
    running: &'a mut bool,
    carry_usage: &'a mut TokenUsage,
    compact_streak: &'a mut u32,
    cmd_tx: &'a mpsc::WeakSender<SessionCmd>,
    events: &'a mpsc::Sender<EngineEvent>,
    current_turn: &'a Arc<Mutex<CancellationToken>>,
}

/// A turn ended: either report it (and promote the queue) or, on a compaction
/// request, fold and respawn the continuation.
async fn on_turn_finished(ctx: TurnFinishedCtx<'_>, end: TurnEnd, mut usage: TokenUsage) {
    let outcome = match end {
        TurnEnd::Outcome(outcome) => Some(outcome),
        TurnEnd::Compact { spec } => {
            *ctx.carry_usage += usage;
            usage = TokenUsage::default();
            try_compact(
                ctx.shared,
                ctx.log,
                ctx.items,
                ctx.compact_streak,
                spec,
                ctx.cmd_tx,
                ctx.events,
                ctx.current_turn,
            )
            .await
        }
    };
    if let Some(outcome) = outcome {
        *ctx.compact_streak = 0;
        let mut total = usage;
        total += std::mem::take(ctx.carry_usage);
        *ctx.running = end_turn(
            ctx.shared,
            ctx.log,
            ctx.items,
            ctx.queue,
            outcome,
            total,
            ctx.cmd_tx,
            ctx.events,
            ctx.current_turn,
        )
        .await;
    }
}

/// One compaction attempt on behalf of a turn that hit the threshold: fold,
/// announce, respawn the continuation. `Some(outcome)` means compaction can't
/// proceed (streak cap, nothing to fold, sealed log) and the turn ends.
#[allow(clippy::too_many_arguments)]
async fn try_compact(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    compact_streak: &mut u32,
    spec: Option<crate::SpecDigest>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> Option<Outcome> {
    *compact_streak += 1;
    // The token interrupt() cancels right now belongs to the turn that just
    // ended with `Compact`. Honor it through the whole compaction window —
    // race the inline summarize against it, and hand the *same* token to the
    // continuation — so an interrupt anywhere in the window ends the logical
    // turn instead of being silently swallowed.
    let cancel = current_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let compacted = if *compact_streak > MAX_COMPACT_STREAK {
        Err("context window exhausted — compaction can no longer make room".into())
    } else {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Some(Outcome::Cancelled),
            compacted = compact(shared, log, items, spec) => compacted,
        }
    };
    match compacted {
        Ok(degraded) => {
            let _ = events.send(EngineEvent::Compacted { degraded }).await;
            if cancel.is_cancelled() {
                return Some(Outcome::Cancelled);
            }
            respawn_turn(shared, cmd_tx, events, cancel);
            None // still running: same logical turn continues
        }
        Err(message) => Some(Outcome::Error { message }),
    }
}

/// Annotate + report a finished turn, then promote the next queued prompt.
/// Returns whether a turn is (still) running.
#[allow(clippy::too_many_arguments)]
async fn end_turn(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    queue: &mut VecDeque<(String, Option<SyntheticReason>)>,
    outcome: Outcome,
    usage: TokenUsage,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    annotate(shared, log, &outcome);
    let _ = events.send(EngineEvent::TurnDone { outcome, usage }).await;
    match queue.pop_front() {
        Some((next, synthetic)) => {
            start_turn(
                shared,
                log,
                items,
                next,
                synthetic,
                cmd_tx,
                events,
                current_turn,
            )
            .await
        }
        None => false,
    }
}

/// Whether the projection is mid-batch: it ends on an assistant turn whose
/// tool calls have no results yet. Both APIs require those results to be the
/// very next message, so nothing else may be appended in this window.
fn awaiting_tool_results(items: &[Item]) -> bool {
    matches!(
        items.last(),
        Some(Item::Assistant { blocks }) if !hotl_types::assistant_tool_uses(blocks).is_empty()
    )
}

/// Durable admission on arrival; projection advances only after the append
/// (commit-protocol §durability). Linear-log M1 records the steer as a
/// tagged user item; the `steer_admission` entry kind arrives with M3b's tree.
///
/// Steering mid-batch is the normal case — the human reacts while a tool runs
/// — and that is precisely the window where appending would strand the batch's
/// results away from the calls they answer. Such a steer is held instead and
/// released once the results land. The model sees it at the same moment either
/// way: the next sample happens after the batch closes.
fn admit_steer(
    shared: &SharedDeps,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    held: &mut Vec<String>,
    text: String,
) {
    if awaiting_tool_results(items) {
        held.push(text);
        return;
    }
    append_steer(shared, log, items, text);
}

fn append_steer(
    shared: &SharedDeps,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    text: String,
) {
    let payload = EntryPayload::Item {
        item: Item::User {
            text,
            synthetic: Some(SyntheticReason::Steer),
        },
    };
    if shared.append(log, &payload) {
        if let EntryPayload::Item { item } = payload {
            Arc::make_mut(items).push(item);
        }
    }
}

/// Append the steers that were waiting on a batch, oldest first, once the
/// pairing is closed.
fn release_steers(
    shared: &SharedDeps,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    held: &mut Vec<String>,
) {
    if held.is_empty() || awaiting_tool_results(items) {
        return;
    }
    for text in held.drain(..) {
        append_steer(shared, log, items, text);
    }
}

/// Answer a batch nothing will answer any more. A turn that dies before it can
/// report leaves calls hanging; the next request would be rejected for the
/// missing results, so the protocol gets completed here instead.
fn close_open_batch(shared: &SharedDeps, log: &mut SessionLog, items: &mut Arc<Vec<Item>>) {
    let Some(Item::Assistant { blocks }) = items.last() else {
        return;
    };
    let uses = hotl_types::assistant_tool_uses(blocks);
    if uses.is_empty() {
        return;
    }
    let payload = EntryPayload::Item {
        item: Item::ToolResults {
            results: uses
                .iter()
                .map(|tu| hotl_types::ToolResultItem {
                    tool_use_id: tu.id.clone(),
                    content: "Not executed (the turn ended first).".into(),
                    is_error: true,
                })
                .collect(),
        },
    };
    if shared.append(log, &payload) {
        if let EntryPayload::Item { item } = payload {
            Arc::make_mut(items).push(item);
        }
    }
}

/// Restore tool_use/tool_result adjacency in history written before steers
/// were held. Items that landed in the gap move to just after the results
/// they interrupted — the order the model would have seen anyway, since the
/// gap only ever opened while a batch was still running. Nothing is dropped.
pub(crate) fn pair_tool_results(items: Vec<Item>) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::with_capacity(items.len());
    // Items pulled out of an open batch, waiting to go back in behind it.
    let mut stranded: Vec<Item> = Vec::new();
    for item in items {
        if !awaiting_tool_results(&out) && stranded.is_empty() {
            out.push(item);
            continue;
        }
        match item {
            Item::ToolResults { .. } => {
                out.push(item);
                out.append(&mut stranded);
            }
            // Another assistant turn means no results were ever coming; the
            // gap was not an open batch, so leave the order as it was found.
            Item::Assistant { .. } => {
                out.append(&mut stranded);
                out.push(item);
            }
            _ => stranded.push(item),
        }
    }
    out.append(&mut stranded);
    out
}

/// Commit a proposal: append each entry durably, then project it.
fn commit(
    shared: &SharedDeps,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    entries: Vec<EntryPayload>,
) -> bool {
    for payload in entries {
        if !shared.append(log, &payload) {
            return false;
        }
        if let EntryPayload::Item { item } = payload {
            Arc::make_mut(items).push(item);
        }
    }
    true
}

/// Non-Done outcomes leave a durable annotation in the log.
fn annotate(shared: &SharedDeps, log: &mut SessionLog, outcome: &Outcome) {
    let reason = match outcome {
        Outcome::Cancelled => Some("user interrupt".to_string()),
        Outcome::TurnLimit => Some(format!("max_turns ({}) reached", shared.config.max_turns)),
        Outcome::DoomLoop { pattern } => Some(format!("doom loop: {pattern}")),
        Outcome::ToolFailureBudget { tool } => Some(format!("tool failure budget: {tool}")),
        Outcome::Error { message } => Some(format!("error: {message}")),
        Outcome::Done { .. } | Outcome::Refused => None,
    };
    if let Some(reason) = reason {
        shared.append(log, &EntryPayload::Cancelled { reason });
    }
}

/// Compact the projection (M2): fold `[prefix..kept_from)` into a typed
/// digest via the fast model, floor to a placeholder if summarize fails, and
/// re-point the projection with an appended `compaction` entry — the log
/// keeps everything. A digest the turn speculatively precomputed folds
/// instantly; otherwise the summarize runs inline in the actor (no turn is
/// in flight, and admission blocking during that call is the serialization
/// working as designed).
async fn compact(
    shared: &SharedDeps,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    spec: Option<crate::SpecDigest>,
) -> Result<bool, String> {
    // Speculative hit: the digest was planned against this same projection
    // lineage (it only appends between folds), so its indices still name the
    // same items. Reset mode folds a wider span than the speculation covered,
    // so it never uses one; the turn doesn't speculate in reset mode.
    if !shared.config.compaction_reset {
        if let Some(spec) = spec {
            if spec.prefix_end < spec.kept_from && spec.kept_from <= items.len() {
                let digest = vec![compaction::digest_item(&spec.text)];
                let payload = EntryPayload::Compaction {
                    digest: digest.clone(),
                    prefix_end: spec.prefix_end,
                    kept_from: spec.kept_from,
                    degraded: false,
                };
                if !shared.append(log, &payload) {
                    return Err("session log is sealed".into());
                }
                let plan = compaction::Plan {
                    prefix_end: spec.prefix_end,
                    kept_from: spec.kept_from,
                };
                *items = Arc::new(compaction::apply(items, &plan, &digest));
                return Ok(false);
            }
        }
    }
    let tail_budget = (shared.config.context_window as f64 * TAIL_RATIO) as u64;
    let Some(plan) = compaction::plan(items, tail_budget) else {
        return Err("context window exhausted — nothing left to compact".into());
    };
    // Reset mode (#9): fold *everything* after the preserved prefix into the
    // digest and keep no verbatim tail — the continuation is a fresh slate.
    // In-place mode (default): fold [prefix..kept_from) and keep the tail.
    let plan = if shared.config.compaction_reset {
        compaction::Plan {
            prefix_end: plan.prefix_end,
            kept_from: items.len(),
        }
    } else {
        plan
    };
    let folded = &items[plan.prefix_end..plan.kept_from];
    let (digest, degraded) = match summarize(shared, folded).await {
        Some(text) => (vec![compaction::digest_item(&text)], false),
        None => (vec![compaction::floor_digest()], true),
    };
    let payload = EntryPayload::Compaction {
        digest: digest.clone(),
        prefix_end: plan.prefix_end,
        kept_from: plan.kept_from,
        degraded,
    };
    if !shared.append(log, &payload) {
        return Err("session log is sealed".into());
    }
    *items = Arc::new(compaction::apply(items, &plan, &digest));
    Ok(degraded)
}

pub(crate) async fn summarize(shared: &SharedDeps, folded: &[Item]) -> Option<String> {
    let model = shared
        .config
        .fast_model
        .clone()
        .unwrap_or_else(|| shared.config.model.clone());
    let request = SamplingRequest {
        model,
        max_tokens: SUMMARIZE_MAX_TOKENS,
        system: compaction::SUMMARIZE_SYSTEM.into(),
        items: Arc::new(vec![Item::User {
            text: compaction::summarize_prompt(folded),
            synthetic: None,
        }]),
        tools: Vec::new().into(),
        thinking: false,
        cache_static: false,
        turn_context: None,
    };
    for _ in 0..SUMMARIZE_ATTEMPTS {
        let mut stream = shared.provider.stream(request.clone());
        let mut text: Option<String> = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::Completed { blocks, .. }) => text = Some(assistant_text(&blocks)),
                Ok(_) => {}
                Err(_) => {
                    text = None;
                    break;
                }
            }
        }
        if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
            return Some(t);
        }
    }
    None
}

/// Start a turn now, or queue the prompt if one is running (one-at-a-time
/// promotion). Carries an optional provenance tag (T2).
#[allow(clippy::too_many_arguments)]
async fn admit_prompt(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    queue: &mut VecDeque<(String, Option<SyntheticReason>)>,
    running: bool,
    text: String,
    synthetic: Option<SyntheticReason>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    if running {
        queue.push_back((text, synthetic));
        let _ = events.send(EngineEvent::PromptQueued).await;
        return true;
    }
    start_turn(
        shared,
        log,
        items,
        text,
        synthetic,
        cmd_tx,
        events,
        current_turn,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_turn(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    items: &mut Arc<Vec<Item>>,
    text: String,
    synthetic: Option<SyntheticReason>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    let payload = EntryPayload::Item {
        item: Item::User { text, synthetic },
    };
    if !shared.append(log, &payload) {
        let _ = events
            .send(EngineEvent::TurnDone {
                outcome: Outcome::Error {
                    message: "session log is sealed".into(),
                },
                usage: TokenUsage::default(),
            })
            .await;
        return false;
    }
    if let EntryPayload::Item { item } = payload {
        Arc::make_mut(items).push(item);
    }
    spawn_turn(shared, cmd_tx, events, current_turn);
    true
}

/// Spawn a fresh turn task against the current projection, installing a new
/// interrupt token for it.
fn spawn_turn(
    shared: &Arc<SharedDeps>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) {
    let token = CancellationToken::new();
    *current_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = token.clone();
    respawn_turn(shared, cmd_tx, events, token);
}

/// Spawn a turn task under an existing token. Compaction respawns use this
/// directly (no new user item, same logical turn — the interrupt token
/// carries over so a cancel during the fold still lands).
fn respawn_turn(
    shared: &Arc<SharedDeps>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    token: CancellationToken,
) {
    // The turn task holds a strong sender for its lifetime; a failed upgrade
    // means the handle is gone and there is nobody left to run for.
    let Some(cmd_tx) = cmd_tx.upgrade() else {
        return;
    };
    tokio::spawn(turn::run(shared.clone(), cmd_tx, events.clone(), token));
}

#[cfg(test)]
mod tests {
    use super::{awaiting_tool_results, pair_tool_results};
    use hotl_types::{Item, SyntheticReason, ToolResultItem};
    use serde_json::json;

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: Some(SyntheticReason::Steer),
        }
    }

    fn calls(id: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type": "tool_use", "id": id, "name": "read", "input": {}})],
        }
    }

    fn says(text: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type": "text", "text": text})],
        }
    }

    fn answers(id: &str) -> Item {
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn only_unanswered_tool_calls_hold_the_batch_open() {
        assert!(awaiting_tool_results(&[calls("t1")]));
        assert!(!awaiting_tool_results(&[says("hello")]));
        assert!(!awaiting_tool_results(&[calls("t1"), answers("t1")]));
        assert!(!awaiting_tool_results(&[]));
    }

    #[test]
    fn a_stranded_steer_moves_behind_the_results_it_interrupted() {
        let repaired = pair_tool_results(vec![calls("t1"), user("wait"), answers("t1")]);
        assert_eq!(repaired, vec![calls("t1"), answers("t1"), user("wait")]);
    }

    #[test]
    fn several_stranded_items_keep_their_order() {
        let repaired = pair_tool_results(vec![
            calls("t1"),
            user("one"),
            user("two"),
            answers("t1"),
            says("done"),
        ]);
        assert_eq!(
            repaired,
            vec![
                calls("t1"),
                answers("t1"),
                user("one"),
                user("two"),
                says("done"),
            ]
        );
    }

    #[test]
    fn already_paired_history_is_left_alone() {
        let good = vec![
            user("start"),
            calls("t1"),
            answers("t1"),
            user("next"),
            says("done"),
        ];
        assert_eq!(pair_tool_results(good.clone()), good);
    }

    #[test]
    fn a_gap_with_no_results_coming_is_not_reordered() {
        // Nothing answered t1, so there is no batch to move anything behind —
        // reordering here would only invent a new history.
        let orphaned = vec![calls("t1"), user("never answered"), says("moved on")];
        assert_eq!(pair_tool_results(orphaned.clone()), orphaned);
    }

    #[test]
    fn a_trailing_gap_survives_repair() {
        let trailing = vec![calls("t1"), user("last word")];
        assert_eq!(pair_tool_results(trailing.clone()), trailing);
    }
}
