//! The session actor: sole committer, projection owner, turn scheduler.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use hotl_platform::Clock;
use hotl_provider::Provider;
use hotl_store::SessionLog;
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, SyntheticReason, TokenUsage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{turn, EngineConfig, EngineEvent, Outcome, SessionCmd, SessionDeps};

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
    pub config: EngineConfig,
    log: Mutex<SessionLog>,
}

impl SharedDeps {
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
    let shared = Arc::new(SharedDeps {
        provider: deps.provider,
        registry: deps.registry,
        rules: deps.rules,
        sandbox_enforced: deps.sandbox_enforced,
        clock: deps.clock,
        system: deps.system,
        config: deps.config,
        log: Mutex::new(deps.log),
    });

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
            SessionCmd::Steer(text) => admit_steer(&shared, &mut items, text),
            SessionCmd::Snapshot { reply } => {
                let _ = reply.send(Arc::new(items.clone()));
            }
            SessionCmd::Propose { entries, reply } => {
                let _ = reply.send(commit(&shared, &mut items, entries));
            }
            SessionCmd::TurnFinished { outcome, usage } => {
                annotate(&shared, &outcome);
                running = false;
                let _ = events.send(EngineEvent::TurnDone { outcome, usage }).await;
                if let Some(next) = queue.pop_front() {
                    running = start_turn(&shared, &mut items, next, &cmd_tx, &events, &current_turn).await;
                }
            }
        }
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
    let token = CancellationToken::new();
    *current_turn.lock().expect("turn token mutex") = token.clone();
    tokio::spawn(turn::run(shared.clone(), cmd_tx.clone(), events.clone(), token));
    true
}
