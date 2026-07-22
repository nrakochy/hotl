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
    let mut items: Arc<Vec<Item>> = Arc::new(std::mem::take(&mut deps.initial_items));
    let mut running = false;
    let mut queue: VecDeque<(String, Option<SyntheticReason>)> = VecDeque::new();
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
            SessionCmd::Steer(text) => admit_steer(&shared, &mut log, &mut items, text),
            SessionCmd::Snapshot { reply } => {
                let _ = reply.send(Arc::clone(&items));
            }
            SessionCmd::Propose { entries, reply } => {
                let _ = reply.send(commit(&shared, &mut log, &mut items, entries));
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

/// Durable admission on arrival; projection advances only after the append
/// (commit-protocol §durability). Linear-log M1 records the steer as a
/// tagged user item; the `steer_admission` entry kind arrives with M3b's tree.
fn admit_steer(
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
