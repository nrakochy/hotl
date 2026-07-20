//! L3 — the turn engine, M0 slice (system-design §L3; 0001 §M0).
//!
//! A plain sample → tools loop: no actor, no steer/queue inbox, no compaction
//! (those are M1/M2 refactors of this running loop, per the plan). What M0
//! does carry: `max_turns`, cancellation via an out-of-band token, gated tool
//! execution with all results returned in one item, and every durable step
//! appended to the session log.

use futures_util::StreamExt;
use hotl_platform::Clock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, StreamEvent, ToolDef};
use hotl_store::SessionLog;
use hotl_tools::{Permission, PermissionGate, Registry, ToolOutcome};
use hotl_types::{
    assistant_text, assistant_tool_uses, EntryPayload, Item, StopReason, TokenUsage, ToolResultItem,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    pub thinking: bool,
    pub cache_static: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            model: "claude-opus-4-8".into(),
            max_tokens: 32_000,
            max_turns: 25,
            thinking: true,
            cache_static: true,
        }
    }
}

/// What the surface renders. Deltas stream; everything else is punctuation.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart { name: String, summary: String },
    ToolDone { name: String, ok: bool },
    ToolDenied { name: String },
    Retrying { attempt: u32, reason: String },
    TurnDone { usage: TokenUsage },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Model finished; final assistant text attached.
    Done { text: String },
    Cancelled,
    /// `max_turns` hit before the model finished.
    TurnLimit,
    /// The model refused (safety classifiers); surface, don't retry.
    Refused,
    /// A repeating tool-call pattern was detected and the human declined to
    /// continue (Forge's detector, surfaced as an ask — corpus 10/11).
    DoomLoop { pattern: String },
}

/// Detect a repeating suffix pattern over tool-call signatures: any period
/// p ≤ 3 whose block repeats 3× at the tail. Returns a human-readable
/// description of the repeating block.
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

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("log write failed: {0}")]
    Log(#[from] std::io::Error),
    #[error("provider stream ended without a Completed event")]
    NoCompletion,
}

pub struct Engine<'a> {
    pub provider: &'a dyn Provider,
    pub registry: &'a Registry,
    pub gate: &'a dyn PermissionGate,
    pub clock: &'a dyn Clock,
    pub config: EngineConfig,
}

impl<'a> Engine<'a> {
    /// Run one user prompt to completion, mutating `items` (the live
    /// conversation) and appending every durable step to `log`.
    pub async fn run_prompt(
        &self,
        items: &mut Vec<Item>,
        log: &mut SessionLog,
        system: &str,
        prompt: String,
        cancel: CancellationToken,
        on_event: &mut dyn FnMut(EngineEvent),
    ) -> Result<Outcome, EngineError> {
        let user = Item::User { text: prompt, synthetic: None };
        log.append(EntryPayload::Item { item: user.clone() }, self.clock.now_ms())?;
        items.push(user);

        let tool_defs: Vec<ToolDef> = self.registry.defs();
        let mut total_usage = TokenUsage::default();
        // Tool-call signatures across the whole prompt, for doom-loop detection.
        let mut call_sigs: Vec<String> = Vec::new();

        for _turn in 0..self.config.max_turns {
            let req = SamplingRequest {
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                system: system.to_string(),
                items: items.clone(),
                tools: tool_defs.clone(),
                thinking: self.config.thinking,
                cache_static: self.config.cache_static,
            };

            let mut stream = self.provider.stream(req);
            let mut completed: Option<(StopReason, TokenUsage, Vec<serde_json::Value>)> = None;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        log.append(
                            EntryPayload::Cancelled { reason: "user interrupt".into() },
                            self.clock.now_ms(),
                        )?;
                        return Ok(Outcome::Cancelled);
                    }
                    next = stream.next() => match next {
                        Some(Ok(StreamEvent::TextDelta { text, .. })) => on_event(EngineEvent::TextDelta(text)),
                        Some(Ok(StreamEvent::ThinkingDelta { text, .. })) => on_event(EngineEvent::ThinkingDelta(text)),
                        Some(Ok(StreamEvent::Retrying { attempt, reason })) => on_event(EngineEvent::Retrying { attempt, reason }),
                        Some(Ok(StreamEvent::Completed { stop, usage, blocks })) => {
                            completed = Some((stop, usage, blocks));
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(e.into()),
                        None => break,
                    }
                }
            }
            let (stop, usage, blocks) = completed.ok_or(EngineError::NoCompletion)?;
            total_usage += usage;

            let assistant = Item::Assistant { blocks: blocks.clone() };
            log.append(EntryPayload::Item { item: assistant.clone() }, self.clock.now_ms())?;
            log.append(EntryPayload::Usage { usage }, self.clock.now_ms())?;
            items.push(assistant);

            match stop {
                StopReason::ToolUse => {
                    let uses = assistant_tool_uses(&blocks);
                    for tu in &uses {
                        call_sigs.push(format!("{}({})", tu.name, tu.input));
                    }
                    if let Some(pattern) = detect_doom_loop(&call_sigs) {
                        let allowed = self
                            .gate
                            .ask(
                                &format!("the agent keeps repeating: {pattern} — let it continue?"),
                                None,
                            )
                            .await;
                        if !allowed {
                            log.append(
                                EntryPayload::Cancelled { reason: format!("doom loop: {pattern}") },
                                self.clock.now_ms(),
                            )?;
                            // Complete protocol pairing so the log stays replayable.
                            let results = uses
                                .iter()
                                .map(|tu| ToolResultItem {
                                    tool_use_id: tu.id.clone(),
                                    content: "Stopped: the user declined to continue a repeating tool-call loop.".into(),
                                    is_error: true,
                                })
                                .collect();
                            let item = Item::ToolResults { results };
                            log.append(EntryPayload::Item { item: item.clone() }, self.clock.now_ms())?;
                            items.push(item);
                            on_event(EngineEvent::TurnDone { usage: total_usage });
                            return Ok(Outcome::DoomLoop { pattern });
                        }
                        // Human said continue: reset the window so the same
                        // pattern doesn't immediately re-trigger.
                        call_sigs.clear();
                    }
                    let mut results = Vec::with_capacity(uses.len());
                    for tu in &uses {
                        let outcome = self.execute_gated(tu, &cancel, on_event).await;
                        results.push(ToolResultItem {
                            tool_use_id: tu.id.clone(),
                            content: outcome.content,
                            is_error: outcome.is_error,
                        });
                        if cancel.is_cancelled() {
                            // Complete the protocol pairing for already-issued
                            // calls, then stop cleanly.
                            for rest in uses.iter().skip(results.len()) {
                                results.push(ToolResultItem {
                                    tool_use_id: rest.id.clone(),
                                    content: "Cancelled by the user before execution.".into(),
                                    is_error: true,
                                });
                            }
                            let item = Item::ToolResults { results };
                            log.append(EntryPayload::Item { item: item.clone() }, self.clock.now_ms())?;
                            items.push(item);
                            log.append(
                                EntryPayload::Cancelled { reason: "user interrupt during tools".into() },
                                self.clock.now_ms(),
                            )?;
                            return Ok(Outcome::Cancelled);
                        }
                    }
                    let item = Item::ToolResults { results };
                    log.append(EntryPayload::Item { item: item.clone() }, self.clock.now_ms())?;
                    items.push(item);
                    // continue to next sample
                }
                StopReason::Refusal => {
                    on_event(EngineEvent::TurnDone { usage: total_usage });
                    return Ok(Outcome::Refused);
                }
                _ => {
                    on_event(EngineEvent::TurnDone { usage: total_usage });
                    return Ok(Outcome::Done { text: assistant_text(&blocks) });
                }
            }
        }

        log.append(
            EntryPayload::Cancelled { reason: format!("max_turns ({}) reached", self.config.max_turns) },
            self.clock.now_ms(),
        )?;
        on_event(EngineEvent::TurnDone { usage: total_usage });
        Ok(Outcome::TurnLimit)
    }

    async fn execute_gated(
        &self,
        tu: &hotl_types::ToolUse,
        cancel: &CancellationToken,
        on_event: &mut dyn FnMut(EngineEvent),
    ) -> ToolOutcome {
        let Some(tool) = self.registry.get(&tu.name) else {
            return ToolOutcome::err(format!(
                "Unknown tool `{}`. Available tools: {}.",
                tu.name,
                self.registry.defs().iter().map(|d| d.name.clone()).collect::<Vec<_>>().join(", ")
            ));
        };
        let (needs_ask, summary, why) = match tool.permission(&tu.input) {
            Permission::None => (false, String::new(), None),
            Permission::Ask { summary } => (true, summary, None),
            Permission::AskProtected { summary, why } => (true, summary, Some(why)),
        };
        if needs_ask {
            let allowed = self.gate.ask(&summary, why.as_deref()).await;
            if !allowed {
                on_event(EngineEvent::ToolDenied { name: tu.name.clone() });
                return ToolOutcome::err(
                    "The user declined this tool call. Ask what they'd like to do instead, or proceed another way.",
                );
            }
        }
        on_event(EngineEvent::ToolStart {
            name: tu.name.clone(),
            summary: if summary.is_empty() { tu.name.clone() } else { summary.clone() },
        });
        let outcome = tool.run(tu.input.clone(), cancel.clone()).await;
        on_event(EngineEvent::ToolDone { name: tu.name.clone(), ok: !outcome.is_error });
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_platform::SystemClock;
    use hotl_provider::ScriptedProvider;
    use hotl_store::Masker;
    use hotl_tools::StaticGate;
    use serde_json::json;

    fn setup_log(dir: &std::path::Path) -> SessionLog {
        SessionLog::create(dir, "test", None, Masker::empty(), 0).unwrap()
    }

    #[tokio::test]
    async fn scripted_tool_roundtrip_lands_in_log() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, "hello from disk\n").unwrap();

        // Script: model asks to read the file, then answers.
        let provider = ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "read", json!({"path": file_path.to_str().unwrap()})),
            ScriptedProvider::text_reply("The file says hello."),
        ]);
        let registry = Registry::builtin();
        let gate = StaticGate(true);
        let clock = SystemClock;
        let engine = Engine {
            provider: &provider,
            registry: &registry,
            gate: &gate,
            clock: &clock,
            config: EngineConfig { max_turns: 5, ..Default::default() },
        };

        let mut items = Vec::new();
        let mut log = setup_log(dir.path());
        let mut events = Vec::new();
        let outcome = engine
            .run_prompt(
                &mut items,
                &mut log,
                "sys",
                "what does hello.txt say?".into(),
                CancellationToken::new(),
                &mut |e| events.push(format!("{e:?}")),
            )
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Done { text: "The file says hello.".into() });
        // Conversation shape: user, assistant(tool_use), tool_results, assistant(text)
        assert_eq!(items.len(), 4);
        assert!(matches!(items[2], Item::ToolResults { .. }));
        if let Item::ToolResults { results } = &items[2] {
            assert!(results[0].content.contains("hello from disk"));
        }
        // Golden-lite: the persisted log replays the same shape.
        let content = std::fs::read_to_string(log.path()).unwrap();
        let kinds: Vec<String> = content
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["payload"]["kind"].as_str().unwrap_or("?").to_string()
            })
            .collect();
        assert_eq!(kinds, ["header", "item", "item", "usage", "item", "item", "usage"]);
        assert!(events.iter().any(|e| e.contains("ToolStart")));
    }

    #[tokio::test]
    async fn denied_gate_feeds_error_result_back() {
        let dir = tempfile::tempdir().unwrap();
        let provider = ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "rm -rf /"})),
            ScriptedProvider::text_reply("Understood, I won't run it."),
        ]);
        let registry = Registry::builtin();
        let gate = StaticGate(false); // headless default-deny
        let clock = SystemClock;
        let engine = Engine {
            provider: &provider,
            registry: &registry,
            gate: &gate,
            clock: &clock,
            config: EngineConfig { max_turns: 3, ..Default::default() },
        };
        let mut items = Vec::new();
        let mut log = setup_log(dir.path());
        let outcome = engine
            .run_prompt(&mut items, &mut log, "sys", "clean up".into(), CancellationToken::new(), &mut |_| {})
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Done { .. }));
        if let Item::ToolResults { results } = &items[2] {
            assert!(results[0].is_error);
            assert!(results[0].content.contains("declined"));
        } else {
            panic!("expected tool results");
        }
    }

    #[test]
    fn doom_detector_finds_periods() {
        let a = "read({\"path\":\"x\"})".to_string();
        let b = "bash({\"command\":\"ls\"})".to_string();
        // period 1: three identical
        assert!(detect_doom_loop(&[a.clone(), a.clone(), a.clone()]).is_some());
        // period 2: ababab
        let sigs = vec![a.clone(), b.clone(), a.clone(), b.clone(), a.clone(), b.clone()];
        assert!(detect_doom_loop(&sigs).is_some());
        // no loop: distinct tail
        let sigs = vec![a.clone(), a.clone(), b.clone()];
        assert!(detect_doom_loop(&sigs).is_none());
        // two repeats only: not yet a loop
        assert!(detect_doom_loop(&[a.clone(), a.clone()]).is_none());
    }

    #[tokio::test]
    async fn doom_loop_surfaced_as_ask_and_stops_on_deny() {
        // Model repeats the identical read call; gate (headless) denies the
        // continue-ask at the third repetition.
        let scripts: Vec<_> = (0..5)
            .map(|_| ScriptedProvider::tool_call("t", "read", json!({"path": "/same"})))
            .collect();
        let provider = ScriptedProvider::new(scripts);
        let registry = Registry::builtin();
        let gate = StaticGate(false);
        let clock = SystemClock;
        let engine = Engine {
            provider: &provider,
            registry: &registry,
            gate: &gate,
            clock: &clock,
            config: EngineConfig { max_turns: 10, ..Default::default() },
        };
        let dir = tempfile::tempdir().unwrap();
        let mut items = Vec::new();
        let mut log = setup_log(dir.path());
        let outcome = engine
            .run_prompt(&mut items, &mut log, "sys", "go".into(), CancellationToken::new(), &mut |_| {})
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::DoomLoop { .. }), "got {outcome:?}");
        // The log records the stop and stays protocol-paired.
        let content = std::fs::read_to_string(log.path()).unwrap();
        assert!(content.contains("doom loop"));
    }

    #[tokio::test]
    async fn max_turns_caps_runaway_loop() {
        // Model calls a tool forever.
        let scripts: Vec<_> = (0..10)
            .map(|i| ScriptedProvider::tool_call(&format!("t{i}"), "read", json!({"path": "/nonexistent"})))
            .collect();
        let provider = ScriptedProvider::new(scripts);
        let registry = Registry::builtin();
        let gate = StaticGate(true);
        let clock = SystemClock;
        let engine = Engine {
            provider: &provider,
            registry: &registry,
            gate: &gate,
            clock: &clock,
            config: EngineConfig { max_turns: 3, ..Default::default() },
        };
        let dir = tempfile::tempdir().unwrap();
        let mut items = Vec::new();
        let mut log = setup_log(dir.path());
        let outcome = engine
            .run_prompt(&mut items, &mut log, "sys", "loop forever".into(), CancellationToken::new(), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::TurnLimit);
    }
}
