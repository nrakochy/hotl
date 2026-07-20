//! One turn: sample → tools → sample, until a terminal outcome.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use hotl_provider::{retry, SamplingRequest, StreamEvent, ToolDef};
use hotl_tools::rules::Verdict;
use hotl_tools::{Permission, ToolOutcome};
use hotl_types::{
    assistant_text, assistant_tool_uses, EntryPayload, Item, StopReason, TokenUsage,
    ToolResultItem, ToolUse,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actor::SharedDeps;
use crate::{EngineEvent, Outcome, SessionCmd};

pub(crate) async fn run(
    shared: Arc<SharedDeps>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
) {
    let mut turn = Turn::new(&shared, cmd_tx.clone(), events, cancel);
    let outcome = turn.drive().await;
    let usage = turn.usage;
    let _ = cmd_tx.send(SessionCmd::TurnFinished { outcome, usage }).await;
}

/// A sample's terminal result, or why it couldn't produce one.
enum SampleEnd {
    Completed { stop: StopReason, blocks: Vec<Value> },
    Cancelled,
    /// Availability-class failure: eligible for a model fallback.
    Unavailable(String),
    Fatal(String),
}

struct Turn<'d> {
    shared: &'d SharedDeps,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
    tool_defs: Vec<ToolDef>,
    models: Vec<String>,
    model_idx: usize,
    call_sigs: Vec<String>,
    consecutive_failures: HashMap<String, u32>,
    usage: TokenUsage,
}

impl<'d> Turn<'d> {
    fn new(
        shared: &'d SharedDeps,
        cmd_tx: mpsc::Sender<SessionCmd>,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Self {
        let mut models = vec![shared.config.model.clone()];
        models.extend(shared.config.fallback_models.iter().cloned());
        Self {
            shared,
            cmd_tx,
            events,
            cancel,
            tool_defs: shared.registry.defs(),
            models,
            model_idx: 0,
            call_sigs: Vec::new(),
            consecutive_failures: HashMap::new(),
            usage: TokenUsage::default(),
        }
    }

    async fn drive(&mut self) -> Outcome {
        for _ in 0..self.shared.config.max_turns {
            let (stop, blocks) = match self.sample().await {
                SampleEnd::Completed { stop, blocks } => (stop, blocks),
                SampleEnd::Cancelled => return Outcome::Cancelled,
                SampleEnd::Unavailable(_) if self.model_idx + 1 < self.models.len() => {
                    self.model_idx += 1;
                    self.emit(EngineEvent::FallbackModel { model: self.models[self.model_idx].clone() })
                        .await;
                    continue;
                }
                SampleEnd::Unavailable(m) | SampleEnd::Fatal(m) => return Outcome::Error { message: m },
            };
            match stop {
                StopReason::ToolUse => {
                    if let Some(outcome) = self.run_tool_phase(&blocks).await {
                        return outcome;
                    }
                }
                StopReason::Refusal => return Outcome::Refused,
                _ => return Outcome::Done { text: assistant_text(&blocks) },
            }
        }
        Outcome::TurnLimit
    }

    /// One provider sample against a fresh snapshot; commits the assistant
    /// item + usage on completion.
    async fn sample(&mut self) -> SampleEnd {
        let Some(snapshot) = self.snapshot().await else {
            return SampleEnd::Fatal("session closed".into());
        };
        let request = SamplingRequest {
            model: self.models[self.model_idx].clone(),
            max_tokens: self.shared.config.max_tokens,
            system: self.shared.system.clone(),
            items: (*snapshot).clone(),
            tools: self.tool_defs.clone(),
            thinking: self.shared.config.thinking,
            cache_static: self.shared.config.cache_static,
        };

        let mut stream = self.shared.provider.stream(request);
        let mut completed = None;
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return SampleEnd::Cancelled,
                next = stream.next() => match next {
                    Some(Ok(event)) => {
                        if let StreamEvent::Completed { stop, usage, blocks } = event {
                            completed = Some((stop, usage, blocks));
                        } else {
                            self.forward(event).await;
                        }
                    }
                    Some(Err(e)) if retry::is_availability(&e) => return SampleEnd::Unavailable(e.to_string()),
                    Some(Err(e)) => return SampleEnd::Fatal(e.to_string()),
                    None => break,
                }
            }
        }
        let Some((stop, usage, blocks)) = completed else {
            return SampleEnd::Fatal("stream ended without completion".into());
        };
        self.usage += usage;
        let assistant = Item::Assistant { blocks: blocks.clone() };
        if !self
            .propose(vec![EntryPayload::Item { item: assistant }, EntryPayload::Usage { usage }])
            .await
        {
            return SampleEnd::Fatal("session log is sealed".into());
        }
        SampleEnd::Completed { stop, blocks }
    }

    /// Doom-loop guard, then the gated tool batch. `Some(outcome)` ends the turn.
    async fn run_tool_phase(&mut self, blocks: &[Value]) -> Option<Outcome> {
        let uses = assistant_tool_uses(blocks);
        for tu in &uses {
            self.call_sigs.push(format!("{}({})", tu.name, tu.input));
        }
        if let Some(pattern) = detect_doom_loop(&self.call_sigs) {
            if !self
                .ask(format!("the agent keeps repeating: {pattern} — let it continue?"), None)
                .await
            {
                self.abort_batch(&uses, "Stopped: the user declined to continue a repeating tool-call loop.")
                    .await;
                return Some(Outcome::DoomLoop { pattern });
            }
            self.call_sigs.clear();
        }
        self.run_tool_batch(&uses).await
    }

    /// Execute the batch in source order; every call gets a paired result.
    async fn run_tool_batch(&mut self, uses: &[ToolUse]) -> Option<Outcome> {
        let mut results = Vec::with_capacity(uses.len());
        let mut budget_blown: Option<String> = None;
        for tu in uses {
            if self.cancel.is_cancelled() || budget_blown.is_some() {
                results.push(pair(tu, "Not executed (turn stopped).", true));
                continue;
            }
            let outcome = self.execute_gated(tu).await;
            let (content, failed) = self.apply_failure_budget(tu, outcome, &mut budget_blown);
            results.push(ToolResultItem { tool_use_id: tu.id.clone(), content, is_error: failed });
        }
        let cancelled = self.cancel.is_cancelled();
        if !self.propose(vec![EntryPayload::Item { item: Item::ToolResults { results } }]).await {
            return Some(Outcome::Error { message: "session log is sealed".into() });
        }
        if cancelled {
            return Some(Outcome::Cancelled);
        }
        budget_blown.map(|tool| Outcome::ToolFailureBudget { tool })
    }

    /// Track per-tool consecutive failures; attach `<retry attempts_left>`
    /// feedback (Forge, corpus 11) and flag the budget when it hits zero.
    fn apply_failure_budget(
        &mut self,
        tu: &ToolUse,
        outcome: ToolOutcome,
        budget_blown: &mut Option<String>,
    ) -> (String, bool) {
        let mut content = outcome.content;
        if outcome.is_error {
            let n = self.consecutive_failures.entry(tu.name.clone()).or_insert(0);
            *n += 1;
            let left = self.shared.config.tool_failure_budget.saturating_sub(*n);
            content.push_str(&format!("\n<retry attempts_left={left}>"));
            if left == 0 {
                *budget_blown = Some(tu.name.clone());
            }
        } else {
            self.consecutive_failures.insert(tu.name.clone(), 0);
        }
        (content, outcome.is_error)
    }

    /// Permission (allow-rules first, then the human), then execution.
    async fn execute_gated(&mut self, tu: &ToolUse) -> ToolOutcome {
        let Some(tool) = self.shared.registry.get(&tu.name) else {
            return unknown_tool(&self.tool_defs, &tu.name);
        };
        let (summary, why) = match tool.permission(&tu.input) {
            Permission::None => (None, None),
            Permission::Ask { summary } => (Some(summary), None),
            Permission::AskProtected { summary, why } => (Some(summary), Some(why)),
        };
        if let Some(summary) = &summary {
            if !self.approve(tu, summary.clone(), why).await {
                self.emit(EngineEvent::ToolDenied { name: tu.name.clone() }).await;
                return ToolOutcome::err(
                    "The user declined this tool call. Ask what they'd like to do instead, or proceed another way.",
                );
            }
        }
        self.emit(EngineEvent::ToolStart {
            name: tu.name.clone(),
            summary: summary.unwrap_or_else(|| tu.name.clone()),
        })
        .await;
        let outcome = tool.run(tu.input.clone(), self.cancel.clone()).await;
        self.emit(EngineEvent::ToolDone { name: tu.name.clone(), ok: !outcome.is_error }).await;
        outcome
    }

    /// Allow-rules (deny-first, sandbox-gated, protected carve-out) or the ask.
    async fn approve(&mut self, tu: &ToolUse, summary: String, why: Option<String>) -> bool {
        let protected = why.is_some();
        match self.shared.rules.evaluate(&tu.name, &tu.input, self.shared.sandbox_enforced, protected) {
            Verdict::Auto { rule } => {
                self.emit(EngineEvent::ToolAutoAllowed { name: tu.name.clone(), rule }).await;
                true
            }
            Verdict::Ask => self.ask(summary, why).await,
        }
    }

    /// Complete protocol pairing for a batch that will not execute.
    async fn abort_batch(&mut self, uses: &[ToolUse], message: &str) {
        let results = uses.iter().map(|tu| pair(tu, message, true)).collect();
        self.propose(vec![EntryPayload::Item { item: Item::ToolResults { results } }]).await;
    }

    async fn snapshot(&self) -> Option<Arc<Vec<Item>>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(SessionCmd::Snapshot { reply: tx }).await.ok()?;
        rx.await.ok()
    }

    async fn propose(&self, entries: Vec<EntryPayload>) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(SessionCmd::Propose { entries, reply: tx }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Ask the human via the event channel; a dropped reply means deny.
    async fn ask(&self, summary: String, why: Option<String>) -> bool {
        let (tx, rx) = oneshot::channel();
        let event = EngineEvent::Ask { summary, protected_why: why, reply: tx };
        if self.events.send(event).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
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

fn pair(tu: &ToolUse, message: &str, is_error: bool) -> ToolResultItem {
    ToolResultItem { tool_use_id: tu.id.clone(), content: message.to_string(), is_error }
}

fn unknown_tool(defs: &[ToolDef], name: &str) -> ToolOutcome {
    let available: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
    ToolOutcome::err(format!("Unknown tool `{name}`. Available tools: {}.", available.join(", ")))
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
