//! One turn: sample → tools → sample, until a terminal outcome.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use hotl_provider::{retry, SamplingRequest, StreamEvent, ToolDef};
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
use crate::{AskReply, EngineEvent, Outcome, SessionCmd, TurnEnd};

/// Compaction triggers when the estimated next request crosses this share of
/// the window (M2; the estimate overcounts, so the miss direction is early).
const COMPACT_TRIGGER: f64 = 0.8;
/// Speculative compaction starts here: the digest summarize fires in the
/// background so it is (usually) already done when [`COMPACT_TRIGGER`] hits,
/// and the fold needs no blocking model call.
const SPECULATE_TRIGGER: f64 = 0.6;

/// Shared per-prompt budget for "intercept the end-turn, inject a reminder,
/// continue" gates (index E4): the TodoGate and the `Stop` hook veto
/// (`Turn::consult_stop`) both draw from this same counter (see
/// `Turn::turn_extensions`) rather than each having its own, so composing
/// gates can't multiply worst-case turn extensions — a turn is extended at
/// most `TURN_EXTENSION_MAX` times total, ever, regardless of which gate(s)
/// fired on any given pass. Capped above any single gate's own bound — it's
/// the *combined* ceiling.
const TURN_EXTENSION_MAX: u32 = 3;
/// The TodoGate's own bound within the shared budget (01 §agent-loop's
/// `max_fires_per_prompt`). It never blocks in `Auto`/`DontAsk` beyond this
/// — a gate that can wedge an unattended run is a bug.
const TODO_GATE_MAX: u32 = 2;

pub(crate) async fn run(
    shared: Arc<SharedDeps>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
) {
    let mut turn = Turn::new(shared, cmd_tx.clone(), events, cancel);
    let end = turn.drive().await;
    // A speculation the turn never consumed has no fold to serve — stop it.
    if let Some(handle) = turn.speculation.take() {
        handle.abort();
    }
    let usage = turn.usage;
    let _ = cmd_tx.send(SessionCmd::TurnFinished { end, usage }).await;
}

/// One gated call: ready to execute with its (possibly hook-rewritten or
/// human-edited) input, or already answered without running.
enum Gate {
    Ready { input: Value, summary: String },
    Resolved(ToolOutcome),
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
    snapshot: &Arc<Vec<Item>>,
) -> Option<tokio::task::JoinHandle<Option<crate::SpecDigest>>> {
    let tail_budget = (shared.config.context_window as f64 * crate::actor::TAIL_RATIO) as u64;
    let plan = hotl_context::compaction::plan(snapshot, tail_budget)?;
    let shared = Arc::clone(shared);
    let snapshot = Arc::clone(snapshot);
    Some(tokio::spawn(async move {
        let folded = &snapshot[plan.prefix_end..plan.kept_from];
        let text = crate::actor::summarize(&shared, folded).await?;
        Some(crate::SpecDigest {
            prefix_end: plan.prefix_end,
            kept_from: plan.kept_from,
            text,
        })
    }))
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
    consecutive_failures: HashMap<String, u32>,
    usage: TokenUsage,
    /// (provider-reported tokens, projection length) at the last completed
    /// sample — the anchor for context estimates (A12b).
    anchor: Option<(u64, usize)>,
    samples: u32,
    /// Subdir hints already injected (per turn) + the latest snapshot for
    /// cross-turn dedup against the projection.
    injected_hints: HashSet<String>,
    last_snapshot: Option<Arc<Vec<Item>>>,
    /// In-flight speculative compaction digest: fired once the estimate
    /// crosses [`SPECULATE_TRIGGER`], consumed when the turn ends in
    /// `Compact`, aborted otherwise.
    speculation: Option<tokio::task::JoinHandle<Option<crate::SpecDigest>>>,
    /// Shared per-prompt "reminder and continue" budget (index E4) — see
    /// [`TURN_EXTENSION_MAX`]. Reset per `drive()` call (one call == one
    /// turn); the TodoGate is the only consumer today, bounded further by
    /// [`TODO_GATE_MAX`].
    turn_extensions: u32,
}

impl Turn {
    fn new(
        shared: Arc<SharedDeps>,
        cmd_tx: mpsc::Sender<SessionCmd>,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Self {
        let mut models = vec![shared.config.model.clone()];
        models.extend(shared.config.fallback_models.iter().cloned());
        Self {
            tool_defs: shared.registry.defs().into(),
            shared,
            cmd_tx,
            events,
            cancel,
            models,
            model_idx: 0,
            call_sigs: VecDeque::new(),
            consecutive_failures: HashMap::new(),
            usage: TokenUsage::default(),
            anchor: None,
            samples: 0,
            injected_hints: HashSet::new(),
            last_snapshot: None,
            speculation: None,
            turn_extensions: 0,
        }
    }

    async fn drive(&mut self) -> TurnEnd {
        for _ in 0..self.shared.config.max_turns {
            let (stop, blocks) = match self.sample().await {
                SampleEnd::Completed { stop, blocks } => (stop, blocks),
                SampleEnd::Cancelled => return TurnEnd::Outcome(Outcome::Cancelled),
                SampleEnd::ContextFull => {
                    return TurnEnd::Compact {
                        spec: self.take_speculation().await,
                    }
                }
                SampleEnd::Unavailable(_) if self.model_idx + 1 < self.models.len() => {
                    self.model_idx += 1;
                    self.emit(EngineEvent::FallbackModel {
                        model: self.models[self.model_idx].clone(),
                    })
                    .await;
                    continue;
                }
                SampleEnd::Unavailable(m) | SampleEnd::Fatal(m) => {
                    return TurnEnd::Outcome(Outcome::Error { message: m })
                }
            };
            match stop {
                StopReason::ToolUse => {
                    if let Some(outcome) = self.run_tool_phase(&blocks).await {
                        return TurnEnd::Outcome(outcome);
                    }
                }
                StopReason::Refusal => return TurnEnd::Outcome(Outcome::Refused),
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
                        self.inject_gate_nudge(todo_fires, stop_reason).await;
                        continue;
                    }
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
                .as_deref()
                .is_some_and(|s| unfinished_todos(s))
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
        let hooks = self.shared.hooks.as_ref()?;
        match crate::hooks::call_stop(hooks, outcome_text).await {
            crate::hooks::StopDecision::Block { reason } => Some(reason),
            crate::hooks::StopDecision::Allow => None,
        }
    }

    /// Commit the Done-branch gate nudge(s) as ONE tagged `SystemReminder`
    /// user item (Innovation #7 — one injection per commit point): when both
    /// the TodoGate and a Stop-hook `Block` land in the same continuation,
    /// their sections ride inside a single `<system-reminder>`, in fixed
    /// order (TodoGate section first, Stop section second), never as two
    /// adjacent items.
    async fn inject_gate_nudge(&mut self, todo_fires: bool, stop_reason: Option<String>) -> bool {
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
        self.propose(vec![EntryPayload::Item {
            item: Item::User {
                text: format!("<system-reminder>{body}</system-reminder>"),
                synthetic: Some(SyntheticReason::SystemReminder),
            },
        }])
        .await
    }

    /// One provider sample against a fresh snapshot; commits the assistant
    /// item + usage on completion.
    async fn sample(&mut self) -> SampleEnd {
        let Some(snapshot) = self.snapshot().await else {
            return SampleEnd::Fatal("session closed".into());
        };
        self.samples += 1;
        let request = match self.build_request(&snapshot) {
            Ok(request) => request,
            Err(end) => return end,
        };
        self.last_snapshot = Some(snapshot.clone());

        let (stop, usage, blocks) = match self.collect_stream(request).await {
            Ok(completed) => completed,
            Err(end) => return end,
        };
        self.usage += usage;
        // Anchor: what the provider says this request cost, plus its output —
        // the base cost of the next request before any new items.
        let reported = usage.input_tokens
            + usage.cache_read_input_tokens
            + usage.cache_creation_input_tokens
            + usage.output_tokens;
        self.anchor = Some((reported, snapshot.len() + 1));
        let assistant = Item::Assistant {
            blocks: blocks.clone(),
        };
        if !self
            .propose(vec![
                EntryPayload::Item { item: assistant },
                EntryPayload::Usage { usage },
            ])
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
            self.call_sigs.push_back(CallSig::new(tu));
        }
        while self.call_sigs.len() > DOOM_WINDOW {
            self.call_sigs.pop_front();
        }
        if let Some(pattern) = detect_doom_loop(self.call_sigs.make_contiguous()) {
            // Auto mode has nobody watching: the doom guard is a malfunction
            // brake, not a permission — stop the turn instead of asking.
            let stop = if self.shared.effective_mode() == hotl_tools::rules::PermissionMode::Auto {
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
                self.abort_batch(&uses, "Stopped: a repeating tool-call loop was detected.")
                    .await;
                return Some(Outcome::DoomLoop { pattern });
            }
            self.call_sigs.clear();
        }
        self.run_tool_batch(&uses).await
    }

    /// Execute the batch with results paired in source order. The batch is
    /// split into chunks: contiguous runs of parallel-safe calls (pure reads,
    /// isolated children) execute concurrently; everything else runs alone,
    /// in order. Gating stays serial — asks are one-at-a-time human moments.
    /// Mutating batches (anything beyond `read`) are bracketed by shadow
    /// snapshots so `hotl undo` can restore the pre-batch tree (M3b).
    async fn run_tool_batch(&mut self, uses: &[ToolUse]) -> Option<Outcome> {
        let mutating = uses.iter().any(|tu| tu.name != "read");
        if mutating {
            self.snap(format!("pre batch {}", self.samples)).await;
        }
        let mut results = Vec::with_capacity(uses.len());
        let mut budget_blown: Option<String> = None;
        for chunk in parallel_chunks(uses, &self.shared.registry) {
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
            // A chunk is one serial call or a run of parallel-safe calls that
            // overlap; join_all returns outcomes in source order either way.
            let outcomes = futures_util::future::join_all(
                chunk
                    .iter()
                    .zip(gates)
                    .map(|(tu, gate)| self.execute(tu, gate)),
            )
            .await;
            for (tu, mut outcome) in chunk.iter().zip(outcomes) {
                self.maybe_evict(tu, &mut outcome).await;
                let (content, failed) = self.apply_failure_budget(tu, outcome, &mut budget_blown);
                results.push(ToolResultItem {
                    tool_use_id: tu.id.clone(),
                    content,
                    is_error: failed,
                });
            }
        }
        if mutating {
            self.snap(format!("post batch {}", self.samples)).await;
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
        if !self.propose(entries).await {
            return Some(Outcome::Error {
                message: "session log is sealed".into(),
            });
        }
        if cancelled {
            return Some(Outcome::Cancelled);
        }
        budget_blown.map(|tool| Outcome::ToolFailureBudget { tool })
    }

    /// Track per-tool consecutive failures; attach `<retry attempts_left>`
    /// feedback and flag the budget when it hits zero.
    fn apply_failure_budget(
        &mut self,
        tu: &ToolUse,
        outcome: ToolOutcome,
        budget_blown: &mut Option<String>,
    ) -> (String, bool) {
        let mut content = outcome.content;
        if outcome.is_error {
            let n = self
                .consecutive_failures
                .entry(tu.name.clone())
                .or_insert(0);
            *n += 1;
            let left = self.shared.config.tool_failure_budget.saturating_sub(*n);
            content.push_str(&format!("\n<retry attempts_left={left}>"));
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
    async fn gate(&self, tu: &ToolUse) -> Gate {
        let Some(tool) = self.shared.registry.get(&tu.name) else {
            return Gate::Resolved(unknown_tool(&self.tool_defs, &tu.name));
        };
        // PreToolUse: a wrap-style intercept may block or rewrite the call.
        let mut input = tu.input.clone();
        if let Some(hooks) = &self.shared.hooks {
            match hooks.pre_tool(&tu.name, &input).await {
                crate::hooks::PreToolDecision::Continue => {}
                crate::hooks::PreToolDecision::Deny { message } => {
                    self.emit(EngineEvent::ToolDenied {
                        name: tu.name.clone(),
                    })
                    .await;
                    return Gate::Resolved(ToolOutcome::err(format!(
                        "A hook blocked this tool call: {message}"
                    )));
                }
                crate::hooks::PreToolDecision::Rewrite { input: rewritten } => input = rewritten,
            }
        }
        let (summary, why) = match tool.permission(&input) {
            Permission::None => (None, None),
            Permission::Ask { summary } => (Some(summary), None),
            Permission::AskProtected { summary, why } => (Some(summary), Some(why)),
        };
        let display = summary.clone().unwrap_or_else(|| tu.name.clone());
        if let Some(summary) = summary {
            match self.approve_input(tu, &input, summary, why).await {
                AskReply::Allow => {}
                AskReply::AllowEdited { input: edited } => input = edited, // §2b
                AskReply::Respond { content } => {
                    // §2b: the human answered as the tool — skip execution.
                    self.emit(EngineEvent::ToolDone {
                        name: tu.name.clone(),
                        ok: true,
                    })
                    .await;
                    return Gate::Resolved(ToolOutcome::ok(content));
                }
                AskReply::Deny { message } => {
                    self.emit(EngineEvent::ToolDenied {
                        name: tu.name.clone(),
                    })
                    .await;
                    return Gate::Resolved(match message {
                        Some(m) => ToolOutcome::err(format!("The user declined this tool call: {m}")),
                        None => ToolOutcome::err(
                            "The user declined this tool call. Ask what they'd like to do instead, or proceed another way.",
                        ),
                    });
                }
            }
        }
        Gate::Ready {
            input,
            summary: display,
        }
    }

    /// Execute an approved call: ToolStart → run → PostToolUse hook →
    /// ToolDone. `&self` only, so approved parallel-safe calls in one chunk
    /// can run concurrently; a call the gate already resolved passes through.
    async fn execute(&self, tu: &ToolUse, gate: Gate) -> ToolOutcome {
        let Gate::Ready { input, summary } = gate else {
            let Gate::Resolved(outcome) = gate else {
                unreachable!()
            };
            return outcome;
        };
        self.emit(EngineEvent::ToolStart {
            name: tu.name.clone(),
            summary,
        })
        .await;
        let Some(tool) = self.shared.registry.get(&tu.name) else {
            return unknown_tool(&self.tool_defs, &tu.name); // gate checked; defensive
        };
        let mut outcome = tool.run(input, self.cancel.clone()).await;
        // PostToolUse: a node-style proposal may replace a successful result.
        if !outcome.is_error {
            if let Some(hooks) = &self.shared.hooks {
                if let Some(replacement) = hooks.post_tool(&tu.name, &outcome.content).await {
                    outcome.content = replacement;
                }
            }
        }
        self.emit(EngineEvent::ToolDone {
            name: tu.name.clone(),
            ok: !outcome.is_error,
        })
        .await;
        outcome
    }

    /// Allow-rules (deny-first, sandbox-gated, protected carve-out) or the
    /// ask, evaluated against `input` (which a PreToolUse hook may have
    /// rewritten — a rewritten call re-enters the gate, never bypasses it).
    async fn approve_input(
        &self,
        tu: &ToolUse,
        input: &Value,
        summary: String,
        why: Option<String>,
    ) -> AskReply {
        let protected = why.is_some();
        let read_only = self
            .shared
            .registry
            .get(&tu.name)
            .is_some_and(|t| t.read_only());
        match self.shared.rules.evaluate(
            self.shared.effective_mode(),
            &tu.name,
            input,
            self.shared.sandbox_enforced,
            protected,
            read_only,
        ) {
            Verdict::Auto { rule } => {
                self.emit(EngineEvent::ToolAutoAllowed {
                    name: tu.name.clone(),
                    rule,
                })
                .await;
                AskReply::Allow
            }
            Verdict::Deny { rule } => AskReply::Deny {
                message: Some(format!(
                    "a deny rule refused this call ({rule}); do not retry it"
                )),
            },
            Verdict::Ask => self.ask(summary, why).await,
        }
    }

    /// Complete protocol pairing for a batch that will not execute.
    async fn abort_batch(&mut self, uses: &[ToolUse], message: &str) {
        let results = uses.iter().map(|tu| pair(tu, message, true)).collect();
        self.propose(vec![EntryPayload::Item {
            item: Item::ToolResults { results },
        }])
        .await;
    }

    /// Pre-flight: the compaction threshold check (M2), then the request with
    /// the MOIM turn-context attached. Past the speculation threshold the
    /// digest summarize starts here, overlapping the samples still to come.
    fn build_request(&mut self, snapshot: &Arc<Vec<Item>>) -> Result<SamplingRequest, SampleEnd> {
        let window = self.shared.config.context_window.max(1);
        let estimate = self.estimate_tokens(snapshot);
        if estimate > (window as f64 * COMPACT_TRIGGER) as u64 {
            return Err(SampleEnd::ContextFull);
        }
        if self.speculation.is_none()
            && !self.shared.config.compaction_reset
            && estimate > (window as f64 * SPECULATE_TRIGGER) as u64
        {
            self.speculation = spawn_speculation(&self.shared, snapshot);
        }
        let used_pct = self
            .shared
            .config
            .show_context_pct
            .then(|| (estimate.saturating_mul(100) / window).min(100) as u8);
        let turn_context = hotl_context::turn_context(
            self.shared.clock.now_ms(),
            &self.shared.cwd,
            used_pct,
            self.samples,
        );
        Ok(SamplingRequest {
            model: self.models[self.model_idx].clone(),
            max_tokens: self.shared.config.max_tokens,
            system: Arc::clone(&self.shared.system),
            items: Arc::clone(snapshot),
            tools: Arc::clone(&self.tool_defs),
            thinking: self.shared.config.thinking,
            cache_static: self.shared.config.cache_static,
            turn_context: Some(turn_context),
        })
    }

    /// Drain the provider stream (cancel-biased), forwarding deltas.
    async fn collect_stream(
        &mut self,
        request: SamplingRequest,
    ) -> Result<(StopReason, TokenUsage, Vec<Value>), SampleEnd> {
        let mut stream = self.shared.provider.stream(request);
        let mut completed = None;
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(SampleEnd::Cancelled),
                next = stream.next() => match next {
                    Some(Ok(event)) => {
                        if let StreamEvent::Completed { stop, usage, blocks } = event {
                            completed = Some((stop, usage, blocks));
                        } else {
                            self.forward(event).await;
                        }
                    }
                    Some(Err(e)) if retry::is_context_overflow(&e) => return Err(SampleEnd::ContextFull),
                    Some(Err(e)) if retry::is_availability(&e) => return Err(SampleEnd::Unavailable(e.to_string())),
                    Some(Err(e)) => return Err(SampleEnd::Fatal(e.to_string())),
                    None => break,
                }
            }
        }
        completed.ok_or_else(|| SampleEnd::Fatal("stream ended without completion".into()))
    }

    /// Anchored context estimate for the next request: the provider-reported
    /// cost of the last sample plus the (overcounting) estimate of everything
    /// appended since. Falls back to a full estimate before the first sample.
    fn estimate_tokens(&self, snapshot: &[Item]) -> u64 {
        use hotl_context::tokens;
        match self.anchor {
            Some((reported, len)) if snapshot.len() >= len => {
                reported + tokens::estimate_items(&snapshot[len..])
            }
            _ => tokens::estimate_text(&self.shared.system) + tokens::estimate_items(snapshot),
        }
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
        snapshot.iter().any(|i| {
            matches!(
                i,
                Item::User { text, synthetic: Some(hotl_types::SyntheticReason::SubdirInstructions) }
                    if text.contains(marker)
            )
        })
    }

    async fn snap(&self, label: String) {
        if let Some(snapshots) = &self.shared.snapshots {
            snapshots.snapshot(label).await;
        }
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

    /// The speculative digest, if one was fired and succeeded. Awaiting the
    /// residual is still a win: the task has had whole samples to run in.
    async fn take_speculation(&mut self) -> Option<crate::SpecDigest> {
        let handle = self.speculation.take()?;
        handle.await.ok().flatten()
    }

    async fn snapshot(&self) -> Option<Arc<Vec<Item>>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(SessionCmd::Snapshot { reply: tx })
            .await
            .ok()?;
        rx.await.ok()
    }

    async fn propose(&self, entries: Vec<EntryPayload>) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCmd::Propose { entries, reply: tx })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Ask the human via the event channel; a dropped reply means deny.
    /// The ask is committed durably *before* it surfaces (§2b `pending_ask`)
    /// and its resolution *after* (`ask_resolved`) — so a process that dies
    /// mid-ask leaves a dangling record that resume re-surfaces. The log
    /// records are best-effort: a sealed log never blocks the ask itself.
    async fn ask(&self, summary: String, why: Option<String>) -> AskReply {
        let id = hotl_types::new_ulid();
        let _ = self
            .propose(vec![EntryPayload::PendingAsk {
                id: id.clone(),
                summary: summary.clone(),
                protected_why: why.clone(),
            }])
            .await;
        // Notification (tier-1 gap #7, the `hotl watch`/desktop seam): the
        // agent is blocked on a human, right before the ask actually
        // surfaces. Fire-and-forget — never awaited on this hot path.
        if let Some(hooks) = &self.shared.hooks {
            crate::hooks::notify(
                hooks,
                crate::hooks::NotificationKind::Blocked,
                summary.clone(),
            );
        }
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
            AskReply::Allow | AskReply::AllowEdited { .. } | AskReply::Respond { .. }
        );
        let _ = self
            .propose(vec![EntryPayload::AskResolved { id, allowed }])
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

/// Whether the todo reminder in a snapshot (appended as its last item, only
/// when the list is non-empty — see `actor::snapshot_with_todos`) lists any
/// `pending`/`in_progress` work. Reads the render text directly rather than
/// round-tripping through `Vec<Todo>`: it's exactly what the model just
/// sampled against, so the gate's read matches what produced the reply.
fn unfinished_todos(snapshot: &[Item]) -> bool {
    matches!(
        snapshot.last(),
        Some(Item::User {
            text,
            synthetic: Some(SyntheticReason::Todos),
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
struct CallSig {
    hash: u64,
    display: String,
}

impl CallSig {
    fn new(tu: &ToolUse) -> Self {
        let display = format!("{}({})", tu.name, tu.input);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        display.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            display,
        }
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
        let same = |a: &CallSig, b: &CallSig| a.hash == b.hash;
        if tail
            .chunks(period)
            .all(|c| c.iter().zip(block).all(|(a, b)| same(a, b)))
        {
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
    fn doom_detector_finds_periods() {
        let a = || sig("read", json!({"path":"x"}));
        let b = || sig("bash", json!({"command":"ls"}));
        assert!(detect_doom_loop(&[a(), a(), a()]).is_some());
        let sigs = vec![a(), b(), a(), b(), a(), b()];
        assert!(detect_doom_loop(&sigs).is_some());
        assert!(detect_doom_loop(&[a(), a(), b()]).is_none());
        assert!(detect_doom_loop(&[a(), a()]).is_none());
        // The ask still shows the human-readable signatures.
        let pattern = detect_doom_loop(&[a(), a(), a()]).unwrap();
        assert_eq!(pattern, "read({\"path\":\"x\"})");
    }

    fn todos_item(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: Some(SyntheticReason::Todos),
        }
    }

    #[test]
    fn unfinished_todos_reads_only_the_tagged_last_item() {
        assert!(!unfinished_todos(&[]));
        // Untagged last item (an ordinary reply) never counts, even if it
        // happens to contain the marker text.
        assert!(!unfinished_todos(&[Item::User {
            text: "[ ] not a todo reminder".into(),
            synthetic: None,
        }]));
        assert!(unfinished_todos(&[todos_item("<todos>\n[ ] a\n</todos>")]));
        assert!(unfinished_todos(&[todos_item("<todos>\n[~] a\n</todos>")]));
        assert!(!unfinished_todos(&[todos_item("<todos>\n[x] a\n</todos>")]));
    }
}
