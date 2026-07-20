//! L3 — the turn engine, M1: actor + turn tasks (commit-protocol.md).
//!
//! One **session actor** per session is the sole committer to the log and the
//! owner of the projection (the live `Vec<Item>`). **Turn tasks** read
//! actor-granted snapshots at sample boundaries and *propose* entries; the
//! actor appends durably (flush before projection advance) and only then
//! acks. Steers are admitted the moment they arrive and get woven into the
//! turn's next sample via the snapshot refresh — the "rebase" row of the
//! conflict table. Compaction/branch-move (the "abort" rows) don't exist yet;
//! the protocol shape is in place for them.
//!
//! Interrupts travel out-of-band (a shared `CancellationToken`, never the
//! command mailbox). Permission asks are events carrying a oneshot reply, so
//! the surface answers them without the loop holding any I/O.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use hotl_platform::Clock;
use hotl_provider::{retry, Provider, SamplingRequest, StreamEvent, ToolDef};
use hotl_store::SessionLog;
use hotl_tools::rules::{Rules, Verdict};
use hotl_tools::{Permission, Registry, ToolOutcome};
use hotl_types::{
    assistant_text, assistant_tool_uses, EntryPayload, Item, StopReason, SyntheticReason,
    TokenUsage, ToolResultItem,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

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
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done { text: String },
    Cancelled,
    TurnLimit,
    Refused,
    DoomLoop { pattern: String },
    /// One tool failed `tool_failure_budget` times in a row.
    ToolFailureBudget { tool: String },
    Error { message: String },
}

/// Everything the surface renders. `Ask` carries the reply channel — the
/// surface (or an allow-rule upstream) is the human on the loop.
pub enum EngineEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart { name: String, summary: String },
    ToolDone { name: String, ok: bool },
    ToolDenied { name: String },
    ToolAutoAllowed { name: String, rule: String },
    Retrying { attempt: u32, reason: String },
    FallbackModel { model: String },
    PromptQueued,
    Ask { summary: String, protected_why: Option<String>, reply: oneshot::Sender<bool> },
    TurnDone { outcome: Outcome, usage: TokenUsage },
}

impl std::fmt::Debug for EngineEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineEvent::TextDelta(t) => write!(f, "TextDelta({t:?})"),
            EngineEvent::ThinkingDelta(_) => write!(f, "ThinkingDelta"),
            EngineEvent::ToolStart { name, .. } => write!(f, "ToolStart({name})"),
            EngineEvent::ToolDone { name, ok } => write!(f, "ToolDone({name},{ok})"),
            EngineEvent::ToolDenied { name } => write!(f, "ToolDenied({name})"),
            EngineEvent::ToolAutoAllowed { name, rule } => write!(f, "ToolAutoAllowed({name},{rule})"),
            EngineEvent::Retrying { attempt, .. } => write!(f, "Retrying({attempt})"),
            EngineEvent::FallbackModel { model } => write!(f, "FallbackModel({model})"),
            EngineEvent::PromptQueued => write!(f, "PromptQueued"),
            EngineEvent::Ask { summary, .. } => write!(f, "Ask({summary})"),
            EngineEvent::TurnDone { outcome, .. } => write!(f, "TurnDone({outcome:?})"),
        }
    }
}

pub enum SessionCmd {
    /// A user prompt. Starts a turn, or queues (one-at-a-time promotion).
    Prompt(String),
    /// Mid-turn guidance: admitted durably now, woven into the next sample.
    Steer(String),
    /// Turn task → actor: sample-boundary snapshot refresh.
    Snapshot { reply: oneshot::Sender<Arc<Vec<Item>>> },
    /// Turn task → actor: commit these entries (durable-ack before reply).
    Propose { entries: Vec<EntryPayload>, reply: oneshot::Sender<bool> },
    /// Turn task → actor: the turn is over.
    TurnFinished { outcome: Outcome, usage: TokenUsage },
}

pub struct SessionDeps {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub rules: Arc<Rules>,
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub log: SessionLog,
    pub system: String,
    pub initial_items: Vec<Item>,
    pub config: EngineConfig,
}

pub struct SessionHandle {
    cmd: mpsc::Sender<SessionCmd>,
    pub events: mpsc::Receiver<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
}

impl SessionHandle {
    pub async fn prompt(&self, text: String) {
        let _ = self.cmd.send(SessionCmd::Prompt(text)).await;
    }
    pub async fn steer(&self, text: String) {
        let _ = self.cmd.send(SessionCmd::Steer(text)).await;
    }
    /// Out-of-band interrupt of the in-flight turn (never queued behind data).
    pub fn interrupt(&self) {
        self.current_turn.lock().expect("turn token mutex").cancel();
    }
}

pub fn spawn_session(deps: SessionDeps) -> SessionHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let current_turn = Arc::new(Mutex::new(CancellationToken::new()));
    tokio::spawn(actor(deps, cmd_rx, cmd_tx.clone(), event_tx, current_turn.clone()));
    SessionHandle { cmd: cmd_tx, events: event_rx, current_turn }
}

async fn actor(
    mut deps: SessionDeps,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
) {
    let mut items: Vec<Item> = std::mem::take(&mut deps.initial_items);
    let mut running = false;
    let mut queue: VecDeque<String> = VecDeque::new();
    let deps = Arc::new(SharedDeps {
        provider: deps.provider.clone(),
        registry: deps.registry.clone(),
        rules: deps.rules.clone(),
        sandbox_enforced: deps.sandbox_enforced,
        clock: deps.clock.clone(),
        system: deps.system.clone(),
        config: deps.config.clone(),
        log: Mutex::new(deps.log),
    });

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCmd::Prompt(text) => {
                if running {
                    queue.push_back(text);
                    let _ = events.send(EngineEvent::PromptQueued).await;
                } else if start_turn(&deps, &mut items, text, &cmd_tx, &events, &current_turn).await {
                    running = true;
                }
            }
            SessionCmd::Steer(text) => {
                // Durable admission on arrival; the projection advances only
                // after the append succeeds (commit-protocol §durability).
                // Linear-log M1: recorded as a Steer-tagged user item (the
                // steer_admission entry kind arrives with the tree log, M3b).
                let item = Item::User { text, synthetic: Some(SyntheticReason::Steer) };
                if deps.append(EntryPayload::Item { item: item.clone() }) {
                    items.push(item);
                }
            }
            SessionCmd::Snapshot { reply } => {
                let _ = reply.send(Arc::new(items.clone()));
            }
            SessionCmd::Propose { entries, reply } => {
                let mut ok = true;
                for payload in entries {
                    if !deps.append(payload.clone()) {
                        ok = false;
                        break;
                    }
                    if let EntryPayload::Item { item } = payload {
                        items.push(item);
                    }
                }
                let _ = reply.send(ok);
            }
            SessionCmd::TurnFinished { outcome, usage } => {
                running = false;
                // Non-Done outcomes leave a durable annotation.
                let note = match &outcome {
                    Outcome::Cancelled => Some("user interrupt".to_string()),
                    Outcome::TurnLimit => Some(format!("max_turns ({}) reached", deps.config.max_turns)),
                    Outcome::DoomLoop { pattern } => Some(format!("doom loop: {pattern}")),
                    Outcome::ToolFailureBudget { tool } => Some(format!("tool failure budget: {tool}")),
                    Outcome::Error { message } => Some(format!("error: {message}")),
                    _ => None,
                };
                if let Some(reason) = note {
                    deps.append(EntryPayload::Cancelled { reason });
                }
                let _ = events.send(EngineEvent::TurnDone { outcome, usage }).await;
                if let Some(next) = queue.pop_front() {
                    if start_turn(&deps, &mut items, next, &cmd_tx, &events, &current_turn).await {
                        running = true;
                    }
                }
            }
        }
    }
}

struct SharedDeps {
    provider: Arc<dyn Provider>,
    registry: Arc<Registry>,
    rules: Arc<Rules>,
    sandbox_enforced: bool,
    clock: Arc<dyn Clock>,
    system: String,
    config: EngineConfig,
    log: Mutex<SessionLog>,
}

impl SharedDeps {
    /// Durable append (flush inside `SessionLog`); false = log sealed.
    fn append(&self, payload: EntryPayload) -> bool {
        let now = self.clock.now_ms();
        let mut log = self.log.lock().expect("log mutex");
        match log.append(payload, now) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("hotl: session log write failed ({e}) — session is sealed read-only");
                false
            }
        }
    }
}

async fn start_turn(
    deps: &Arc<SharedDeps>,
    items: &mut Vec<Item>,
    text: String,
    cmd_tx: &mpsc::Sender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    let item = Item::User { text, synthetic: None };
    if !deps.append(EntryPayload::Item { item: item.clone() }) {
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
    tokio::spawn(turn_task(deps.clone(), cmd_tx.clone(), events.clone(), token));
    true
}

/// Ask the human via the event channel; a dropped reply means deny.
async fn ask(events: &mpsc::Sender<EngineEvent>, summary: String, why: Option<String>) -> bool {
    let (tx, rx) = oneshot::channel();
    if events
        .send(EngineEvent::Ask { summary, protected_why: why, reply: tx })
        .await
        .is_err()
    {
        return false;
    }
    rx.await.unwrap_or(false)
}

async fn turn_task(
    deps: Arc<SharedDeps>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
) {
    let (outcome, usage) = run_turn(&deps, &cmd_tx, &events, &cancel).await;
    let _ = cmd_tx.send(SessionCmd::TurnFinished { outcome, usage }).await;
}

async fn snapshot(cmd_tx: &mpsc::Sender<SessionCmd>) -> Option<Arc<Vec<Item>>> {
    let (tx, rx) = oneshot::channel();
    cmd_tx.send(SessionCmd::Snapshot { reply: tx }).await.ok()?;
    rx.await.ok()
}

async fn propose(cmd_tx: &mpsc::Sender<SessionCmd>, entries: Vec<EntryPayload>) -> bool {
    let (tx, rx) = oneshot::channel();
    if cmd_tx.send(SessionCmd::Propose { entries, reply: tx }).await.is_err() {
        return false;
    }
    rx.await.unwrap_or(false)
}

async fn run_turn(
    deps: &SharedDeps,
    cmd_tx: &mpsc::Sender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    cancel: &CancellationToken,
) -> (Outcome, TokenUsage) {
    let cfg = &deps.config;
    let tool_defs: Vec<ToolDef> = deps.registry.defs();
    let mut total_usage = TokenUsage::default();
    let mut call_sigs: Vec<String> = Vec::new();
    let mut consecutive_failures: HashMap<String, u32> = HashMap::new();
    let mut model_chain: Vec<&String> = vec![&cfg.model];
    model_chain.extend(cfg.fallback_models.iter());
    let mut chain_idx = 0usize;

    for _turn in 0..cfg.max_turns {
        // Sample-boundary snapshot refresh: steers admitted since the last
        // sample are in this projection (conflict table: rebase).
        let Some(snap) = snapshot(cmd_tx).await else {
            return (Outcome::Error { message: "session closed".into() }, total_usage);
        };
        let req = SamplingRequest {
            model: model_chain[chain_idx].clone(),
            max_tokens: cfg.max_tokens,
            system: deps.system.clone(),
            items: (*snap).clone(),
            tools: tool_defs.clone(),
            thinking: cfg.thinking,
            cache_static: cfg.cache_static,
        };

        let mut stream = deps.provider.stream(req);
        let mut completed: Option<(StopReason, TokenUsage, Vec<serde_json::Value>)> = None;
        let sample_result: Result<(), hotl_provider::ProviderError> = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return (Outcome::Cancelled, total_usage),
                next = stream.next() => match next {
                    Some(Ok(StreamEvent::TextDelta { text, .. })) => { let _ = events.send(EngineEvent::TextDelta(text)).await; }
                    Some(Ok(StreamEvent::ThinkingDelta { text, .. })) => { let _ = events.send(EngineEvent::ThinkingDelta(text)).await; }
                    Some(Ok(StreamEvent::Retrying { attempt, reason })) => { let _ = events.send(EngineEvent::Retrying { attempt, reason }).await; }
                    Some(Ok(StreamEvent::Completed { stop, usage, blocks })) => completed = Some((stop, usage, blocks)),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(e),
                    None => break Ok(()),
                }
            }
        };

        match sample_result {
            Err(e) if retry::is_availability(&e) && chain_idx + 1 < model_chain.len() => {
                chain_idx += 1;
                let _ = events
                    .send(EngineEvent::FallbackModel { model: model_chain[chain_idx].clone() })
                    .await;
                continue;
            }
            Err(e) => return (Outcome::Error { message: e.to_string() }, total_usage),
            Ok(()) => {}
        }
        let Some((stop, usage, blocks)) = completed else {
            return (Outcome::Error { message: "stream ended without completion".into() }, total_usage);
        };
        total_usage += usage;

        let assistant = Item::Assistant { blocks: blocks.clone() };
        if !propose(
            cmd_tx,
            vec![EntryPayload::Item { item: assistant }, EntryPayload::Usage { usage }],
        )
        .await
        {
            return (Outcome::Error { message: "session log is sealed".into() }, total_usage);
        }

        match stop {
            StopReason::ToolUse => {
                let uses = assistant_tool_uses(&blocks);
                for tu in &uses {
                    call_sigs.push(format!("{}({})", tu.name, tu.input));
                }
                if let Some(pattern) = detect_doom_loop(&call_sigs) {
                    let allowed = ask(
                        events,
                        format!("the agent keeps repeating: {pattern} — let it continue?"),
                        None,
                    )
                    .await;
                    if !allowed {
                        let results = uses
                            .iter()
                            .map(|tu| ToolResultItem {
                                tool_use_id: tu.id.clone(),
                                content: "Stopped: the user declined to continue a repeating tool-call loop.".into(),
                                is_error: true,
                            })
                            .collect();
                        let _ = propose(cmd_tx, vec![EntryPayload::Item { item: Item::ToolResults { results } }]).await;
                        return (Outcome::DoomLoop { pattern }, total_usage);
                    }
                    call_sigs.clear();
                }

                let mut results = Vec::with_capacity(uses.len());
                let mut budget_blown: Option<String> = None;
                for (i, tu) in uses.iter().enumerate() {
                    if cancel.is_cancelled() || budget_blown.is_some() {
                        results.push(ToolResultItem {
                            tool_use_id: tu.id.clone(),
                            content: "Not executed (turn stopped).".into(),
                            is_error: true,
                        });
                        continue;
                    }
                    let outcome = execute_gated(deps, events, tu, cancel).await;
                    let failed = outcome.is_error;
                    let mut content = outcome.content;
                    if failed {
                        let n = consecutive_failures.entry(tu.name.clone()).or_insert(0);
                        *n += 1;
                        let left = cfg.tool_failure_budget.saturating_sub(*n);
                        content.push_str(&format!("\n<retry attempts_left={left}>"));
                        if left == 0 {
                            budget_blown = Some(tu.name.clone());
                        }
                    } else {
                        consecutive_failures.insert(tu.name.clone(), 0);
                    }
                    results.push(ToolResultItem { tool_use_id: tu.id.clone(), content, is_error: failed });
                    let _ = i;
                }
                let cancelled = cancel.is_cancelled();
                if !propose(cmd_tx, vec![EntryPayload::Item { item: Item::ToolResults { results } }]).await {
                    return (Outcome::Error { message: "session log is sealed".into() }, total_usage);
                }
                if cancelled {
                    return (Outcome::Cancelled, total_usage);
                }
                if let Some(tool) = budget_blown {
                    return (Outcome::ToolFailureBudget { tool }, total_usage);
                }
            }
            StopReason::Refusal => return (Outcome::Refused, total_usage),
            _ => return (Outcome::Done { text: assistant_text(&blocks) }, total_usage),
        }
    }
    (Outcome::TurnLimit, total_usage)
}

async fn execute_gated(
    deps: &SharedDeps,
    events: &mpsc::Sender<EngineEvent>,
    tu: &hotl_types::ToolUse,
    cancel: &CancellationToken,
) -> ToolOutcome {
    let Some(tool) = deps.registry.get(&tu.name) else {
        return ToolOutcome::err(format!(
            "Unknown tool `{}`. Available tools: {}.",
            tu.name,
            deps.registry.defs().iter().map(|d| d.name.clone()).collect::<Vec<_>>().join(", ")
        ));
    };
    let (needs_ask, summary, why) = match tool.permission(&tu.input) {
        Permission::None => (false, String::new(), None),
        Permission::Ask { summary } => (true, summary, None),
        Permission::AskProtected { summary, why } => (true, summary, Some(why)),
    };
    if needs_ask {
        let protected = why.is_some();
        match deps.rules.evaluate(&tu.name, &tu.input, deps.sandbox_enforced, protected) {
            Verdict::Auto { rule } => {
                let _ = events
                    .send(EngineEvent::ToolAutoAllowed { name: tu.name.clone(), rule })
                    .await;
            }
            Verdict::Ask => {
                if !ask(events, summary.clone(), why).await {
                    let _ = events.send(EngineEvent::ToolDenied { name: tu.name.clone() }).await;
                    return ToolOutcome::err(
                        "The user declined this tool call. Ask what they'd like to do instead, or proceed another way.",
                    );
                }
            }
        }
    }
    let _ = events
        .send(EngineEvent::ToolStart {
            name: tu.name.clone(),
            summary: if summary.is_empty() { tu.name.clone() } else { summary },
        })
        .await;
    let outcome = tool.run(tu.input.clone(), cancel.clone()).await;
    let _ = events
        .send(EngineEvent::ToolDone { name: tu.name.clone(), ok: !outcome.is_error })
        .await;
    outcome
}

/// Repeating suffix patterns over tool-call signatures: any period p ≤ 3
/// whose block repeats 3× at the tail (Forge's detector, corpus 11).
fn detect_doom_loop(sigs: &[String]) -> Option<String> {
    const REPEATS: usize = 3;
    for period in 1..=3usize {
        let need = period * REPEATS;
        if sigs.len() < need {
            continue;
        }
        let tail = &sigs[sigs.len() - need..];
        let block = &tail[..period];
        if tail.chunks(period).all(|c| c == block) {
            return Some(block.join(" → "));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doom_detector_finds_periods() {
        let a = "read({\"path\":\"x\"})".to_string();
        let b = "bash({\"command\":\"ls\"})".to_string();
        assert!(detect_doom_loop(&[a.clone(), a.clone(), a.clone()]).is_some());
        let sigs = vec![a.clone(), b.clone(), a.clone(), b.clone(), a.clone(), b.clone()];
        assert!(detect_doom_loop(&sigs).is_some());
        assert!(detect_doom_loop(&[a.clone(), a.clone(), b.clone()]).is_none());
        assert!(detect_doom_loop(&[a.clone(), a.clone()]).is_none());
    }
}
