//! Golden-transcript testkit.
//!
//! Scripted completions drive the *real* actor/turn/persistence stack; tests
//! assert on the normalized persisted transcript — the log is the canon, so
//! the log is what gets golden-checked. Determinism comes from the commit
//! protocol: the log fixes exactly one order for every interleaving the
//! harness can produce.

use std::sync::Arc;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Entry, Item};

pub use hotl_provider::ScriptedProvider as Scripted;

pub struct Harness {
    pub handle: SessionHandle,
    pub provider: Arc<ScriptedProvider>,
    /// Debug strings of every event seen, in order.
    pub seen: Vec<String>,
    log_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
    /// Extra temp dirs whose lifetime must match the harness (scenario
    /// fixtures the scripted tools read/write) — kept, not leaked.
    extra_dirs: Vec<tempfile::TempDir>,
    /// Answer for every Ask event (T1: defaults to Allow).
    pub ask_reply: AskReply,
    /// One-shot steer to send when the next ToolStart is observed.
    pub steer_on_tool_start: Option<String>,
    /// Labels of every shadow snapshot the engine requested, in order.
    pub snapshots: Arc<std::sync::Mutex<Vec<String>>>,
}

/// Records snapshot labels instead of running git.
struct RecordingSnapshotter(Arc<std::sync::Mutex<Vec<String>>>);

impl hotl_engine::Snapshotter for RecordingSnapshotter {
    fn snapshot(&self, label: String) -> futures_util::future::BoxFuture<'static, ()> {
        self.0.lock().expect("snapshot log").push(label);
        Box::pin(async {})
    }
}

impl Harness {
    /// The harness working directory (the engine's `cwd` for subdir hints;
    /// also a scratch space for files the scripted tools touch).
    pub fn dir(&self) -> &std::path::Path {
        self._dir.path()
    }

    /// Tie a fixture temp dir's lifetime to the harness (it is removed when
    /// the harness drops, instead of being forgotten and leaked on disk).
    pub fn keep_dir(&mut self, dir: tempfile::TempDir) {
        self.extra_dirs.push(dir);
    }
}

impl Harness {
    pub fn new(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
    ) -> Self {
        Self::with_items(scripts, config, Vec::new())
    }

    /// Construct a harness with a pre-seeded projection (resume scenarios).
    pub fn with_items(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
    ) -> Self {
        Self::build(scripts, config, initial_items, None)
    }

    /// Construct a harness with extension hooks (M5 scenarios).
    pub fn with_hooks(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        hooks: Arc<dyn hotl_engine::hooks::Hooks>,
    ) -> Self {
        Self::build_with(
            scripts,
            config,
            Vec::new(),
            Some(hooks),
            Registry::builtin(),
        )
    }

    /// Construct a harness with a custom tool registry (concurrency probes,
    /// scripted tools).
    pub fn with_registry(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        registry: Registry,
    ) -> Self {
        Self::build_with(scripts, config, Vec::new(), None, registry)
    }

    fn build(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
    ) -> Self {
        Self::build_with(scripts, config, initial_items, hooks, Registry::builtin())
    }

    /// Construct a harness with custom permission rules (mode/deny/admin
    /// scenarios).
    pub fn with_rules(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        rules: Rules,
    ) -> Self {
        Self::build_full(scripts, config, Vec::new(), None, Registry::builtin(), rules)
    }

    fn build_with(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
        registry: Registry,
    ) -> Self {
        Self::build_full(scripts, config, initial_items, hooks, registry, Rules::default())
    }

    fn build_full(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
        registry: Registry,
        rules: Rules,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
            .expect("session log");
        let log_path = log.path().to_path_buf();
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deps = SessionDeps {
            provider: provider.clone(),
            registry: Arc::new(registry),
            rules: Arc::new(rules),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "test-system".into(),
            cwd: dir.path().to_path_buf(),
            snapshots: Some(Arc::new(RecordingSnapshotter(snapshots.clone()))),
            hooks,
            initial_items,
            config,
        };
        let handle = spawn_session(deps);
        Self {
            handle,
            provider,
            seen: Vec::new(),
            log_path,
            _dir: dir,
            extra_dirs: Vec::new(),
            ask_reply: AskReply::Allow,
            steer_on_tool_start: None,
            snapshots,
        }
    }

    /// Send a prompt and drain events until the turn finishes.
    pub async fn prompt_and_wait(&mut self, text: &str) -> Outcome {
        self.handle.prompt(text.to_string()).await;
        self.wait_for_outcome().await
    }

    pub async fn wait_for_outcome(&mut self) -> Outcome {
        loop {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.handle.events.recv(),
            )
            .await
            .expect("event timeout")
            .expect("event channel closed");
            self.seen.push(format!("{event:?}"));
            match event {
                EngineEvent::Ask { reply, .. } => {
                    let _ = reply.send(self.ask_reply.clone());
                }
                EngineEvent::ToolStart { .. } => {
                    if let Some(steer) = self.steer_on_tool_start.take() {
                        self.handle.steer(steer).await;
                    }
                }
                EngineEvent::TurnDone { outcome, .. } => return outcome,
                _ => {}
            }
        }
    }

    /// The persisted entry kinds, in order — the coarse golden signature.
    pub fn kinds(&self) -> Vec<String> {
        self.entries()
            .iter()
            .map(|e| {
                serde_json::to_value(&e.payload)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str().map(String::from)))
                    .unwrap_or_else(|| "?".into())
            })
            .collect()
    }

    /// The full normalized transcript: ids/parents/timestamps zeroed so runs
    /// are byte-comparable.
    pub fn transcript(&self) -> String {
        self.entries()
            .iter()
            .map(|e| {
                let mut v = serde_json::to_value(e).expect("entry to value");
                v["id"] = "ID".into();
                v["parent_id"] = if e.parent_id.is_some() {
                    "PARENT".into()
                } else {
                    serde_json::Value::Null
                };
                v["ts_ms"] = 0.into();
                if let Some(h) = v.pointer_mut("/payload/header") {
                    h["session_id"] = "SESSION".into();
                    h["created_at_ms"] = 0.into();
                }
                v.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn entries(&self) -> Vec<Entry> {
        std::fs::read_to_string(&self.log_path)
            .expect("read log")
            .lines()
            .map(|l| serde_json::from_str(l).expect("parse entry"))
            .collect()
    }

    /// Conversation items as persisted, in order.
    pub fn items(&self) -> Vec<Item> {
        self.entries()
            .into_iter()
            .filter_map(|e| match e.payload {
                hotl_types::EntryPayload::Item { item } => Some(item),
                _ => None,
            })
            .collect()
    }

    /// (tool name, input) for every persisted assistant tool_use, in log order
    /// — the *trajectory* a scenario produced.
    pub fn tool_calls(&self) -> Vec<(String, serde_json::Value)> {
        self.items()
            .iter()
            .filter_map(|i| match i {
                Item::Assistant { blocks } => Some(hotl_types::assistant_tool_uses(blocks)),
                _ => None,
            })
            .flatten()
            .map(|tu| (tu.name, tu.input))
            .collect()
    }

    /// Assert on the tool-call sequence a scenario produced (not just entry
    /// kinds). Panics — with both sequences — when the mode's relation fails.
    pub fn assert_trajectory(&self, expected: &[&str], mode: TrajectoryMatch) {
        let actual: Vec<String> = self.tool_calls().into_iter().map(|(n, _)| n).collect();
        let ok = match mode {
            TrajectoryMatch::Exact => actual
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied()),
            TrajectoryMatch::Unordered => {
                let mut a: Vec<&str> = actual.iter().map(String::as_str).collect();
                let mut e = expected.to_vec();
                a.sort_unstable();
                e.sort_unstable();
                a == e
            }
            TrajectoryMatch::Subset => is_subsequence(expected, &actual),
        };
        assert!(
            ok,
            "trajectory {mode:?} failed:\n expected: {expected:?}\n actual:   {actual:?}"
        );
    }
}

/// How `assert_trajectory` relates the expected names to the actual sequence.
#[derive(Debug, Clone, Copy)]
pub enum TrajectoryMatch {
    /// Exactly this sequence, in order.
    Exact,
    /// The same multiset of names, any order.
    Unordered,
    /// These names appear in order (an in-order subsequence).
    Subset,
}

/// A one-sample script whose assistant turn calls several tools in one batch.
pub fn tool_batch(
    calls: &[(&str, &str, serde_json::Value)],
) -> Vec<Result<StreamEvent, ProviderError>> {
    let blocks: Vec<serde_json::Value> = calls
        .iter()
        .map(|(id, name, input)| {
            serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
        })
        .collect();
    vec![
        Ok(StreamEvent::Started),
        Ok(StreamEvent::Completed {
            stop: hotl_types::StopReason::ToolUse,
            usage: hotl_types::TokenUsage {
                input_tokens: 10,
                output_tokens: 8,
                ..Default::default()
            },
            blocks,
        }),
    ]
}

/// Is `needles` an in-order subsequence of `haystack`?
fn is_subsequence(needles: &[&str], haystack: &[String]) -> bool {
    let mut it = haystack.iter();
    needles.iter().all(|n| it.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::{StopReason, SyntheticReason, TokenUsage};
    use serde_json::json;

    fn cfg() -> EngineConfig {
        EngineConfig {
            max_turns: 6,
            ..Default::default()
        }
    }

    /// A concurrency probe: each call bumps a shared running-counter and
    /// records the high-water mark, then waits (bounded) for the mark to hit
    /// 2. Overlapping calls drive the mark to 2; serial execution never does.
    /// The mark is monotonic, so a fast partner can't be missed between
    /// polls, and no cross-call state survives a timeout (unlike a barrier).
    struct OverlapProbe {
        safe: bool,
        running: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl hotl_tools::Tool for OverlapProbe {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn description(&self) -> &str {
            "waits for a partner call"
        }
        fn schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission(&self, _: &serde_json::Value) -> hotl_tools::Permission {
            hotl_tools::Permission::None
        }
        fn parallel_safe(&self) -> bool {
            self.safe
        }
        fn run<'a>(
            &'a self,
            _input: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
            use std::sync::atomic::Ordering;
            Box::pin(async move {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                let mut saw_partner = self.peak.load(Ordering::SeqCst) >= 2;
                for _ in 0..50 {
                    if saw_partner {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    saw_partner = self.peak.load(Ordering::SeqCst) >= 2;
                }
                self.running.fetch_sub(1, Ordering::SeqCst);
                if saw_partner {
                    hotl_tools::ToolOutcome::ok("overlapped")
                } else {
                    hotl_tools::ToolOutcome::err("did not overlap")
                }
            })
        }
    }

    fn probe_registry(safe: bool) -> Registry {
        let mut reg = Registry::builtin();
        reg.register(Box::new(OverlapProbe {
            safe,
            running: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peak: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));
        reg
    }

    #[tokio::test]
    async fn parallel_safe_calls_in_one_batch_overlap() {
        let mut h = Harness::with_registry(
            vec![
                tool_batch(&[("t1", "probe", json!({})), ("t2", "probe", json!({}))]),
                ScriptedProvider::text_reply("both ran"),
            ],
            cfg(),
            probe_registry(true),
        );
        let outcome = h.prompt_and_wait("probe twice").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "both ran".into()
            }
        );
        let items = h.items();
        let Item::ToolResults { results } = &items[2] else {
            panic!("expected results, got {items:#?}")
        };
        // Both calls passed the barrier — they ran concurrently…
        assert!(
            results
                .iter()
                .all(|r| !r.is_error && r.content == "overlapped"),
            "{results:?}"
        );
        // …and the paired results keep assistant source order.
        let ids: Vec<_> = results.iter().map(|r| r.tool_use_id.as_str()).collect();
        assert_eq!(ids, ["t1", "t2"]);
    }

    #[tokio::test]
    async fn unsafe_calls_in_one_batch_stay_serial() {
        let mut h = Harness::with_registry(
            vec![
                tool_batch(&[("t1", "probe", json!({})), ("t2", "probe", json!({}))]),
                ScriptedProvider::text_reply("done"),
            ],
            cfg(),
            probe_registry(false),
        );
        let outcome = h.prompt_and_wait("probe twice").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "done".into()
            }
        );
        let items = h.items();
        let Item::ToolResults { results } = &items[2] else {
            panic!("expected results, got {items:#?}")
        };
        // Neither call may see the other running: both time out at the barrier.
        assert!(
            results
                .iter()
                .all(|r| r.is_error && r.content.contains("did not overlap")),
            "{results:?}"
        );
    }

    fn auto_rules() -> hotl_tools::rules::Rules {
        hotl_tools::rules::Rules::default().with_mode(hotl_tools::rules::PermissionMode::Auto)
    }

    #[tokio::test]
    async fn auto_mode_runs_mutating_calls_without_asking() {
        // write (not bash): the harness runs unsandboxed, and auto mode
        // deliberately excludes unsandboxed bash — covered by rules tests.
        let mut h = Harness::with_rules(
            vec![
                ScriptedProvider::tool_call("t1", "write", json!({"path": "notes.txt", "content": "x"})),
                ScriptedProvider::text_reply("ran silently"),
            ],
            cfg(),
            auto_rules(),
        );
        let outcome = h.prompt_and_wait("write the note").await;
        assert_eq!(outcome, Outcome::Done { text: "ran silently".into() });
        // No ask fired; the transcript shows who silenced it.
        assert!(!h.seen.iter().any(|e| e.starts_with("Ask(")), "events: {:?}", h.seen);
        assert!(
            h.seen.iter().any(|e| e.contains("permissions.mode=auto")),
            "events: {:?}",
            h.seen
        );
        // The undo safety net still brackets the batch.
        assert_eq!(*h.snapshots.lock().unwrap(), vec!["pre batch 1", "post batch 1"]);
    }

    #[tokio::test]
    async fn auto_mode_protected_write_still_asks() {
        let mut h = Harness::with_rules(
            vec![
                ScriptedProvider::tool_call("t1", "write", json!({"path": "Makefile", "content": "x"})),
                ScriptedProvider::text_reply("done"),
            ],
            cfg(),
            auto_rules(),
        );
        h.prompt_and_wait("write the makefile").await;
        assert!(
            h.seen.iter().any(|e| e.starts_with("Ask(")),
            "protected must ask: {:?}",
            h.seen
        );
    }

    #[tokio::test]
    async fn auto_mode_doom_loop_stops_without_asking() {
        let scripts: Vec<_> = (0..5)
            .map(|_| ScriptedProvider::tool_call("t", "read", json!({"path": "/same"})))
            .collect();
        let mut h = Harness::with_rules(
            scripts,
            EngineConfig { max_turns: 10, ..Default::default() },
            auto_rules(),
        );
        let outcome = h.prompt_and_wait("go").await;
        assert!(matches!(outcome, Outcome::DoomLoop { .. }), "got {outcome:?}");
        assert!(
            !h.seen.iter().any(|e| e.starts_with("Ask(")),
            "no human to ask in auto: {:?}",
            h.seen
        );
    }

    #[tokio::test]
    async fn golden_tool_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hello from disk\n").unwrap();
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": file.to_str().unwrap()})),
                ScriptedProvider::text_reply("The file says hello."),
            ],
            cfg(),
        );
        let outcome = h.prompt_and_wait("what does hello.txt say?").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "The file says hello.".into()
            }
        );
        assert_eq!(
            h.kinds(),
            ["header", "item", "item", "usage", "item", "item", "usage"]
        );
        // Golden: the normalized transcript is stable across runs.
        let t1 = h.transcript();
        assert!(t1.contains("hello from disk"));
        assert!(t1.contains("tool_use"));
    }

    #[tokio::test]
    async fn denied_ask_feeds_error_back() {
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "bash", json!({"command": "rm -rf /"})),
                ScriptedProvider::text_reply("Understood."),
            ],
            cfg(),
        );
        h.ask_reply = AskReply::Deny { message: None };
        let outcome = h.prompt_and_wait("clean up").await;
        assert!(matches!(outcome, Outcome::Done { .. }));
        let items = h.items();
        let Item::ToolResults { results } = &items[2] else {
            panic!("expected results")
        };
        assert!(results[0].is_error && results[0].content.contains("declined"));
        assert!(h.seen.iter().any(|e| e.starts_with("Ask(")));
    }

    #[tokio::test]
    async fn steer_mid_turn_reaches_next_sample() {
        // Turn 1 runs a (slow-ish) bash; the harness steers when ToolStart
        // appears; sample 2 must include the steer in its request items.
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call(
                    "t1",
                    "bash",
                    json!({"command": "sleep 0.2; echo done"}),
                ),
                ScriptedProvider::text_reply("Done, and noted your steer."),
            ],
            cfg(),
        );
        h.steer_on_tool_start = Some("also check the README".into());
        let outcome = h.prompt_and_wait("run the thing").await;
        assert!(matches!(outcome, Outcome::Done { .. }));

        // The steer is durably recorded with provenance…
        let steer_items: Vec<_> = h
            .items()
            .into_iter()
            .filter(|i| {
                matches!(
                    i,
                    Item::User {
                        synthetic: Some(SyntheticReason::Steer),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(steer_items.len(), 1);

        // …and the SECOND sample's request actually contained it (rebase row
        // of the conflict table: woven into the next sample, not the current).
        let requests = h.provider.requests();
        assert_eq!(requests.len(), 2);
        let saw_in_first = requests[0].items.iter().any(is_steer);
        let saw_in_second = requests[1].items.iter().any(is_steer);
        assert!(
            !saw_in_first,
            "steer must not appear in the sample that was already running"
        );
        assert!(saw_in_second, "steer must be woven into the next sample");
    }

    fn is_steer(i: &Item) -> bool {
        matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::Steer),
                ..
            }
        )
    }

    #[tokio::test]
    async fn queued_prompt_promotes_after_turn() {
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "bash", json!({"command": "sleep 0.2"})),
                ScriptedProvider::text_reply("first done"),
                ScriptedProvider::text_reply("second done"),
            ],
            cfg(),
        );
        h.handle.prompt("first".into()).await;
        // Queue a second prompt immediately (turn is running).
        h.handle.prompt("second".into()).await;
        let first = h.wait_for_outcome().await;
        assert_eq!(
            first,
            Outcome::Done {
                text: "first done".into()
            }
        );
        let second = h.wait_for_outcome().await;
        assert_eq!(
            second,
            Outcome::Done {
                text: "second done".into()
            }
        );
        assert!(h.seen.iter().any(|e| e == "PromptQueued"));
    }

    #[tokio::test]
    async fn doom_loop_stops_on_deny() {
        let scripts: Vec<_> = (0..5)
            .map(|_| ScriptedProvider::tool_call("t", "read", json!({"path": "/same"})))
            .collect();
        let mut h = Harness::new(
            scripts,
            EngineConfig {
                max_turns: 10,
                ..Default::default()
            },
        );
        h.ask_reply = AskReply::Deny { message: None };
        let outcome = h.prompt_and_wait("go").await;
        assert!(
            matches!(outcome, Outcome::DoomLoop { .. }),
            "got {outcome:?}"
        );
        assert!(h.entries().iter().any(|e| matches!(
            &e.payload,
            hotl_types::EntryPayload::Cancelled { reason } if reason.contains("doom")
        )));
    }

    #[tokio::test]
    async fn fallback_model_on_availability_error() {
        let mut h = Harness::new(
            vec![
                vec![Err(ProviderError::Transport("connection reset".into()))],
                ScriptedProvider::text_reply("served by fallback"),
            ],
            EngineConfig {
                fallback_models: vec!["backup-model".into()],
                ..cfg()
            },
        );
        let outcome = h.prompt_and_wait("hi").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "served by fallback".into()
            }
        );
        assert!(h
            .seen
            .iter()
            .any(|e| e.contains("FallbackModel(backup-model)")));
        let reqs = h.provider.requests();
        assert_eq!(reqs[1].model, "backup-model");
    }

    #[tokio::test]
    async fn auth_error_does_not_fall_back() {
        let mut h = Harness::new(
            vec![vec![Err(ProviderError::Auth("bad key".into()))]],
            EngineConfig {
                fallback_models: vec!["backup".into()],
                ..cfg()
            },
        );
        let outcome = h.prompt_and_wait("hi").await;
        assert!(matches!(outcome, Outcome::Error { .. }));
        assert!(!h.seen.iter().any(|e| e.contains("FallbackModel")));
    }

    #[tokio::test]
    async fn tool_failure_budget_stops_turn() {
        // Distinct paths so the doom detector (identical sigs) stays quiet.
        let scripts: Vec<_> = (0..6)
            .map(|i| {
                ScriptedProvider::tool_call(
                    &format!("t{i}"),
                    "read",
                    json!({"path": format!("/nope{i}")}),
                )
            })
            .collect();
        let mut h = Harness::new(
            scripts,
            EngineConfig {
                max_turns: 10,
                tool_failure_budget: 3,
                ..Default::default()
            },
        );
        let outcome = h.prompt_and_wait("read them all").await;
        assert_eq!(
            outcome,
            Outcome::ToolFailureBudget {
                tool: "read".into()
            }
        );
        // Feedback element present in the failing results.
        let items = h.items();
        let with_feedback = items.iter().any(|i| matches!(
            i, Item::ToolResults { results } if results.iter().any(|r| r.content.contains("<retry attempts_left="))
        ));
        assert!(with_feedback);
    }

    #[tokio::test]
    async fn max_turns_caps_runaway() {
        let scripts: Vec<_> = (0..10)
            .map(|i| {
                // Alternate two calls so neither doom (period ≤3 needs 3 repeats
                // of a block) nor the failure budget trips first… actually use
                // successful bash echoes: no failures, distinct args.
                ScriptedProvider::tool_call(
                    &format!("t{i}"),
                    "bash",
                    json!({"command": format!("echo {i}")}),
                )
            })
            .collect();
        let mut h = Harness::new(
            scripts,
            EngineConfig {
                max_turns: 3,
                ..Default::default()
            },
        );
        let outcome = h.prompt_and_wait("loop").await;
        assert_eq!(outcome, Outcome::TurnLimit);
    }

    #[tokio::test]
    async fn interrupt_cancels_running_turn() {
        let mut h = Harness::new(
            vec![ScriptedProvider::tool_call(
                "t1",
                "bash",
                json!({"command": "sleep 30"}),
            )],
            cfg(),
        );
        h.handle.prompt("run forever".into()).await;
        // Wait until the tool starts, then interrupt out-of-band.
        loop {
            let ev =
                tokio::time::timeout(std::time::Duration::from_secs(5), h.handle.events.recv())
                    .await
                    .expect("timeout")
                    .expect("closed");
            h.seen.push(format!("{ev:?}"));
            match ev {
                EngineEvent::Ask { reply, .. } => {
                    let _ = reply.send(AskReply::Allow);
                }
                EngineEvent::ToolStart { .. } => break,
                _ => {}
            }
        }
        h.handle.interrupt();
        let outcome = h.wait_for_outcome().await;
        assert_eq!(outcome, Outcome::Cancelled);
    }

    #[tokio::test]
    async fn transcript_normalization_is_deterministic() {
        let make = || async {
            let mut h = Harness::new(vec![ScriptedProvider::text_reply("stable")], cfg());
            h.prompt_and_wait("say something stable").await;
            h.transcript()
        };
        let (a, b) = (make().await, make().await);
        assert_eq!(
            a, b,
            "normalized transcripts must be byte-identical across runs"
        );
    }

    /// A tool-call sample whose Completed reports a chosen input_tokens —
    /// compaction tests anchor on provider-reported usage (A12b), so the
    /// script must be able to "report" a nearly-full window.
    fn tool_call_reporting(
        id: &str,
        name: &str,
        input: serde_json::Value,
        input_tokens: u64,
    ) -> Vec<Result<StreamEvent, ProviderError>> {
        let mut script = ScriptedProvider::tool_call(id, name, input);
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.input_tokens = input_tokens;
        }
        script
    }

    #[tokio::test]
    async fn compaction_folds_history_and_continues() {
        // Window 1000 → trigger at 800, tail budget 300. A big first result,
        // then a sample that "reports" 750 tokens: the next request estimate
        // crosses 800, so the turn compacts. The plan folds the big early
        // history and keeps the small recent exchange verbatim.
        let cfg = EngineConfig {
            context_window: 1000,
            max_turns: 10,
            ..Default::default()
        };
        let scripts = vec![
            ScriptedProvider::tool_call(
                "t1",
                "bash",
                json!({"command": format!("echo {}", "A".repeat(1100))}),
            ),
            tool_call_reporting(
                "t2",
                "bash",
                json!({"command": format!("echo {}", "B".repeat(200))}),
                750,
            ),
            ScriptedProvider::text_reply("GOAL: digest of earlier work"),
            ScriptedProvider::text_reply("finished after compaction"),
        ];
        let mut h = Harness::new(scripts, cfg);
        let outcome = h.prompt_and_wait("summarize both outputs").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "finished after compaction".into()
            }
        );
        assert!(
            h.seen.iter().any(|e| e == "Compacted(false)"),
            "events: {:?}",
            h.seen
        );

        // The log records the compaction; the projection was re-pointed.
        assert!(h.kinds().iter().any(|k| k == "compaction"));
        let requests = h.provider.requests();
        assert_eq!(requests.len(), 4);
        // Request 3 is the summarize call (its own tiny conversation)…
        assert!(requests[2].system.contains("compress"));
        // …and the continuation request opens with the digest, tail verbatim.
        let continuation = &requests[3];
        assert!(matches!(
            &continuation.items[0],
            Item::User { synthetic: Some(SyntheticReason::CompactionSummary), text }
                if text.contains("GOAL: digest of earlier work")
        ));
        let flat = format!("{:?}", continuation.items);
        assert!(
            !flat.contains(&"A".repeat(64)),
            "folded history must not ride along"
        );
        assert!(flat.contains(&"B".repeat(64)), "the tail stays verbatim");
    }

    #[tokio::test]
    async fn compaction_floor_survives_summarize_failure() {
        let scripts = vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo start"})),
            tool_call_reporting("t2", "bash", json!({"command": "echo more"}), 900),
            // Both summarize attempts fail: the floor placeholder applies.
            vec![Err(ProviderError::Transport("summarizer down".into()))],
            vec![Err(ProviderError::Transport(
                "summarizer still down".into(),
            ))],
            ScriptedProvider::text_reply("continued on the floor"),
        ];
        let mut h = Harness::new(
            scripts,
            EngineConfig {
                context_window: 1000,
                max_turns: 10,
                ..Default::default()
            },
        );
        // ~1500 estimated tokens of tool results pushes past the 800 trigger.
        let outcome = h.prompt_and_wait(&"x".repeat(1200)).await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "continued on the floor".into()
            }
        );
        assert!(
            h.seen.iter().any(|e| e == "Compacted(true)"),
            "events: {:?}",
            h.seen
        );
        let degraded = h.entries().iter().any(|e| {
            matches!(
                &e.payload,
                hotl_types::EntryPayload::Compaction { degraded: true, .. }
            )
        });
        assert!(degraded, "the compaction entry records the floor");
    }

    #[tokio::test]
    async fn moim_rides_the_request_but_never_the_log() {
        let mut h = Harness::new(vec![ScriptedProvider::text_reply("hi")], cfg());
        h.prompt_and_wait("hello").await;
        let requests = h.provider.requests();
        let tc = requests[0]
            .turn_context
            .as_deref()
            .expect("turn context attached");
        assert!(
            tc.contains("sample=\"1\"") && tc.contains("context_used="),
            "was: {tc}"
        );
        assert!(
            !h.transcript().contains("<turn-context"),
            "MOIM must never persist"
        );
    }

    #[tokio::test]
    async fn subdir_agents_md_injected_on_first_touch() {
        let mut h = Harness::new(vec![], cfg());
        let sub = h.dir().join("web");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "web subproject rules").unwrap();
        std::fs::write(sub.join("page.txt"), "content").unwrap();
        let path = sub.join("page.txt");
        h.provider.push_script(ScriptedProvider::tool_call(
            "t1",
            "read",
            json!({"path": path.to_str().unwrap()}),
        ));
        h.provider
            .push_script(ScriptedProvider::text_reply("read it"));
        let outcome = h.prompt_and_wait("read the page").await;
        assert!(matches!(outcome, Outcome::Done { .. }));
        drop(h.provider.requests());
        let hint_items: Vec<_> = h
            .items()
            .into_iter()
            .filter(|i| {
                matches!(
                    i,
                    Item::User {
                        synthetic: Some(SyntheticReason::SubdirInstructions),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(hint_items.len(), 1, "items: {:#?}", h.items());
        let Item::User { text, .. } = &hint_items[0] else {
            unreachable!()
        };
        assert!(text.contains("web subproject rules") && text.contains("trust=\"untrusted\""));
        // The second sample's request saw the hint.
        let requests = h.provider.requests();
        let flat = format!("{:?}", requests[1].items);
        assert!(flat.contains("web subproject rules"));
    }

    #[tokio::test]
    async fn mutating_batches_are_bracketed_by_snapshots() {
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
                ScriptedProvider::text_reply("done"),
            ],
            cfg(),
        );
        h.prompt_and_wait("run it").await;
        let labels = h.snapshots.lock().unwrap().clone();
        assert_eq!(labels, ["pre batch 1", "post batch 1"]);

        // Read-only batches don't snapshot.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": file.to_str().unwrap()})),
                ScriptedProvider::text_reply("read"),
            ],
            cfg(),
        );
        h.prompt_and_wait("read it").await;
        assert!(h.snapshots.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resume_continues_an_interrupted_turn() {
        // A projection ending in a user turn the model never answered (the
        // process died mid-turn). continue_turn re-samples and completes it
        // without a fresh prompt (#8).
        let seeded = vec![Item::User {
            text: "half-finished request".into(),
            synthetic: None,
        }];
        let mut h = Harness::with_items(
            vec![ScriptedProvider::text_reply(
                "finished the interrupted turn",
            )],
            cfg(),
            seeded,
        );
        assert!(hotl_engine::needs_continuation(&h.items()) || h.items().is_empty());
        h.handle.continue_turn().await;
        let outcome = h.wait_for_outcome().await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "finished the interrupted turn".into()
            }
        );
        // No new user item was appended — the request the model saw is the
        // seeded one, not a duplicate.
        let user_turns = h.provider.requests()[0]
            .items
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    Item::User {
                        synthetic: None,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(user_turns, 1, "continue must not append a second user item");
    }

    #[tokio::test]
    async fn continue_is_a_noop_on_a_complete_projection() {
        // Last item is an assistant reply → nothing to continue.
        let done = vec![
            Item::User {
                text: "q".into(),
                synthetic: None,
            },
            Item::Assistant {
                blocks: vec![json!({"type":"text","text":"a"})],
            },
        ];
        assert!(!hotl_engine::needs_continuation(&done));
    }

    #[tokio::test]
    async fn reset_mode_compaction_drops_the_verbatim_tail() {
        // Same overflow setup as the in-place test, but compaction_reset=true:
        // the continuation request carries the digest and NO verbatim tail.
        let cfg = EngineConfig {
            context_window: 1000,
            max_turns: 10,
            compaction_reset: true,
            ..Default::default()
        };
        let scripts = vec![
            ScriptedProvider::tool_call(
                "t1",
                "bash",
                json!({"command": format!("echo {}", "A".repeat(1100))}),
            ),
            tool_call_reporting(
                "t2",
                "bash",
                json!({"command": format!("echo {}", "B".repeat(200))}),
                750,
            ),
            ScriptedProvider::text_reply("GOAL: digest"),
            ScriptedProvider::text_reply("done after reset compaction"),
        ];
        let mut h = Harness::new(scripts, cfg);
        let outcome = h.prompt_and_wait("do the thing").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "done after reset compaction".into()
            }
        );
        assert!(h.seen.iter().any(|e| e.starts_with("Compacted")));
        let continuation = h.provider.last_request().unwrap();
        // The digest is present…
        assert!(continuation.items.iter().any(|i| matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::CompactionSummary),
                ..
            }
        )));
        // …and no ToolResults / Assistant verbatim tail rode along (fresh slate).
        assert!(
            !continuation
                .items
                .iter()
                .any(|i| matches!(i, Item::ToolResults { .. } | Item::Assistant { .. })),
            "reset-mode continuation must not carry the verbatim tail: {:?}",
            continuation.items
        );
    }

    #[tokio::test]
    async fn moim_context_pct_can_be_hidden() {
        let mut h = Harness::new(
            vec![ScriptedProvider::text_reply("hi")],
            EngineConfig {
                show_context_pct: false,
                ..cfg()
            },
        );
        h.prompt_and_wait("hello").await;
        let reqs = h.provider.requests();
        let tc = reqs[0].turn_context.as_deref().unwrap();
        assert!(!tc.contains("context_used"), "pct must be omitted: {tc}");
        assert!(tc.contains("sample="), "the rest of MOIM still rides");
    }

    #[tokio::test]
    async fn pre_tool_hook_blocks_a_call() {
        use hotl_engine::hooks::{InProcessHooks, PreToolDecision};
        let hooks = Arc::new(InProcessHooks::new().on_pre_tool(|name, input| {
            if name == "bash" && input.get("command").and_then(|c| c.as_str()) == Some("danger") {
                PreToolDecision::Deny {
                    message: "policy: no danger".into(),
                }
            } else {
                PreToolDecision::Continue
            }
        }));
        let mut h = Harness::with_hooks(
            vec![
                ScriptedProvider::tool_call("t1", "bash", json!({"command": "danger"})),
                ScriptedProvider::text_reply("understood, blocked"),
            ],
            cfg(),
            hooks,
        );
        let outcome = h.prompt_and_wait("do the dangerous thing").await;
        assert!(matches!(outcome, Outcome::Done { .. }));
        // The blocked call became an error tool result carrying the hook message.
        let blocked = h.items().into_iter().any(|i| matches!(
            i, Item::ToolResults { results } if results.iter().any(|r| r.is_error && r.content.contains("policy: no danger"))
        ));
        assert!(
            blocked,
            "the hook's denial reached the model as a tool result"
        );
    }

    #[tokio::test]
    async fn post_tool_hook_annotates_a_result() {
        use hotl_engine::hooks::InProcessHooks;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "secret content").unwrap();
        let hooks = Arc::new(InProcessHooks::new().on_post_tool(|name, _result| {
            (name == "read").then(|| "[redacted by hook]".to_string())
        }));
        let mut h = Harness::with_hooks(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": file.to_str().unwrap()})),
                ScriptedProvider::text_reply("read the redacted file"),
            ],
            cfg(),
            hooks,
        );
        h.prompt_and_wait("read it").await;
        let redacted = h.items().into_iter().any(|i| matches!(
            i, Item::ToolResults { results } if results.iter().any(|r| r.content.contains("[redacted by hook]"))
        ));
        assert!(
            redacted,
            "the post-tool hook replaced the result the model saw"
        );
        // The real content never reached the transcript.
        assert!(!h.transcript().contains("secret content"));
    }

    #[tokio::test]
    async fn deny_with_message_reaches_the_model() {
        // A denial that carries a reason surfaces as the tool-result feedback
        // (a steer fused with a "no").
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "write", json!({"path": "a.md", "content": "x"})),
                ScriptedProvider::text_reply("understood, using notes.md"),
            ],
            cfg(),
        );
        h.ask_reply = AskReply::Deny {
            message: Some("wrong file — use notes.md".into()),
        };
        let outcome = h.prompt_and_wait("write it").await;
        assert!(matches!(outcome, Outcome::Done { .. }));
        let items = h.items();
        let Item::ToolResults { results } = &items[2] else {
            panic!("expected results")
        };
        assert!(results[0].is_error);
        assert!(
            results[0]
                .content
                .contains("declined this tool call: wrong file — use notes.md"),
            "the denial reason must reach the model: {}",
            results[0].content
        );
    }

    async fn harness_read_then_write() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.txt");
        std::fs::write(&f, "content").unwrap();
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": f.to_str().unwrap()})),
                ScriptedProvider::tool_call(
                    "t2",
                    "write",
                    json!({"path": f.to_str().unwrap(), "content": "new"}),
                ),
                ScriptedProvider::text_reply("did both"),
            ],
            EngineConfig {
                max_turns: 6,
                ..Default::default()
            },
        );
        h.keep_dir(dir);
        h.prompt_and_wait("read then write").await;
        h
    }

    #[tokio::test]
    async fn trajectory_matches() {
        let h = harness_read_then_write().await;
        h.assert_trajectory(&["read", "write"], TrajectoryMatch::Exact);
        h.assert_trajectory(&["write", "read"], TrajectoryMatch::Unordered);
        h.assert_trajectory(&["write"], TrajectoryMatch::Subset);
        assert_eq!(h.tool_calls()[0].0, "read");
        assert_eq!(h.tool_calls()[1].1["content"], "new");
    }

    #[tokio::test]
    #[should_panic(expected = "trajectory")]
    async fn trajectory_exact_rejects_wrong_order() {
        let h = harness_read_then_write().await;
        h.assert_trajectory(&["write", "read"], TrajectoryMatch::Exact);
    }

    /// Read a 60KB file with the given eviction threshold; return the first
    /// tool result's content.
    async fn read_big_with_threshold(threshold: u64) -> String {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "B".repeat(60_000)).unwrap();
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": big.to_str().unwrap()})),
                ScriptedProvider::text_reply("read it"),
            ],
            EngineConfig {
                evict_threshold_tokens: threshold,
                max_turns: 6,
                ..Default::default()
            },
        );
        h.prompt_and_wait("read the big file").await;
        drop(dir);
        let items = h.items();
        let Item::ToolResults { results } = &items[2] else {
            panic!("expected results")
        };
        // The blob (when evicted) lives beside the harness log.
        if threshold != 0 {
            let has_blobs = std::fs::read_dir(h.dir())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.path().to_string_lossy().contains(".blobs"));
            assert!(
                has_blobs,
                "a .blobs dir should exist beside the log after eviction"
            );
        }
        results[0].content.clone()
    }

    #[tokio::test]
    async fn oversized_tool_result_is_evicted_to_a_blob() {
        let content = read_big_with_threshold(5_000).await;
        assert!(content.contains("<evicted"), "result should be evicted");
        assert!(content.contains("Read it with the read tool"));
        assert!(
            content.len() < 5_000,
            "in-context result is a preview, not the full 60KB"
        );
    }

    #[tokio::test]
    async fn eviction_disabled_at_threshold_zero() {
        let content = read_big_with_threshold(0).await;
        assert!(
            !content.contains("<evicted"),
            "threshold 0 disables eviction"
        );
        assert!(
            content.len() > 50_000,
            "the full result rides in-context when disabled"
        );
    }

    #[allow(dead_code)]
    fn silence_unused(_: StopReason, _: TokenUsage) {}
}
