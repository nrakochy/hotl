//! Golden-transcript testkit.
//!
//! Scripted completions drive the *real* actor/turn/persistence stack; tests
//! assert on the normalized persisted transcript — the log is the canon, so
//! the log is what gets golden-checked. Determinism comes from the commit
//! protocol: the log fixes exactly one order for every interleaving the
//! harness can produce.

use std::sync::Arc;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, LedgerSummary, Outcome, SessionDeps,
    SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Entry, Item};

pub use hotl_provider::ScriptedProvider as Scripted;

/// The system prompt every harness runs under unless a scenario varies it.
const DEFAULT_TEST_SYSTEM: &str = "test-system";

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
    /// One-shot steer to send when the next stream delta is observed — i.e.
    /// *inside* a sample window. Deterministic only with
    /// [`Harness::with_paused_completion`], which holds the stream open long
    /// enough for the steer to reach the actor before the sample closes.
    pub steer_on_text_delta: Option<String>,
    /// Labels of every shadow snapshot the engine requested, in order.
    pub snapshots: Arc<std::sync::Mutex<Vec<String>>>,
    /// Every `EngineEvent::LedgerReport` seen, in order (§S1 instrument).
    pub ledger_reports: Vec<LedgerSummary>,
    /// The session log's live `sync_data()` counter — the group-commit
    /// counter assertions (commit-protocol.md §Test obligations) read it
    /// after the log itself has moved into the session.
    fsyncs: Arc<std::sync::atomic::AtomicU64>,
}

/// Wraps the scripted provider so every stream stalls for `pause` right
/// before its terminal `Completed` event. That stall is the *sample window*:
/// with the `Completed` pair committing as a pipelined causal group
/// (commit-protocol.md §Causal groups), it is the only place a scenario can
/// deterministically land a steer between "the request went out" and "the
/// assistant item is durable" — the interleaving 72a6f1b is about.
struct PausedCompletion {
    inner: Arc<ScriptedProvider>,
    pause: std::time::Duration,
}

impl Provider for PausedCompletion {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>> {
        use futures_util::StreamExt;
        let pause = self.pause;
        Box::pin(self.inner.stream(req).then(move |event| async move {
            if matches!(event, Ok(StreamEvent::Completed { .. })) {
                tokio::time::sleep(pause).await;
            }
            event
        }))
    }
}

/// A wrapper the harness installs between the engine and the scripted
/// provider — see [`Harness::build_wrapped`].
type ProviderWrap = Box<dyn FnOnce(Arc<ScriptedProvider>) -> Arc<dyn Provider>>;

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

    /// The session log this harness writes — for scenarios whose scripted
    /// tools have to read the canon while the turn is still running.
    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }

    /// The directory that log lives in — what `hotl_store::replay_chain`
    /// takes. A fork scenario seeds itself by replaying its parent from disk
    /// (the production path), not by copying the parent's in-memory items.
    pub fn sessions_dir(&self) -> &std::path::Path {
        self.log_path
            .parent()
            .expect("the harness log always lives in its temp dir")
    }

    /// This session's store id — the other half of `replay_chain`'s arguments.
    pub fn session_id(&self) -> &str {
        self.log_path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("the harness log is named <ulid>.jsonl")
    }
}

impl Harness {
    pub fn new(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
    ) -> Self {
        Self::with_items(scripts, config, Vec::new())
    }

    /// Construct a harness with a pre-seeded projection **and its own system
    /// prompt**. The system string is byte-stable for a session's lifetime by
    /// construction, so the only way to vary it is at construction — which is
    /// exactly what the fork cache proof's negative control has to do.
    pub fn with_items_and_system(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        system: &str,
    ) -> Self {
        Self::build_wrapped_with_system(
            scripts,
            config,
            initial_items,
            None,
            Registry::builtin(),
            Rules::default(),
            false,
            None,
            system,
            None,
        )
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

    /// Construct a harness with a pre-seeded projection *and* a custom
    /// registry. The cache-prefix scenarios need both at once: a history long
    /// enough that the breakpoint planner has already placed a rolling anchor,
    /// and the scripted tools whose batches grow it.
    pub fn with_items_and_registry(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        registry: Registry,
    ) -> Self {
        Self::build_with(scripts, config, initial_items, None, registry)
    }

    /// Construct a harness with a custom tool registry and the writer's
    /// `sync_data()` no-op'd (`hotl_store::SessionLog::set_sync_noop`) — the
    /// §S1 loop-overhead CI gate's scenario, isolating measured overhead
    /// from real disk sync latency.
    pub fn with_registry_sync_noop(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        registry: Registry,
    ) -> Self {
        Self::build_full(
            scripts,
            config,
            Vec::new(),
            None,
            registry,
            Rules::default(),
            true,
        )
    }

    fn build(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
    ) -> Self {
        Self::build_with(scripts, config, initial_items, hooks, Registry::builtin())
    }

    /// Construct a harness whose every sample stalls before completing — see
    /// [`PausedCompletion`]. Pair with [`Harness::steer_on_text_delta`].
    pub fn with_paused_completion(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        pause: std::time::Duration,
    ) -> Self {
        Self::build_wrapped(
            scripts,
            config,
            Vec::new(),
            None,
            Registry::builtin(),
            Rules::default(),
            false,
            Some(Box::new(move |inner| {
                Arc::new(PausedCompletion { inner, pause })
            })),
        )
    }

    /// Construct a harness with custom permission rules (mode/deny/admin
    /// scenarios).
    pub fn with_rules(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        rules: Rules,
    ) -> Self {
        Self::build_full(
            scripts,
            config,
            Vec::new(),
            None,
            Registry::builtin(),
            rules,
            false,
        )
    }

    fn build_with(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
        registry: Registry,
    ) -> Self {
        Self::build_full(
            scripts,
            config,
            initial_items,
            hooks,
            registry,
            Rules::default(),
            false,
        )
    }

    fn build_full(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
        registry: Registry,
        rules: Rules,
        sync_noop: bool,
    ) -> Self {
        Self::build_wrapped(
            scripts,
            config,
            initial_items,
            hooks,
            registry,
            rules,
            sync_noop,
            None,
        )
    }

    /// `wrap`, when present, sits between the engine and the scripted
    /// provider: the harness keeps the `ScriptedProvider` for `requests()`
    /// and `push_script`, while the engine talks to the wrapper.
    /// Construct a harness with a custom registry AND a custom `Snapshotter`
    /// (0033 Task 4: detached post-batch snapshot scenarios need one whose
    /// futures the test can hold open).
    pub fn with_registry_and_snapshotter(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        registry: Registry,
        snapshotter: Arc<dyn hotl_engine::Snapshotter>,
    ) -> Self {
        Self::build_wrapped_with_system(
            scripts,
            config,
            Vec::new(),
            None,
            registry,
            Rules::default(),
            false,
            None,
            DEFAULT_TEST_SYSTEM,
            Some(snapshotter),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_wrapped(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
        registry: Registry,
        rules: Rules,
        sync_noop: bool,
        wrap: Option<ProviderWrap>,
    ) -> Self {
        Self::build_wrapped_with_system(
            scripts,
            config,
            initial_items,
            hooks,
            registry,
            rules,
            sync_noop,
            wrap,
            DEFAULT_TEST_SYSTEM,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_wrapped_with_system(
        scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
        config: EngineConfig,
        initial_items: Vec<Item>,
        hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
        registry: Registry,
        rules: Rules,
        sync_noop: bool,
        wrap: Option<ProviderWrap>,
        system: &str,
        snapshotter: Option<Arc<dyn hotl_engine::Snapshotter>>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
            .expect("session log");
        if sync_noop {
            log.set_sync_noop(true);
        }
        let fsyncs = log.fsync_counter();
        let log_path = log.path().to_path_buf();
        // Mirror the binary's `record_fresh_seed`: a session's seed is part of
        // the projection it runs with, so it has to be part of the log too —
        // otherwise a replay (and so any fork of this session) reconstructs the
        // conversation one block short of what actually went on the wire.
        for item in &initial_items {
            log.append(&hotl_types::EntryPayload::Item { item: item.clone() }, 0)
                .expect("seed append");
        }
        let provider = Arc::new(ScriptedProvider::new(scripts));
        let snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine_provider: Arc<dyn Provider> = match wrap {
            Some(wrap) => wrap(Arc::clone(&provider)),
            None => provider.clone(),
        };
        let deps = SessionDeps {
            provider: engine_provider,
            registry: Arc::new(registry),
            rules: Arc::new(rules),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: system.into(),
            cwd: dir.path().to_path_buf(),
            snapshots: Some(
                snapshotter.unwrap_or_else(|| Arc::new(RecordingSnapshotter(snapshots.clone()))),
            ),
            hooks,
            initial_items,
            initial_todos: Vec::new(),
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
            steer_on_text_delta: None,
            snapshots,
            ledger_reports: Vec::new(),
            fsyncs,
        }
    }

    /// `sync_data()` calls the session's writer has completed so far.
    pub fn fsync_count(&self) -> u64 {
        self.fsyncs.load(std::sync::atomic::Ordering::SeqCst)
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
                EngineEvent::TextDelta(_) | EngineEvent::ThinkingDelta(_) => {
                    if let Some(steer) = self.steer_on_text_delta.take() {
                        self.handle.steer(steer).await;
                    }
                }
                EngineEvent::TurnDone { outcome, .. } => return outcome,
                EngineEvent::LedgerReport(summary) => self.ledger_reports.push(summary),
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

/// The cache-assert vocabulary: what "the durable prefix only ever grew"
/// means on the wire, expressed once so every cache claim in the workspace is
/// the same claim.
///
/// Public because the property is not one crate's: a *fork*'s first request
/// has to extend its *parent's* last request byte for byte, which is a
/// cross-session version of exactly what `assert_stable_cache_prefix` says
/// within one. Asserting that from a second hand-rolled comparison would let
/// the two definitions of "unchanged prefix" drift.
pub mod wire {
    use std::sync::Arc;

    use hotl_provider::SamplingRequest;

    /// The API's cache lookback, in wire content blocks. A marker further than
    /// this past the nearest entry the previous request wrote cannot see it,
    /// and the segment before it re-bills at write price.
    pub const CACHE_LOOKBACK: usize = 20;

    /// The API's per-request `cache_control` budget (a fifth is a 400).
    pub const MARKER_BUDGET: usize = 4;

    /// The request as the wire would carry it with **only its durable half**:
    /// ephemeral tail emptied, MOIM dropped. Built by re-serializing the
    /// `SamplingRequest` (the `without_moim` pattern), never by fishing
    /// messages back out of a built body — a tail user message is
    /// byte-indistinguishable from a durable one once it is JSON, which is
    /// precisely why the split had to happen upstream of the serializer.
    pub fn durable_wire_body(req: &SamplingRequest) -> serde_json::Value {
        parsed(&hotl_provider_anthropic::wire_body(&SamplingRequest {
            ephemeral_tail: Arc::new(Vec::new()),
            turn_context: None,
            ..req.clone()
        }))
    }

    /// `wire_body` returns the final bytes (0033 Task 6); the structural
    /// helpers here still want a tree to walk.
    pub fn parsed(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("a wire body is valid JSON")
    }

    /// Every `cache_control` key removed, recursively.
    ///
    /// Markers are *metadata*: they are not part of the prefix the API hashes,
    /// so moving one between requests does not invalidate an entry. "The same
    /// conversation, grown" therefore has to compare equal whether or not an
    /// anchor rolled — which is exactly what makes anchor-rolling safe.
    pub fn without_markers(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .filter(|(k, _)| k.as_str() != "cache_control")
                    .map(|(k, val)| (k.clone(), without_markers(val)))
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(without_markers).collect())
            }
            other => other.clone(),
        }
    }

    /// Every `cache_control` in a built body, counted structurally.
    pub fn count_markers(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(count_markers).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter().map(count_markers).sum(),
            _ => 0,
        }
    }

    /// 1-based wire content-block index of every marker in `messages` — the
    /// coordinate space the lookback counts in. The prefix marker (system, or
    /// the last tool def) sits *before* block 1 and is represented by the
    /// implicit start position 0.
    pub fn marker_positions(body: &serde_json::Value) -> Vec<usize> {
        let mut idx = 0usize;
        let mut out = Vec::new();
        for msg in body["messages"].as_array().expect("messages") {
            for block in msg["content"].as_array().expect("content") {
                idx += 1;
                if block.get("cache_control").is_some() {
                    out.push(idx);
                }
            }
        }
        out
    }

    /// Every marker's `ttl` (`None` = plain, i.e. the API's default 5m), in
    /// the order the API parses the prompt: tools, then system, then messages
    /// in wire order. That order is what "longer TTL before shorter" is a
    /// claim about.
    pub fn marker_ttls_in_prompt_order(body: &serde_json::Value) -> Vec<Option<String>> {
        let mut out = Vec::new();
        let mut take = |v: &serde_json::Value| {
            if let Some(cc) = v.get("cache_control") {
                out.push(cc.get("ttl").and_then(|t| t.as_str()).map(String::from));
            }
        };
        for section in ["tools", "system"] {
            for entry in body[section].as_array().into_iter().flatten() {
                take(entry);
            }
        }
        for msg in body["messages"].as_array().expect("messages") {
            for block in msg["content"].as_array().expect("content") {
                take(block);
            }
        }
        out
    }

    /// Claims 2 and 3, held over one request: the marker budget, and TTL
    /// ordering (the API requires longer-lived breakpoints to precede
    /// shorter-lived ones).
    pub fn assert_marker_budget_and_ttl_order(req: &SamplingRequest, label: &str) {
        let body = parsed(&hotl_provider_anthropic::wire_body(req));
        let markers = count_markers(&body);
        assert!(
            markers <= MARKER_BUDGET,
            "{label}: {markers} cache_control markers exceeds the API budget of \
             {MARKER_BUDGET}:\n{body:#}"
        );
        let ttls = marker_ttls_in_prompt_order(&body);
        if let Some(plain) = ttls.iter().position(Option::is_none) {
            assert!(
                ttls[plain..].iter().all(|t| t.as_deref() != Some("1h")),
                "{label}: a 1h marker follows a plain (5m) one — longer TTLs must \
                 come first: {ttls:?}"
            );
        }
    }

    /// Is `earlier`'s durable body a **structural prefix** of `later`'s: the
    /// same system, the same tools, and every message `earlier` carried still
    /// present, unchanged, at the same index? Markers are ignored (see
    /// [`without_markers`]) — a rolled anchor is growth, not a rewrite.
    pub fn is_structural_prefix(earlier: &serde_json::Value, later: &serde_json::Value) -> bool {
        if without_markers(&earlier["system"]) != without_markers(&later["system"])
            || without_markers(&earlier["tools"]) != without_markers(&later["tools"])
        {
            return false;
        }
        let a = earlier["messages"].as_array().expect("messages");
        let b = later["messages"].as_array().expect("messages");
        a.len() <= b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| without_markers(x) == without_markers(y))
    }

    /// Indices `i` where request `i + 1` broke the prefix relation with
    /// request `i`. Empty for an ordinary session; exactly one across a
    /// compaction fold, which is the only sanctioned discontinuity.
    pub fn cache_prefix_breaks(requests: &[SamplingRequest]) -> Vec<usize> {
        requests
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| {
                !is_structural_prefix(&durable_wire_body(&pair[0]), &durable_wire_body(&pair[1]))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// THE suite. Over a sequence of requests one session actually issued:
    ///
    /// 1. every request's durable body is a structural prefix of the next's;
    /// 2. no request exceeds the `cache_control` budget;
    /// 3. no plain marker precedes a `1h` one in prompt order;
    /// 4. the lookback guarantee, in its two halves — (4a) the **deepest**
    ///    entry request N wrote is reachable from a marker in request N+1
    ///    (else the longest cached prefix N+1 can find is shallower than the
    ///    one N just paid to write, and everything past it re-bills — this is
    ///    the billing claim); and (4b) no marker is more than a lookback past
    ///    the nearest marker at or before it, counting request N's markers,
    ///    request N+1's own shallower markers, and the start.
    ///
    /// Claim 4 is the one a byte-identity test cannot make: it is a statement
    /// about two *different* requests, and it is what the original bug broke.
    ///
    /// Claim 4a is therefore not a universal property of the planner: a
    /// fixture whose single turn appends ≥3 stride crossings at once (a
    /// ~45+ block user-role turn) will fail it even though the code is
    /// behaving exactly as designed — see the budget-exhaustion note in
    /// `cache_plan`'s module doc. That one request legitimately re-bills its
    /// history once and self-heals on the next sample, so such a fixture
    /// needs its own assertion, not this one.
    ///
    /// (4b) deliberately admits N+1's own shallower markers as chain links.
    /// Not because a deeper breakpoint can *read* one — within a single
    /// request the lookup runs against entries that already existed, and
    /// nothing can read an entry this same request is still creating. The
    /// reason is that the content between N+1's own markers is **new**: it has
    /// to be written this request no matter where the markers sit, so there is
    /// no miss to price. What matters is the state afterwards — entries now
    /// exist at both positions, so the chain N+2 walks back through is intact.
    /// Requiring every marker to reach a marker of N alone would fail on
    /// correct code: a request that adds an anchor deeper than anything the
    /// previous request marked is exactly what a growing history is supposed
    /// to do.
    pub fn assert_stable_cache_prefix(requests: &[SamplingRequest]) {
        assert!(
            requests.len() >= 2,
            "a cross-request claim needs at least two requests"
        );
        for (i, req) in requests.iter().enumerate() {
            assert_marker_budget_and_ttl_order(req, &format!("request {i}"));
        }
        let breaks = cache_prefix_breaks(requests);
        assert!(
            breaks.is_empty(),
            "the durable prefix must only ever grow; it changed at {breaks:?}"
        );
        for (i, pair) in requests.windows(2).enumerate() {
            // The start (0) is always an entry: the prefix marker on
            // system/tools seals everything before block 1.
            let written: Vec<usize> = std::iter::once(0)
                .chain(marker_positions(&durable_wire_body(&pair[0])))
                .collect();
            let now = marker_positions(&durable_wire_body(&pair[1]));
            let deepest = *written.last().expect("0 is always written");

            // (4a) the billing claim.
            assert!(
                now.iter()
                    .any(|m| *m >= deepest && m - deepest <= CACHE_LOOKBACK),
                "requests {i}->{}: nothing in {now:?} can see the deepest entry \
                 request {i} wrote (block {deepest}) from within the \
                 {CACHE_LOOKBACK}-block lookback — the history past it re-bills",
                i + 1
            );

            // (4b) no marker outruns the chain.
            let mut reachable = written.clone();
            for p in &now {
                let nearest = reachable
                    .iter()
                    .copied()
                    .filter(|q| q <= p)
                    .max()
                    .expect("0 is always a candidate");
                assert!(
                    p - nearest <= CACHE_LOOKBACK,
                    "requests {i}->{}: the marker at block {p} is {} blocks past the \
                     nearest reachable entry ({reachable:?}) — outside the \
                     {CACHE_LOOKBACK}-block lookback",
                    i + 1,
                    p - nearest
                );
                reachable.push(*p);
                reachable.sort_unstable();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wire::*;
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

    fn bypass_rules() -> hotl_tools::rules::Rules {
        hotl_tools::rules::Rules::default().with_mode(hotl_tools::rules::PermissionMode::Bypass)
    }

    /// True when `hotl-tools` was compiled `security-enforced`, the build where
    /// `Bypass` cannot exist at runtime — `bypass_rules()` above comes back as
    /// `Ask`, so a "runs without asking" premise is **void**, not violated.
    ///
    /// A runtime check rather than `#[cfg(not(feature = ...))]`: this crate has
    /// no `security-enforced` feature of its own, so a `cfg` here is always
    /// false — while `cargo test --workspace --all-features` turns hotl-tools'
    /// on through feature unification.
    fn bypass_unavailable() -> bool {
        hotl_tools::rules::enforced_build()
    }

    #[tokio::test]
    async fn bypass_mode_runs_mutating_calls_without_asking() {
        if bypass_unavailable() {
            return;
        }
        // write (not bash): the harness runs unsandboxed, and auto mode
        // deliberately excludes unsandboxed bash — covered by rules tests.
        //
        // The target must be *inside* the working directory: a write outside
        // it is a protected ask that deliberately outranks `mode=auto` (the
        // sandbox write-confinement floor does not cover it), and this test is
        // about the auto tier silencing an *ordinary* call. A scratch dir
        // created inside the process cwd is both in-tree and self-cleaning, so
        // the write still never leaves a stray file behind.
        let mut h = Harness::with_rules(Vec::new(), cfg(), bypass_rules());
        let scratch = tempfile::TempDir::new_in(".").expect("scratch dir");
        let note = format!(
            "{}/notes.txt",
            scratch.path().file_name().unwrap().to_str().unwrap()
        );
        h.provider.push_script(ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": note, "content": "x"}),
        ));
        h.provider
            .push_script(ScriptedProvider::text_reply("ran silently"));
        let outcome = h.prompt_and_wait("write the note").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "ran silently".into()
            }
        );
        // No ask fired; the transcript shows who silenced it.
        assert!(
            !h.seen.iter().any(|e| e.starts_with("Ask(")),
            "events: {:?}",
            h.seen
        );
        assert!(scratch.path().join("notes.txt").exists(), "write ran");
        assert!(
            h.seen.iter().any(|e| e.contains("permissions.mode=bypass")),
            "events: {:?}",
            h.seen
        );
        // The undo safety net still brackets the batch.
        assert_eq!(
            *h.snapshots.lock().unwrap(),
            vec!["pre batch 1", "post batch 1"]
        );
    }

    #[tokio::test]
    async fn auto_mode_protected_write_still_asks() {
        let mut h = Harness::with_rules(Vec::new(), cfg(), bypass_rules());
        // Protected by file name; inside the harness tempdir so the approved
        // write never touches the test process cwd.
        let makefile = h.dir().join("Makefile");
        h.provider.push_script(ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": makefile.to_str().unwrap(), "content": "x"}),
        ));
        h.provider.push_script(ScriptedProvider::text_reply("done"));
        h.prompt_and_wait("write the makefile").await;
        assert!(
            h.seen.iter().any(|e| e.starts_with("Ask(")),
            "protected must ask: {:?}",
            h.seen
        );
    }

    #[tokio::test]
    async fn auto_mode_doom_loop_stops_without_asking() {
        if bypass_unavailable() {
            return;
        }
        // A *relative* path: `read` is only unprompted inside the working
        // directory, and an absolute one is a protected ask that deliberately
        // outranks `mode=auto`. This test is about auto mode not asking for an
        // ordinary call, so the call has to be an ordinary one.
        let scripts: Vec<_> = (0..5)
            .map(|_| ScriptedProvider::tool_call("t", "read", json!({"path": "same"})))
            .collect();
        let mut h = Harness::with_rules(
            scripts,
            EngineConfig {
                max_turns: 10,
                ..Default::default()
            },
            bypass_rules(),
        );
        let outcome = h.prompt_and_wait("go").await;
        assert!(
            matches!(outcome, Outcome::DoomLoop { .. }),
            "got {outcome:?}"
        );
        assert!(
            !h.seen.iter().any(|e| e.starts_with("Ask(")),
            "no human to ask in auto: {:?}",
            h.seen
        );
    }

    /// 0030 Task 7: an identical call repeated across batches whose results
    /// keep changing is polling, not a doom loop — the turn runs to its
    /// scripted end. The command reads a file it also appends to, so each
    /// repeat's output genuinely differs. Absolute path: bash runs in the
    /// test process's own cwd, and a bare `c` would litter the repo.
    #[tokio::test]
    async fn polling_with_changing_results_is_not_a_doom_loop() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("c");
        let c = c.to_str().unwrap();
        let poll = format!("cat {c}; echo x >> {c}");
        let mut scripts = vec![ScriptedProvider::tool_call(
            "t0",
            "bash",
            json!({"command": format!("echo start > {c}")}),
        )];
        scripts.extend((1..=4).map(|i| {
            ScriptedProvider::tool_call(&format!("t{i}"), "bash", json!({"command": poll}))
        }));
        scripts.push(ScriptedProvider::text_reply("came up"));
        let mut h = Harness::new(
            scripts,
            EngineConfig {
                max_turns: 10,
                ..Default::default()
            },
        );
        let outcome = h.prompt_and_wait("wait for the service").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "came up".into()
            }
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
        // The fixture is an absolute tempdir path, i.e. outside the working
        // directory, so the read is a protected ask before it runs. The
        // pending_ask/ask_resolved pair is part of the golden: it pins that an
        // out-of-tree read is gated and that the gate is journalled.
        assert_eq!(
            h.kinds(),
            [
                "header",
                "item",
                "item",
                "usage",
                "pending_ask",
                "ask_resolved",
                "item",
                "item",
                "usage"
            ]
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

        // …and the sample that actually ran next contained it (rebase row of
        // the conflict table: woven into the next sample, not the current).
        //
        // Three requests, not two: the steer lands exactly at the boundary
        // where the turn optimistically dispatched the next sample, so the
        // refreshed leaf is the steer's and the speculation is refused. That
        // costs one cancelled request and nothing else (commit-protocol.md
        // §Causal groups: "one cancelled request … with no other
        // consequence") — the last request is the sequential rebuild.
        let requests = h.provider.requests();
        assert_eq!(
            requests.len(),
            3,
            "sample 1, the mispredicted optimistic dispatch, and the rebuild"
        );
        assert!(
            !requests[0].items.iter().any(is_steer),
            "steer must not appear in the sample that was already running"
        );
        assert!(
            !requests[1].items.iter().any(is_steer),
            "the speculative request was built before the steer landed"
        );
        assert!(
            requests[2].items.iter().any(is_steer),
            "steer must be woven into the sample that actually ran"
        );
    }

    fn is_steer<I: std::borrow::Borrow<Item>>(i: &I) -> bool {
        matches!(
            i.borrow(),
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
        // Exactly one last-chance hint, on the one-failure-left result — a
        // per-failure countdown was visible give-up pressure (0030 Task 5).
        let items = h.items();
        let hints: usize = items
            .iter()
            .map(|i| match i {
                Item::ToolResults { results } => results
                    .iter()
                    .filter(|r| r.content.contains("<system-hint>"))
                    .count(),
                _ => 0,
            })
            .sum();
        assert_eq!(hints, 1, "one warning, one failure before the budget blows");
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
    async fn negative_max_turns_never_caps() {
        // 30 tool steps — past the old hard-wired 25 — then a real answer.
        // A negative `max_turns` is the opt-in "run until the model is done"
        // posture, so the turn must reach `Done`, never `TurnLimit`.
        let mut scripts: Vec<_> = (0..30)
            .map(|i| {
                ScriptedProvider::tool_call(
                    &format!("t{i}"),
                    "bash",
                    json!({"command": format!("echo {i}")}),
                )
            })
            .collect();
        scripts.push(ScriptedProvider::text_reply("finished"));
        let mut h = Harness::new(
            scripts,
            EngineConfig {
                max_turns: -1,
                ..Default::default()
            },
        );
        let outcome = h.prompt_and_wait("go as long as it takes").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "finished".into()
            }
        );
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

    /// §S1: a turn flushes exactly one `LedgerReport`, sized to the samples
    /// it actually took (a tool-call sample plus the follow-up text-reply
    /// sample), and every sample's `BoundaryEnd` is at or after its
    /// `BoundaryStart` — the instrument must never invert its own window.
    #[tokio::test]
    async fn a_turn_flushes_exactly_one_ledger_report_matching_its_samples() {
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": "x"})),
                ScriptedProvider::text_reply("done"),
            ],
            cfg(),
        );
        let outcome = h.prompt_and_wait("read x then answer").await;
        assert_eq!(
            outcome,
            Outcome::Done {
                text: "done".into()
            }
        );
        assert_eq!(
            h.ledger_reports.len(),
            1,
            "exactly one LedgerReport must be flushed per turn"
        );
        let report = &h.ledger_reports[0];
        assert_eq!(
            report.sample_count,
            h.provider.request_count(),
            "the ledger's sample count must match the samples actually taken"
        );
        for (i, sample) in report.samples.iter().enumerate() {
            let start = sample[hotl_engine::Phase::BoundaryStart as usize];
            let end = sample[hotl_engine::Phase::BoundaryEnd as usize];
            assert!(
                end >= start,
                "sample {i}: BoundaryEnd ({end}) must be >= BoundaryStart ({start})"
            );
        }
    }

    /// §S1 fix: `BatchProposed`/`WatermarkDurable` must land on the sample's
    /// REAL final propose — the tool-results commit when a tool phase runs,
    /// the model's own assistant+usage commit otherwise. Before the fix both
    /// phases were first-stamp-wins like every other phase, so the
    /// tool-results propose in `run_tool_batch` was always a silent no-op:
    /// the (ToolsJoined→BatchProposed) delta was provably always zero, and
    /// the tool-results commit's own latency was never measured at all
    /// (absorbed into WatermarkDurable→BoundaryEnd instead).
    #[tokio::test]
    async fn batch_proposed_and_watermark_durable_track_each_samples_own_final_commit() {
        let mut h = Harness::new(
            vec![
                ScriptedProvider::tool_call("t1", "read", json!({"path": "x"})),
                ScriptedProvider::text_reply("done"),
            ],
            cfg(),
        );
        h.prompt_and_wait("read x then answer").await;
        let report = &h.ledger_reports[0];
        assert_eq!(report.sample_count, 2);

        // Sample 0 ran a tool phase: ToolsJoined must be stamped, and the
        // sample's final commit (BatchProposed/WatermarkDurable) must land
        // AT OR AFTER it — never before, which is what the bug produced
        // (both stuck at the model's own, earlier, assistant+usage commit).
        let tool_sample = &report.samples[0];
        let tools_joined = tool_sample[hotl_engine::Phase::ToolsJoined as usize];
        let batch_proposed = tool_sample[hotl_engine::Phase::BatchProposed as usize];
        let watermark_durable = tool_sample[hotl_engine::Phase::WatermarkDurable as usize];
        assert_ne!(tools_joined, 0, "sample 0 must have run a tool phase");
        assert_ne!(batch_proposed, 0);
        assert_ne!(watermark_durable, 0);
        assert!(
            batch_proposed >= tools_joined,
            "BatchProposed ({batch_proposed}) must be >= ToolsJoined ({tools_joined}) — \
             it must track the tool-results commit, not the earlier model commit"
        );
        assert!(
            watermark_durable >= batch_proposed,
            "WatermarkDurable ({watermark_durable}) must be >= BatchProposed ({batch_proposed})"
        );

        // Sample 1 had no tool phase: ToolsSpawned/ToolsJoined stay absent,
        // but BatchProposed/WatermarkDurable must still be present — the
        // model's own assistant+usage commit is that sample's only (and so
        // its final) propose.
        let text_sample = &report.samples[1];
        assert_eq!(text_sample[hotl_engine::Phase::ToolsSpawned as usize], 0);
        assert_eq!(text_sample[hotl_engine::Phase::ToolsJoined as usize], 0);
        assert_ne!(
            text_sample[hotl_engine::Phase::BatchProposed as usize],
            0,
            "a no-tool sample's own commit must still be captured"
        );
        assert_ne!(
            text_sample[hotl_engine::Phase::WatermarkDurable as usize],
            0
        );
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

    // --- Task 9 (S2b pipelined commits) ------------------------------

    fn cfg_with(mode: hotl_engine::AckMode) -> EngineConfig {
        EngineConfig {
            ack_mode: mode,
            ..cfg()
        }
    }

    /// A permission-free call that stays running long enough for a steer to
    /// arrive mid-batch. Deliberately path-free and ask-free: the two ack
    /// modes are compared on the *normalized transcript*, and a `bash` ask
    /// would put a freshly-minted `pending_ask` id (and a tempdir path) into
    /// it, which normalization does not erase and which differs per run
    /// regardless of ack mode.
    struct Dawdle;

    impl hotl_tools::Tool for Dawdle {
        fn name(&self) -> &'static str {
            "dawdle"
        }
        fn description(&self) -> &str {
            "waits, then answers"
        }
        fn schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission(&self, _: &serde_json::Value) -> hotl_tools::Permission {
            hotl_tools::Permission::None
        }
        fn read_only(&self) -> bool {
            true
        }
        fn run<'a>(
            &'a self,
            _input: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                hotl_tools::ToolOutcome::ok("waited")
            })
        }
    }

    fn dawdle_registry() -> Registry {
        let mut reg = Registry::builtin();
        reg.register(Box::new(Dawdle));
        reg
    }

    /// The steer-during-tools scenario, driven through both ack modes.
    ///
    /// Seeded with [`anchored_history`] so the mispredict path is exercised
    /// *with rolling anchors present* — the cancelled speculation and the
    /// sequential rebuild both plan over a history that has already crossed a
    /// stride boundary. The seed is a projection seed only, so the normalized
    /// transcripts these callers compare are unaffected.
    async fn steered_tool_turn(mode: hotl_engine::AckMode) -> Harness {
        let mut h = Harness::with_items_and_registry(
            vec![
                ScriptedProvider::tool_call("t1", "dawdle", json!({})),
                ScriptedProvider::text_reply("Done, and noted your steer."),
            ],
            cfg_with(mode),
            anchored_history(),
            dawdle_registry(),
        );
        h.steer_on_tool_start = Some("also check the README".into());
        let outcome = h.prompt_and_wait("run the thing").await;
        assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
        h
    }

    /// commit-protocol.md test matrix case 6: "Steer between pipelined
    /// blocks → the steer is held per 72a6f1b, lands at the boundary after
    /// the assistant blocks it did not see, and the normalized transcript
    /// matches the same run in `Sync` mode."
    #[tokio::test]
    async fn a_steer_between_pipelined_commits_lands_after_the_drained_batch() {
        let pipelined = steered_tool_turn(hotl_engine::AckMode::Pipelined).await;

        let items = pipelined.items();
        let results = items
            .iter()
            .position(|i| matches!(i, Item::ToolResults { .. }))
            .expect("the batch committed");
        let steer = items.iter().position(is_steer).expect("the steer landed");
        assert!(
            steer > results,
            "a steer must never precede the results it did not see: {items:#?}"
        );

        // …and the pipeline changed nothing about what the log says.
        let sync = steered_tool_turn(hotl_engine::AckMode::Sync).await;
        assert_eq!(pipelined.kinds(), sync.kinds());
        assert_eq!(
            pipelined.transcript(),
            sync.transcript(),
            "pipelining must not reorder entries"
        );
    }

    /// The revision's second counter assertion: "the same session driven
    /// through `Sync` and `Pipelined` modes produces the same normalized
    /// transcript". A mode switch can only move ids and timestamps, which
    /// normalization already erases — anything else is a reordering.
    #[tokio::test]
    async fn both_ack_modes_produce_the_same_normalized_transcript() {
        let run = |mode| async move {
            let mut h = Harness::with_registry(
                vec![
                    ScriptedProvider::tool_call("t1", "dawdle", json!({})),
                    ScriptedProvider::text_reply("waited, as asked"),
                ],
                cfg_with(mode),
                dawdle_registry(),
            );
            h.prompt_and_wait("take your time").await;
            h.transcript()
        };
        assert_eq!(
            run(hotl_engine::AckMode::Pipelined).await,
            run(hotl_engine::AckMode::Sync).await
        );
    }

    /// A multi-entry proposal is **one causal group**: one writer message,
    /// one ack, and its items applied to the head whole or not at all (S2c).
    /// So a held steer released at that ack can only land after the entire
    /// group — there is no longer an instant where the first entry is
    /// projected and its siblings are still in the writer's queue.
    ///
    /// Task 9 wrote this against exactly that instant, which the group
    /// closes structurally; it is kept, repurposed, because it still pins
    /// the observable that instant used to break — the projection the next
    /// request is built from agrees with the log, item for item, in the
    /// log's order. That is the invariant, whichever mechanism holds it.
    #[tokio::test]
    async fn a_steer_lands_after_a_whole_group_and_the_next_request_agrees_with_the_log() {
        let mut h = Harness::new(vec![], cfg());
        for sub in ["web", "api"] {
            let dir = h.dir().join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("AGENTS.md"), format!("{sub} subproject rules")).unwrap();
            std::fs::write(dir.join("page.txt"), "content").unwrap();
        }
        // One batch, two fresh subdirs: tool results + two subdir-instruction
        // entries travel as ONE pipelined proposal.
        h.provider.push_script(tool_batch(&[
            (
                "t1",
                "read",
                json!({"path": h.dir().join("web/page.txt").to_str().unwrap()}),
            ),
            (
                "t2",
                "read",
                json!({"path": h.dir().join("api/page.txt").to_str().unwrap()}),
            ),
        ]));
        h.provider
            .push_script(ScriptedProvider::text_reply("read both"));
        h.steer_on_tool_start = Some("also check the README".into());
        let outcome = h.prompt_and_wait("read both pages").await;
        assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");

        // What the log says…
        let logged = h.items();
        let last_hint = logged
            .iter()
            .rposition(|i| {
                matches!(
                    i,
                    Item::User {
                        synthetic: Some(SyntheticReason::SubdirInstructions),
                        ..
                    }
                )
            })
            .expect("both hints committed");
        let steer = logged.iter().position(is_steer).expect("the steer landed");
        assert!(
            steer > last_hint,
            "the steer is logged after the whole proposal: {logged:#?}"
        );

        // …and what the next sample was built from must agree with it, item
        // for item. A projection that ran ahead of the log would order these
        // differently even though both contain the same items.
        let requests = h.provider.requests();
        let sampled: Vec<Item> = requests[1].items.iter().map(|i| (**i).clone()).collect();
        assert_eq!(
            sampled.as_slice(),
            &logged[..sampled.len()],
            "the projection must be the log, in the log's order"
        );
    }

    /// The revision's first counter assertion, at the engine level, measured
    /// against a real baseline rather than a bound that is true by accident:
    /// the SAME session in `Sync` mode spends exactly one `sync_data` per
    /// entry (queue depth 1, by construction), and `Pipelined` must beat it.
    /// (The 3-entries-1-sync property itself is pinned deterministically in
    /// hotl-store's
    /// `a_queued_batch_syncs_once_and_acks_every_entry_with_its_own_offset`;
    /// this is the end-to-end half, where the grouping is a real race against
    /// a real writer thread.)
    #[tokio::test]
    async fn a_multi_entry_pipelined_proposal_spends_fewer_syncs_than_sync_mode() {
        // One batch, two fresh subdirs: the tool-results entry plus two
        // subdir-instruction entries travel as ONE proposal — three entries
        // that a group commit collapses to one sync.
        async fn run(mode: hotl_engine::AckMode) -> (u64, u64) {
            let mut h = Harness::new(vec![], cfg_with(mode));
            for sub in ["web", "api"] {
                let dir = h.dir().join(sub);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("AGENTS.md"), format!("{sub} rules")).unwrap();
                std::fs::write(dir.join("page.txt"), "content").unwrap();
            }
            h.provider.push_script(tool_batch(&[
                (
                    "t1",
                    "read",
                    json!({"path": h.dir().join("web/page.txt").to_str().unwrap()}),
                ),
                (
                    "t2",
                    "read",
                    json!({"path": h.dir().join("api/page.txt").to_str().unwrap()}),
                ),
            ]));
            h.provider
                .push_script(ScriptedProvider::text_reply("read both"));
            let outcome = h.prompt_and_wait("read both pages").await;
            assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
            (h.fsync_count(), h.entries().len() as u64)
        }

        let (sync_syncs, sync_entries) = run(hotl_engine::AckMode::Sync).await;
        assert_eq!(
            sync_syncs, sync_entries,
            "without pipelining every entry pays its own fsync"
        );

        let (syncs, entries) = run(hotl_engine::AckMode::Pipelined).await;
        assert_eq!(entries, sync_entries, "the same session, the same log");
        assert!(
            syncs < sync_syncs,
            "group commit must beat the serial baseline: {syncs} syncs vs {sync_syncs} \
             for {entries} entries"
        );
    }

    // --- Task 10 (S2c causal groups) ---------------------------------

    /// commit-protocol.md §Causal groups (a), the revision's own counter
    /// assertion: "the `Completed` boundary spends exactly one `sync_data`
    /// (not two)". Both counts are deterministic — every commit here is one
    /// writer message on an otherwise idle queue, so the only difference
    /// between the modes is the boundary pair collapsing 2→1.
    #[tokio::test]
    async fn the_completed_boundary_spends_one_sync_instead_of_two() {
        async fn run(mode: hotl_engine::AckMode) -> (u64, Vec<String>) {
            let mut h = Harness::new(vec![ScriptedProvider::text_reply("hi")], cfg_with(mode));
            let outcome = h.prompt_and_wait("say hi").await;
            assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
            (h.fsync_count(), h.kinds())
        }

        let (sync_syncs, sync_kinds) = run(hotl_engine::AckMode::Sync).await;
        assert_eq!(
            sync_kinds,
            ["header", "item", "item", "usage"],
            "the scenario is header + prompt + the Completed pair"
        );
        assert_eq!(
            sync_syncs, 4,
            "entry at a time: header, prompt, assistant, usage — one sync each"
        );

        let (syncs, kinds) = run(hotl_engine::AckMode::Pipelined).await;
        assert_eq!(kinds, sync_kinds, "the same session, the same log");
        assert_eq!(
            syncs, 3,
            "the assistant item and its usage are ONE causal group: 2 syncs become 1"
        );
    }

    /// commit-protocol.md test matrix case 6, extended to the boundary group:
    /// a steer that arrives *inside* the sample window — after the request
    /// went out, while the `Completed` group is still in flight — is held,
    /// lands after the assistant item it could not have seen, and produces
    /// the same normalized transcript as the same run in `Sync` mode.
    ///
    /// This is the interleaving the boundary group creates: before S2c the
    /// pair committed synchronously, so there was no window between "the
    /// group was forwarded" and "the group is durable" for a steer to land
    /// in. The hold now ends at the *settle*, and the released steer joins
    /// the same published head as the group it followed.
    #[tokio::test]
    async fn a_steer_inside_the_boundary_group_lands_after_the_assistant_item() {
        async fn run(mode: hotl_engine::AckMode) -> Harness {
            let mut h = Harness::with_paused_completion(
                vec![ScriptedProvider::text_reply("the reply")],
                cfg_with(mode),
                std::time::Duration::from_millis(150),
            );
            h.steer_on_text_delta = Some("actually, do it differently".into());
            let outcome = h.prompt_and_wait("go").await;
            assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
            h
        }

        let pipelined = run(hotl_engine::AckMode::Pipelined).await;
        let items = pipelined.items();
        let assistant = items
            .iter()
            .position(|i| matches!(i, Item::Assistant { .. }))
            .expect("the assistant item committed");
        let steer = items.iter().position(is_steer).expect("the steer landed");
        assert!(
            steer > assistant,
            "a steer must never precede the reply that could not have seen it: {items:#?}"
        );

        let sync = run(hotl_engine::AckMode::Sync).await;
        assert_eq!(
            pipelined.transcript(),
            sync.transcript(),
            "the boundary group must not reorder anything"
        );
    }

    // --- Task 10 (S2c optimistic dispatch) ---------------------------

    /// A request with its MOIM turn-context block replaced by a constant.
    ///
    /// MOIM is the **sanctioned ephemeral suffix** (commit-protocol.md
    /// §Causal groups): it carries a wall-clock timestamp and the working
    /// directory, is regenerated on both paths, rides after the cache marker
    /// and never persists. Everything else is what byte-identity is claimed
    /// over.
    fn without_moim(req: &hotl_provider::SamplingRequest) -> hotl_provider::SamplingRequest {
        hotl_provider::SamplingRequest {
            turn_context: Some("<MOIM>".into()),
            ..req.clone()
        }
    }

    // --- Task 6 (Phase 5): cross-request cache-prefix stability -------
    //
    // Byte-identity between two builds of the *same* request is not enough:
    // the todo-marker bug this effort exists to close passed every such test,
    // because both build paths marked the same wrong block. The claims below
    // are about CONSECUTIVE requests — where markers land, and whether the
    // next request can still see the entry the last one wrote.

    /// An instant, permission-free, parallel-safe tool. The cache scenarios
    /// need *wide* turns (many `tool_use` blocks, many `tool_result` blocks)
    /// to reach a stride crossing at all, and `Dawdle`'s deliberate stall
    /// would turn a four-turn scenario into a multi-second test for nothing.
    struct Ping;

    impl hotl_tools::Tool for Ping {
        fn name(&self) -> &'static str {
            "ping"
        }
        fn description(&self) -> &str {
            "answers immediately"
        }
        fn schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission(&self, _: &serde_json::Value) -> hotl_tools::Permission {
            hotl_tools::Permission::None
        }
        fn read_only(&self) -> bool {
            true
        }
        fn parallel_safe(&self) -> bool {
            true
        }
        fn run<'a>(
            &'a self,
            _input: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
            Box::pin(async { hotl_tools::ToolOutcome::ok("pong") })
        }
    }

    fn ping_registry() -> Registry {
        let mut reg = Registry::builtin();
        reg.register(Box::new(Ping));
        reg
    }

    /// `turns` prompts, each one wide `ping` batch followed by a text reply.
    /// Every call carries a distinct id and input so the doom detector has no
    /// repeating signature to trip on.
    async fn tool_heavy_session(
        turns: usize,
        calls: usize,
        cache_ttl: hotl_provider::CacheTtl,
    ) -> Harness {
        let ids: Vec<String> = (0..turns * calls).map(|n| format!("t{n}")).collect();
        let mut scripts = Vec::new();
        for turn in 0..turns {
            let batch: Vec<(&str, &str, serde_json::Value)> = (0..calls)
                .map(|i| {
                    let n = turn * calls + i;
                    (ids[n].as_str(), "ping", json!({ "n": n }))
                })
                .collect();
            scripts.push(tool_batch(&batch));
            scripts.push(ScriptedProvider::text_reply("acknowledged"));
        }
        let mut h = Harness::with_registry(
            scripts,
            EngineConfig { cache_ttl, ..cfg() },
            ping_registry(),
        );
        for turn in 0..turns {
            let outcome = h.prompt_and_wait(&format!("prompt {turn}")).await;
            assert!(
                matches!(outcome, Outcome::Done { .. }),
                "turn {turn}: {outcome:?}"
            );
        }
        h
    }

    /// The ordinary case, and the one the effort is *for*: a tool-heavy
    /// session whose history outgrows the anchor stride several times over.
    /// Every consecutive pair of requests keeps the durable prefix byte-stable
    /// and every marker inside the lookback, so each sample reads the entry
    /// the previous sample wrote instead of re-billing the history.
    ///
    /// Driven at the interactive 1h TTL — the mode this effort ships for
    /// `hotl tui`/`acp`, and the only one that makes the TTL-ordering claim
    /// non-vacuous (under `FiveMinutes` every marker renders plain).
    ///
    /// Twelve calls per batch, not three: a turn has to grow the history by
    /// more than the lookback for claim 4 to have teeth at all. At this width,
    /// deleting the rolling anchors fails the assertion; at a narrow width the
    /// latest marker alone would still land inside the lookback and the suite
    /// would pass a session that re-bills its whole history.
    #[tokio::test]
    async fn normal_turns_grow_the_prefix_byte_stably() {
        let h = tool_heavy_session(3, 12, hotl_provider::CacheTtl::OneHour).await;
        let requests = h.provider.requests();
        assert_eq!(
            requests.len(),
            6,
            "three turns, two samples each — every speculative dispatch adopted"
        );
        assert_stable_cache_prefix(&requests);

        // Non-vacuous: the history really did outgrow the stride, so rolling
        // anchors exist, the budget is actually spent, and 1h reached the wire.
        let last = parsed(&hotl_provider_anthropic::wire_body(
            requests.last().expect("requests"),
        ));
        let marked = marker_positions(&last);
        assert!(
            marked.len() >= 2,
            "the fixture must produce at least one anchor besides the latest \
             marker, else claim 4 is trivial: {marked:?}"
        );
        assert_eq!(
            count_markers(&last),
            MARKER_BUDGET,
            "the deepest request spends the whole budget: {last:#}"
        );
        assert!(
            marker_ttls_in_prompt_order(&last)
                .iter()
                .any(|t| t.as_deref() == Some("1h")),
            "OneHour must actually reach the wire"
        );
    }

    /// A completed todo, so the reminder renders (the list is non-empty) while
    /// the TodoGate — a different subsystem, with its own tests — never fires
    /// and never adds samples this scenario would have to script for.
    fn done_todo(content: &str) -> hotl_types::Todo {
        hotl_types::Todo {
            content: content.into(),
            status: hotl_types::TodoStatus::Completed,
            active_form: None,
        }
    }

    /// The live billing bug, stated across requests. The `<todos>` reminder's
    /// bytes change whenever the list is edited; the original bug marked it,
    /// so every edit wrote a cache entry nothing ever read and re-billed the
    /// whole history at write price. With the reminder in the ephemeral tail
    /// the durable prefix is unmoved by any of it: no list → a list → an
    /// edited list changes only the appended, unmarked tail message.
    #[tokio::test]
    async fn todos_toggle_keeps_prefix_bytes_identical() {
        let mut h = Harness::with_registry(
            vec![
                ScriptedProvider::text_reply("no list yet"),
                ScriptedProvider::text_reply("list noted"),
                ScriptedProvider::text_reply("list edited"),
            ],
            cfg(),
            ping_registry(),
        );
        let first = h.prompt_and_wait("first").await;
        assert!(matches!(first, Outcome::Done { .. }), "turn 1: {first:?}");
        h.handle.set_todos(vec![done_todo("write the suite")]).await;
        let second = h.prompt_and_wait("second").await;
        assert!(matches!(second, Outcome::Done { .. }), "turn 2: {second:?}");
        h.handle
            .set_todos(vec![
                done_todo("write the suite"),
                done_todo("sweep the docs"),
            ])
            .await;
        let third = h.prompt_and_wait("third").await;
        assert!(matches!(third, Outcome::Done { .. }), "turn 3: {third:?}");

        let requests = h.provider.requests();
        assert_eq!(requests.len(), 3, "one sample per turn");
        assert_stable_cache_prefix(&requests);

        // Non-vacuous: three genuinely different tails — absent, then two
        // different renders of the same reminder.
        assert!(requests[0].ephemeral_tail.is_empty(), "no list yet");
        assert_eq!(requests[1].ephemeral_tail.len(), 1);
        assert_eq!(requests[2].ephemeral_tail.len(), 1);
        assert_ne!(
            requests[1].ephemeral_tail, requests[2].ephemeral_tail,
            "the fixture must actually edit the list"
        );

        for (i, req) in requests.iter().enumerate() {
            let full = parsed(&hotl_provider_anthropic::wire_body(req));
            let durable = durable_wire_body(req);
            let full_msgs = full["messages"].as_array().expect("messages");
            let durable_msgs = durable["messages"].as_array().expect("messages");
            // The durable region, markers included, is untouched by the tail.
            assert_eq!(
                full_msgs[..durable_msgs.len()],
                durable_msgs[..],
                "request {i}: the tail disturbed a durable byte or a marker"
            );
            // …and nothing after it carries a marker.
            for msg in &full_msgs[durable_msgs.len()..] {
                for block in msg["content"].as_array().expect("content") {
                    assert!(
                        block.get("cache_control").is_none(),
                        "request {i}: the ephemeral tail carries a marker: {block:#}"
                    );
                }
            }
        }
    }

    /// A steer landing at a sample boundary is an **append**, not a rewrite:
    /// the cancelled speculative request is a strict prefix of the sequential
    /// rebuild that replaces it, so no cache entry the session already wrote
    /// is orphaned by the interruption.
    #[tokio::test]
    async fn steer_injection_appends_without_prefix_break() {
        let h = steered_tool_turn(hotl_engine::AckMode::Pipelined).await;
        let requests = h.provider.requests();
        assert_eq!(
            requests.len(),
            3,
            "the first sample, the cancelled speculation, and the rebuild"
        );
        assert_stable_cache_prefix(&requests);

        // Non-vacuous: the steer really did land, and it landed as an append.
        let speculative = &requests[1];
        let rebuilt = requests.last().expect("requests");
        assert!(
            rebuilt.items.iter().any(is_steer),
            "the steer must reach the rebuilt request: {:#?}",
            rebuilt.items
        );
        assert!(
            speculative.items.len() < rebuilt.items.len(),
            "the rebuild must be strictly longer than the speculation it replaced"
        );
        assert_eq!(
            speculative.items[..],
            rebuilt.items[..speculative.items.len()],
            "the speculative request must be a prefix of its rebuild, byte for byte"
        );
    }

    /// A session driven past its compaction trigger. Window 1000 → trigger at
    /// 800, tail budget 300: a big first tool result, then a sample that
    /// "reports" 750 tokens, so the next estimate crosses the trigger and the
    /// turn folds. Shared by the golden fold test and the two cache claims
    /// about it, so all three are talking about the same fold.
    async fn compacting_session() -> Harness {
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
        h
    }

    /// Compaction is the ONE place the durable prefix is allowed to change out
    /// from under the cache: the fold rewrites history, so the entries behind
    /// it are dead by construction. What must survive is the *prefix* segment
    /// — system + tools, sealed by the system marker — since that is the part
    /// a fold does not touch and the part most expensive to rebuild.
    #[tokio::test]
    async fn compaction_is_the_single_sanctioned_discontinuity() {
        let h = compacting_session().await;
        let all = h.provider.requests();
        // The summarize call is its own tiny conversation on the cache-off
        // path; the session's own requests are what the prefix claim is about.
        let session: Vec<SamplingRequest> = all
            .iter()
            .filter(|r| r.cache != hotl_provider::CachePolicy::Off)
            .cloned()
            .collect();
        assert_eq!(
            session.len(),
            3,
            "two samples before the fold, one continuation after"
        );
        for (i, req) in session.iter().enumerate() {
            assert_marker_budget_and_ttl_order(req, &format!("session request {i}"));
        }
        assert_eq!(
            cache_prefix_breaks(&session),
            vec![1],
            "exactly one discontinuity, and it is the fold"
        );

        let before = parsed(&hotl_provider_anthropic::wire_body(&session[1]));
        let after = parsed(&hotl_provider_anthropic::wire_body(&session[2]));
        assert_eq!(
            before["system"], after["system"],
            "the system segment — and so the entry the system marker wrote — \
             must survive the fold byte for byte"
        );
        assert_eq!(before["tools"], after["tools"], "tools must survive too");
        assert_eq!(
            after["system"][0]["cache_control"]["type"], "ephemeral",
            "…and the continuation must still mark it"
        );
    }

    /// `CachePolicy::Off` is a policy, not a hint. The compaction summarize
    /// call is the production path that uses it: a one-shot conversation that
    /// will never be sampled against again, so a marker on it would be a write
    /// nothing can ever read.
    #[tokio::test]
    async fn cache_off_paths_emit_zero_markers() {
        let h = compacting_session().await;
        let all = h.provider.requests();
        let off: Vec<&SamplingRequest> = all
            .iter()
            .filter(|r| r.cache == hotl_provider::CachePolicy::Off)
            .collect();
        assert_eq!(
            off.len(),
            1,
            "this scenario has exactly one cache-off path: the summarize call"
        );
        assert!(
            off[0].system.contains("compress"),
            "…and it is the summarize call: {}",
            off[0].system
        );
        let body = hotl_provider_anthropic::wire_body(off[0]);
        assert!(
            !body.to_string().contains("cache_control"),
            "a cache-off request must put no marker on the wire: {body:#}"
        );
        // Non-vacuous: the same run's session requests do mark.
        assert!(
            all.iter()
                .any(|r| count_markers(&parsed(&hotl_provider_anthropic::wire_body(r))) > 0),
            "the fixture must also exercise the marking path"
        );
    }

    /// A synthetic history long enough that the breakpoint planner has already
    /// placed a rolling anchor before the scenario's own turn starts — 14
    /// user/assistant pairs, so 28 wire blocks and a crossing at block 15.
    ///
    /// Case 9's byte-identity claim is only interesting where the two build
    /// paths have anchors to disagree about, and it is strongest where the
    /// speculated tail crosses the *next* stride boundary: the sample-2
    /// request carries an anchor the sample-1 request did not.
    ///
    /// Seeded through `initial_items`, so it is a projection seed and never
    /// reaches the log — normalized-transcript comparisons are unaffected.
    fn anchored_history() -> Vec<Item> {
        (0..14)
            .flat_map(|i| {
                vec![
                    Item::User {
                        text: format!("earlier request {i}"),
                        synthetic: None,
                        images: Vec::new(),
                    },
                    Item::Assistant {
                        blocks: vec![json!({"type": "text", "text": format!("earlier reply {i}")})],
                    },
                ]
            })
            .collect()
    }

    /// A path-free tool round trip, so two runs in two tempdirs produce
    /// byte-identical histories (a path in a tool input would differ per
    /// run for reasons that have nothing to do with adoption).
    ///
    /// The batch is two calls wide on purpose: with a single `tool_result` the
    /// batch's only block *is* the latest marker, and a stride crossing that
    /// lands on it is deduped away — there would be no new anchor for the two
    /// build paths to disagree about. See [`anchored_history`].
    async fn dawdle_then_reply(mode: hotl_engine::AckMode) -> Harness {
        let mut h = Harness::with_items_and_registry(
            vec![
                tool_batch(&[("t1", "dawdle", json!({})), ("t2", "dawdle", json!({}))]),
                ScriptedProvider::text_reply("done"),
            ],
            cfg_with(mode),
            anchored_history(),
            dawdle_registry(),
        );
        let outcome = h.prompt_and_wait("go").await;
        assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
        h
    }

    /// [`dawdle_then_reply`] with a live ephemeral tail. The todo is
    /// **completed** on purpose: the reminder renders (the list is non-empty)
    /// but the TodoGate never fires, so the script still needs exactly the two
    /// samples adoption is measured over.
    async fn dawdle_then_reply_with_todos(mode: hotl_engine::AckMode) -> Harness {
        let mut h = Harness::with_items_and_registry(
            vec![
                tool_batch(&[("t1", "dawdle", json!({})), ("t2", "dawdle", json!({}))]),
                ScriptedProvider::text_reply("done"),
            ],
            cfg_with(mode),
            anchored_history(),
            dawdle_registry(),
        );
        h.handle
            .set_todos(vec![hotl_types::Todo {
                content: "already done".into(),
                status: hotl_types::TodoStatus::Completed,
                active_form: None,
            }])
            .await;
        let outcome = h.prompt_and_wait("go").await;
        assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
        h
    }

    /// Rolling anchors in a request's plan: every marker in the durable body
    /// except the deepest, which is always `latest`.
    fn anchor_count(req: &SamplingRequest) -> usize {
        marker_positions(&durable_wire_body(req))
            .len()
            .saturating_sub(1)
    }

    /// The anchor half of case 9's fixture, asserted rather than assumed: the
    /// first request already carries a rolling anchor, and the turn's tool
    /// batch crosses the next stride boundary, so the second request carries
    /// an anchor the first did not. Without both, "the adopted request equals
    /// the rebuild" is a claim about a history with no anchors in it.
    ///
    /// Only applies to fixtures whose batch is **at least two calls wide** —
    /// see [`dawdle_then_reply`]. With a one-call batch the single
    /// `tool_result` block is itself `latest`, a crossing landing on it is
    /// deduped away, and no new anchor appears; assert
    /// [`anchor_count`] directly on such a fixture instead.
    fn assert_the_speculated_tail_crossed_an_anchor(requests: &[SamplingRequest]) {
        let first = marker_positions(&durable_wire_body(&requests[0]));
        let second = marker_positions(&durable_wire_body(&requests[1]));
        assert!(
            anchor_count(&requests[0]) >= 1,
            "the seeded history must already have a rolling anchor: {first:?}"
        );
        assert!(
            anchor_count(&requests[1]) > anchor_count(&requests[0]),
            "the speculated tail must cross a stride boundary — a NEW anchor, \
             not merely a new latest: {first:?} -> {second:?}"
        );
    }

    /// commit-protocol.md test matrix case 9 (MANDATORY): "under leaf
    /// equality, the adopted speculative request's bytes equal the
    /// sequentially rebuilt request's, byte for byte".
    ///
    /// The two runs are the same scenario in the two ack modes: `Pipelined`
    /// dispatches the second sample optimistically at the boundary and adopts
    /// it (proved by the request count — a refusal would show up as a third,
    /// rebuilt request), `Sync` issues no ticket and so never speculates,
    /// which makes its second request the sequential rebuild by construction.
    ///
    /// Both levels the brief asks for: the `SamplingRequest` the engine
    /// hands the provider, and the JSON body a real dialect puts on the wire.
    #[tokio::test]
    async fn an_adopted_request_is_byte_identical_to_the_sequential_rebuild() {
        let adopted_run = dawdle_then_reply(hotl_engine::AckMode::Pipelined).await;
        let sequential_run = dawdle_then_reply(hotl_engine::AckMode::Sync).await;

        let adopted = adopted_run.provider.requests();
        let sequential = sequential_run.provider.requests();
        assert_eq!(
            adopted.len(),
            2,
            "the optimistic dispatch WAS adopted: a refusal would rebuild, making three"
        );
        assert_eq!(sequential.len(), 2, "Sync mode never speculates");
        // Anchors are in play on both paths — see the helper for why that is
        // what makes the byte-identity claim load-bearing rather than trivial.
        assert_the_speculated_tail_crossed_an_anchor(&adopted);
        assert_the_speculated_tail_crossed_an_anchor(&sequential);

        // Level 1: the request the engine hands the provider.
        assert_eq!(
            format!("{:?}", without_moim(&adopted[1])),
            format!("{:?}", without_moim(&sequential[1])),
            "the adopted request must equal the sequential rebuild"
        );

        // Level 2: the provider-serialized body — the actual bytes.
        assert_eq!(
            hotl_provider_anthropic::wire_body(&without_moim(&adopted[1])),
            hotl_provider_anthropic::wire_body(&without_moim(&sequential[1])),
            "…and so must the wire body built from it"
        );

        // MOIM itself is regenerated on both paths and still numbers the
        // sample it actually belongs to — the speculative build is one
        // sample ahead of the sample it was built during.
        for req in [&adopted[1], &sequential[1]] {
            let moim = req.turn_context.as_deref().expect("MOIM attached");
            assert!(moim.contains("sample=\"2\""), "was: {moim}");
        }
    }

    /// Case 9 again, now with a live ephemeral tail — the half the split adds.
    ///
    /// `Turn::predicted_snapshot` *carries* the tail rather than re-deriving
    /// it; if that carry were wrong (dropped, duplicated, or folded back into
    /// `items`) the adopted request would stop matching the sequential rebuild
    /// here, and only here. Also pins the structural guarantee the later
    /// breakpoint planner is built on: `items` is durable-only on BOTH paths.
    #[tokio::test]
    async fn an_adopted_request_with_an_ephemeral_tail_still_equals_the_rebuild() {
        let adopted_run = dawdle_then_reply_with_todos(hotl_engine::AckMode::Pipelined).await;
        let sequential_run = dawdle_then_reply_with_todos(hotl_engine::AckMode::Sync).await;

        let adopted = adopted_run.provider.requests();
        let sequential = sequential_run.provider.requests();
        assert_eq!(adopted.len(), 2, "the optimistic dispatch WAS adopted");
        assert_eq!(sequential.len(), 2, "Sync mode never speculates");
        assert_the_speculated_tail_crossed_an_anchor(&adopted);

        // Non-vacuous: there really is an ephemeral tail on both paths.
        for req in [&adopted[1], &sequential[1]] {
            assert_eq!(
                req.ephemeral_tail.len(),
                1,
                "fixture: the todo reminder must be live"
            );
            assert!(
                !req.items.iter().any(|i| matches!(
                    &**i,
                    Item::User {
                        synthetic: Some(hotl_types::SyntheticReason::Todos),
                        ..
                    }
                )),
                "`items` must stay durable-only: {:#?}",
                req.items
            );
        }

        assert_eq!(
            format!("{:?}", without_moim(&adopted[1])),
            format!("{:?}", without_moim(&sequential[1])),
            "the adopted request must equal the sequential rebuild"
        );
        assert_eq!(
            hotl_provider_anthropic::wire_body(&without_moim(&adopted[1])),
            hotl_provider_anthropic::wire_body(&without_moim(&sequential[1])),
            "…and so must the wire body built from it"
        );
    }

    /// The mispredict half of case 9: "the mispredict path (a steer at the
    /// boundary) cancels without emitting an entry, without a `Failed`
    /// outcome, and produces the same transcript as the sequential path."
    #[tokio::test]
    async fn a_mispredicted_speculation_cancels_without_a_trace() {
        let mispredicted = steered_tool_turn(hotl_engine::AckMode::Pipelined).await;

        assert_eq!(
            mispredicted.provider.request_count(),
            3,
            "the mispredict costs exactly one cancelled request, and nothing else"
        );
        // Un-adopted deltas are ephemeral: they die before any proposal, so
        // they never reach the surface. The scripted reply emits exactly one
        // TextDelta — seeing two would mean the cancelled stream leaked.
        let deltas = mispredicted
            .seen
            .iter()
            .filter(|e| e.starts_with("TextDelta("))
            .count();
        assert_eq!(
            deltas, 1,
            "an un-adopted stream's deltas must never be forwarded: {:?}",
            mispredicted.seen
        );
        // Cancelled ≠ Failed: cancelling an un-adopted stream is not a turn
        // outcome and produces no entry — "not even a `cancelled` one".
        assert!(
            !mispredicted.kinds().iter().any(|k| k == "cancelled"),
            "kinds: {:?}",
            mispredicted.kinds()
        );

        // The mispredict path must be exercised WITH rolling anchors present,
        // or it proves nothing about the planner. `steered_tool_turn`'s
        // `anchored_history` seed is what supplies them, and nothing else in
        // this test would fail if that seed shrank — so say it here.
        //
        // `assert_the_speculated_tail_crossed_an_anchor` does NOT fit this
        // fixture: its batch is a single `dawdle` call, so the one
        // `tool_result` block *is* `latest` and the crossing that lands on it
        // is deduped away — both requests carry exactly one anchor, and the
        // helper's "strictly more anchors" half would fail. The claim here is
        // only that anchors exist at all.
        let reqs = mispredicted.provider.requests();
        for (label, req) in [
            ("the cancelled speculation", &reqs[1]),
            ("its sequential rebuild", &reqs[2]),
        ] {
            assert!(
                anchor_count(req) >= 1,
                "{label} must plan over a history that already has a rolling \
                 anchor: {:?}",
                marker_positions(&durable_wire_body(req))
            );
        }

        // …and the transcript is the one a never-speculated run produces.
        let sequential = steered_tool_turn(hotl_engine::AckMode::Sync).await;
        assert_eq!(
            sequential.provider.request_count(),
            2,
            "Sync mode never speculates, so it never mispredicts"
        );
        assert!(
            anchor_count(&sequential.provider.requests()[1]) >= 1,
            "the never-speculated rebuild plans over the same anchored history"
        );
        assert_eq!(mispredicted.transcript(), sequential.transcript());
    }

    /// A tool that reports how much of the session log was on disk at the
    /// moment it ran — barrier (a)'s only observable: "no external side
    /// effect until the `tool_use` blocks that caused it are durable".
    struct LogWatcher(Arc<std::sync::Mutex<Option<std::path::PathBuf>>>);

    impl hotl_tools::Tool for LogWatcher {
        fn name(&self) -> &'static str {
            "watch_log"
        }
        fn description(&self) -> &str {
            "counts durable log lines"
        }
        fn schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission(&self, _: &serde_json::Value) -> hotl_tools::Permission {
            hotl_tools::Permission::None
        }
        fn read_only(&self) -> bool {
            true
        }
        fn parallel_safe(&self) -> bool {
            true
        }
        fn run<'a>(
            &'a self,
            _input: serde_json::Value,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
            let path = self.0.lock().expect("log path").clone();
            Box::pin(async move {
                match path.and_then(|p| std::fs::read_to_string(p).ok()) {
                    Some(text) => hotl_tools::ToolOutcome::ok(text.lines().count().to_string()),
                    None => hotl_tools::ToolOutcome::err("no log"),
                }
            })
        }
    }

    async fn watched_batch(calls: &[(&str, &str, serde_json::Value)]) -> Vec<String> {
        // The registry has to exist before the session log does, so the tool
        // is handed a cell the harness fills in once the log is created.
        let cell = Arc::new(std::sync::Mutex::new(None));
        let mut registry = Registry::builtin();
        registry.register(Box::new(LogWatcher(cell.clone())));
        let mut h = Harness::with_registry(
            vec![tool_batch(calls), ScriptedProvider::text_reply("done")],
            cfg(),
            registry,
        );
        *cell.lock().unwrap() = Some(h.log_path().to_path_buf());
        let outcome = h.prompt_and_wait("watch the log").await;
        assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
        let items = h.items();
        let Item::ToolResults { results } = items
            .iter()
            .find(|i| matches!(i, Item::ToolResults { .. }))
            .expect("results")
        else {
            unreachable!()
        };
        results.iter().map(|r| r.content.clone()).collect()
    }

    /// commit-protocol.md §Pipelined commits barrier (a): "no external side
    /// effect until the `tool_use` blocks that caused it are durable … and it
    /// holds on the inline single-tool execution path exactly as it does on
    /// the spawned batch path". By the time a call runs, every entry this
    /// turn committed — header, prompt, assistant blocks, usage — is on
    /// disk, and the turn holds no unresolved ticket.
    #[tokio::test]
    async fn the_inline_tool_path_dispatches_only_behind_the_durability_barrier() {
        let seen = watched_batch(&[("t1", "watch_log", json!({}))]).await;
        assert_eq!(
            seen,
            vec!["4".to_string()],
            "header + prompt + assistant + usage must all be durable before the call runs"
        );
    }

    #[tokio::test]
    async fn the_batch_tool_path_dispatches_only_behind_the_durability_barrier() {
        let seen = watched_batch(&[
            ("t1", "watch_log", json!({})),
            ("t2", "watch_log", json!({})),
        ])
        .await;
        assert_eq!(seen, vec!["4".to_string(), "4".to_string()]);
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
        // The plan folds the big early history and keeps the small recent
        // exchange verbatim — see `compacting_session` for the arithmetic.
        let h = compacting_session().await;
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
            &*continuation.items[0],
            Item::User { synthetic: Some(SyntheticReason::CompactionSummary), text, .. }
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
            tc.contains("sample=\"1\"") && !tc.contains("context_used="),
            "pct is opt-in since 0030; was: {tc}"
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
            images: Vec::new(),
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
                    &***i,
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
                images: Vec::new(),
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
            &**i,
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
                .any(|i| matches!(&**i, Item::ToolResults { .. } | Item::Assistant { .. })),
            "reset-mode continuation must not carry the verbatim tail: {:?}",
            continuation.items
        );
    }

    #[tokio::test]
    async fn moim_context_pct_can_be_shown() {
        let mut h = Harness::new(
            vec![ScriptedProvider::text_reply("hi")],
            EngineConfig {
                show_context_pct: true,
                ..cfg()
            },
        );
        h.prompt_and_wait("hello").await;
        let reqs = h.provider.requests();
        let tc = reqs[0].turn_context.as_deref().unwrap();
        assert!(tc.contains("context_used"), "pct rides when opted in: {tc}");
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
        // 60KB spread over 1000 lines, not one 60KB line: `read` clips any
        // single line at 8KB, so a one-line fixture is no longer a large
        // *result* and would never reach the eviction threshold this test is
        // about.
        std::fs::write(&big, format!("{}\n", "B".repeat(59)).repeat(1000)).unwrap();
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
