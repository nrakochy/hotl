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
const TAIL_RATIO: f64 = 0.3;
const SUMMARIZE_ATTEMPTS: u32 = 2;
const SUMMARIZE_MAX_TOKENS: u32 = 2_000;
/// Compactions without an intervening completed sample before giving up —
/// prevents a fold-the-digest spiral when the tail alone overflows.
const MAX_COMPACT_STREAK: u32 = 2;

/// Dependencies shared with turn tasks. The log lives behind a mutex but is
/// only ever touched from the actor loop — the mutex exists to make the
/// struct `Sync` for the spawned turns that never use it.
pub(crate) struct SharedDeps {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub rules: Arc<Rules>,
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub system: String,
    pub cwd: PathBuf,
    pub config: EngineConfig,
    pub snapshots: Option<Arc<dyn crate::Snapshotter>>,
    log: Mutex<SessionLog>,
}

impl SharedDeps {
    fn new(deps: SessionDeps) -> Self {
        Self {
            provider: deps.provider,
            registry: deps.registry,
            rules: deps.rules,
            sandbox_enforced: deps.sandbox_enforced,
            clock: deps.clock,
            system: deps.system,
            cwd: deps.cwd,
            config: deps.config,
            snapshots: deps.snapshots,
            log: Mutex::new(deps.log),
        }
    }

    /// Durable append (flush inside `SessionLog`); false = log sealed.
    /// The failure surfaces to the user via the turn outcome, not stderr.
    fn append(&self, payload: EntryPayload) -> bool {
        let now = self.clock.now_ms();
        self.log.lock().expect("log mutex").append(payload, now).is_ok()
    }
}

pub(crate) async fn run(
    mut deps: SessionDeps,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
) {
    let mut items: Vec<Item> = std::mem::take(&mut deps.initial_items);
    let mut running = false;
    let mut queue: VecDeque<String> = VecDeque::new();
    let shared = Arc::new(SharedDeps::new(deps));
    // Usage carried across compaction respawns within one logical turn.
    let mut carry_usage = TokenUsage::default();
    let mut compact_streak: u32 = 0;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCmd::Prompt(text) => {
                if running {
                    queue.push_back(text);
                    let _ = events.send(EngineEvent::PromptQueued).await;
                } else {
                    running = start_turn(&shared, &mut items, text, &cmd_tx, &events, &current_turn).await;
                }
            }
            SessionCmd::Continue => {
                if !running && crate::needs_continuation(&items) {
                    spawn_turn(&shared, &cmd_tx, &events, &current_turn);
                    running = true;
                }
            }
            SessionCmd::Steer(text) => admit_steer(&shared, &mut items, text),
            SessionCmd::Snapshot { reply } => {
                let _ = reply.send(Arc::new(items.clone()));
            }
            SessionCmd::Propose { entries, reply } => {
                let _ = reply.send(commit(&shared, &mut items, entries));
            }
            SessionCmd::TurnFinished { end, mut usage } => {
                let outcome = match end {
                    TurnEnd::Outcome(outcome) => Some(outcome),
                    TurnEnd::Compact => {
                        carry_usage += usage;
                        usage = TokenUsage::default();
                        try_compact(&shared, &mut items, &mut compact_streak, &cmd_tx, &events, &current_turn)
                            .await
                    }
                };
                if let Some(outcome) = outcome {
                    compact_streak = 0;
                    let mut total = usage;
                    total += std::mem::take(&mut carry_usage);
                    running =
                        end_turn(&shared, &mut items, &mut queue, outcome, total, &cmd_tx, &events, &current_turn)
                            .await;
                }
            }
        }
    }
}

/// One compaction attempt on behalf of a turn that hit the threshold: fold,
/// announce, respawn the continuation. `Some(outcome)` means compaction can't
/// proceed (streak cap, nothing to fold, sealed log) and the turn ends.
async fn try_compact(
    shared: &Arc<SharedDeps>,
    items: &mut Vec<Item>,
    compact_streak: &mut u32,
    cmd_tx: &mpsc::Sender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> Option<Outcome> {
    *compact_streak += 1;
    let compacted = if *compact_streak > MAX_COMPACT_STREAK {
        Err("context window exhausted — compaction can no longer make room".into())
    } else {
        compact(shared, items).await
    };
    match compacted {
        Ok(degraded) => {
            let _ = events.send(EngineEvent::Compacted { degraded }).await;
            spawn_turn(shared, cmd_tx, events, current_turn);
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
    items: &mut Vec<Item>,
    queue: &mut VecDeque<String>,
    outcome: Outcome,
    usage: TokenUsage,
    cmd_tx: &mpsc::Sender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    annotate(shared, &outcome);
    let _ = events.send(EngineEvent::TurnDone { outcome, usage }).await;
    match queue.pop_front() {
        Some(next) => start_turn(shared, items, next, cmd_tx, events, current_turn).await,
        None => false,
    }
}

/// Durable admission on arrival; projection advances only after the append
/// (commit-protocol §durability). Linear-log M1 records the steer as a
/// tagged user item; the `steer_admission` entry kind arrives with M3b's tree.
fn admit_steer(shared: &SharedDeps, items: &mut Vec<Item>, text: String) {
    let item = Item::User { text, synthetic: Some(SyntheticReason::Steer) };
    if shared.append(EntryPayload::Item { item: item.clone() }) {
        items.push(item);
    }
}

/// Commit a proposal: append each entry durably, then project it.
fn commit(shared: &SharedDeps, items: &mut Vec<Item>, entries: Vec<EntryPayload>) -> bool {
    for payload in entries {
        if !shared.append(payload.clone()) {
            return false;
        }
        if let EntryPayload::Item { item } = payload {
            items.push(item);
        }
    }
    true
}

/// Non-Done outcomes leave a durable annotation in the log.
fn annotate(shared: &SharedDeps, outcome: &Outcome) {
    let reason = match outcome {
        Outcome::Cancelled => Some("user interrupt".to_string()),
        Outcome::TurnLimit => Some(format!("max_turns ({}) reached", shared.config.max_turns)),
        Outcome::DoomLoop { pattern } => Some(format!("doom loop: {pattern}")),
        Outcome::ToolFailureBudget { tool } => Some(format!("tool failure budget: {tool}")),
        Outcome::Error { message } => Some(format!("error: {message}")),
        Outcome::Done { .. } | Outcome::Refused => None,
    };
    if let Some(reason) = reason {
        shared.append(EntryPayload::Cancelled { reason });
    }
}

/// Compact the projection (M2): fold `[prefix..kept_from)` into a typed
/// digest via the fast model, floor to a placeholder if summarize fails, and
/// re-point the projection with an appended `compaction` entry — the log
/// keeps everything. Runs inline in the actor: no turn is in flight, and
/// admission blocking during the summarize call is the serialization working
/// as designed.
async fn compact(shared: &SharedDeps, items: &mut Vec<Item>) -> Result<bool, String> {
    let tail_budget = (shared.config.context_window as f64 * TAIL_RATIO) as u64;
    let Some(plan) = compaction::plan(items, tail_budget) else {
        return Err("context window exhausted — nothing left to compact".into());
    };
    // Reset mode (#9): fold *everything* after the preserved prefix into the
    // digest and keep no verbatim tail — the continuation is a fresh slate.
    // In-place mode (default): fold [prefix..kept_from) and keep the tail.
    let plan = if shared.config.compaction_reset {
        compaction::Plan { prefix_end: plan.prefix_end, kept_from: items.len() }
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
    if !shared.append(payload) {
        return Err("session log is sealed".into());
    }
    *items = compaction::apply(items, &plan, &digest);
    Ok(degraded)
}

async fn summarize(shared: &SharedDeps, folded: &[Item]) -> Option<String> {
    let model =
        shared.config.fast_model.clone().unwrap_or_else(|| shared.config.model.clone());
    let request = SamplingRequest {
        model,
        max_tokens: SUMMARIZE_MAX_TOKENS,
        system: compaction::SUMMARIZE_SYSTEM.into(),
        items: vec![Item::User { text: compaction::summarize_prompt(folded), synthetic: None }],
        tools: Vec::new(),
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

async fn start_turn(
    shared: &Arc<SharedDeps>,
    items: &mut Vec<Item>,
    text: String,
    cmd_tx: &mpsc::Sender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    let item = Item::User { text, synthetic: None };
    if !shared.append(EntryPayload::Item { item: item.clone() }) {
        let _ = events
            .send(EngineEvent::TurnDone {
                outcome: Outcome::Error { message: "session log is sealed".into() },
                usage: TokenUsage::default(),
            })
            .await;
        return false;
    }
    items.push(item);
    spawn_turn(shared, cmd_tx, events, current_turn);
    true
}

/// Spawn a turn task against the current projection. Continuation respawns
/// after a compaction use this directly — no new user item is appended.
fn spawn_turn(
    shared: &Arc<SharedDeps>,
    cmd_tx: &mpsc::Sender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) {
    let token = CancellationToken::new();
    *current_turn.lock().expect("turn token mutex") = token.clone();
    tokio::spawn(turn::run(shared.clone(), cmd_tx.clone(), events.clone(), token));
}
