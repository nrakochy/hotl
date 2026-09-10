//! 0055 T1: a parallel tool batch is bounded by the Layer-B `subprocs`
//! budget. Before this, a 40-call batch of parallel-safe calls forked 40
//! children at once — green threads are free, the processes behind them are
//! not. The permit is drawn *inside* each future, so gating stays serial and
//! `join_all` still returns in source order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::concurrency::{ConcurrencyLimits, SessionConcurrency};
use hotl_tools::{rules::Rules, Permission, Registry, Tool, ToolOutcome};
use hotl_types::{StopReason, TokenUsage};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Records the high-water mark of simultaneous executions. Every call holds
/// the same fixed window: a latch that releases once the expected width is
/// seen would let every later call pass straight through, and the test would
/// pass with no budget at all.
struct FanoutProbe {
    running: Arc<AtomicUsize>,
    max_seen: Arc<AtomicUsize>,
    hold: Duration,
}

impl Tool for FanoutProbe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn description(&self) -> &str {
        "test fan-out probe"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn read_only(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: Value,
        _cancel: CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now, Ordering::SeqCst);
            // The whole batch is polled inside one `join_all`, so every call
            // that holds a permit enters within microseconds of its peers —
            // the window only has to outlast that, not a scheduler drift.
            tokio::time::sleep(self.hold).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            ToolOutcome::ok("probed")
        })
    }
}

/// One assistant reply carrying `n` distinct `probe` calls. Distinct inputs
/// on purpose: identical parallel-safe calls are deduplicated before dispatch
/// (0037 D6) and would never reach the budget at all.
fn probe_batch(n: usize) -> Vec<Result<StreamEvent, hotl_provider::ProviderError>> {
    let blocks: Vec<Value> = (0..n)
        .map(|i| json!({"type": "tool_use", "id": format!("p{i}"), "name": "probe", "input": {"n": i}}))
        .collect();
    vec![
        Ok(StreamEvent::Started),
        Ok(StreamEvent::Completed {
            stop: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 8,
                ..Default::default()
            },
            blocks,
        }),
    ]
}

/// Prompts one session whose only turn is `batch`, and waits for it to end.
/// The tempdir guard is returned with nothing to do — dropping it early would
/// pull the session log out from under the actor.
async fn run_one_turn(
    registry: Registry,
    concurrency: SessionConcurrency,
    batch: Vec<Result<StreamEvent, hotl_provider::ProviderError>>,
    timeout: Duration,
) {
    let config = EngineConfig::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        batch,
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle: SessionHandle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(registry),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
        concurrency,
    });
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(timeout, handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }
}

/// Runs one 12-call probe batch under `limits` and returns the high-water
/// mark of simultaneous probe executions.
async fn max_concurrent(limits: ConcurrencyLimits) -> usize {
    const CALLS: usize = 12;
    let running = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::builtin();
    registry.register(Box::new(FanoutProbe {
        running,
        max_seen: max_seen.clone(),
        hold: Duration::from_millis(150),
    }));
    run_one_turn(
        registry,
        SessionConcurrency::new(limits),
        probe_batch(CALLS),
        Duration::from_secs(60),
    )
    .await;
    max_seen.load(Ordering::SeqCst)
}

/// A narrowed budget caps the batch, and the default budget still runs it
/// wide: the permit bounds fan-out, it does not serialize it.
#[tokio::test]
async fn parallel_batch_never_exceeds_the_subproc_budget() {
    let narrow = max_concurrent(ConcurrencyLimits {
        subprocs: 3,
        ..Default::default()
    })
    .await;
    assert_eq!(
        narrow, 3,
        "12 parallel-safe calls under a budget of 3 ran {narrow}-wide"
    );

    let default = ConcurrencyLimits::default();
    let wide = max_concurrent(default).await;
    assert_eq!(
        wide, default.subprocs,
        "12 parallel-safe calls under the default budget of {} ran {wide}-wide",
        default.subprocs
    );
    assert!(
        wide > 1,
        "the default budget serialized the batch (max {wide})"
    );
}

/// A tool that draws a `subproc()` permit of its own from the same budget —
/// what a `spawn`/`workflow` child does the moment it runs its first tool.
struct NestingProbe {
    concurrency: SessionConcurrency,
}

impl Tool for NestingProbe {
    fn name(&self) -> &'static str {
        "nest"
    }
    fn description(&self) -> &str {
        "test nesting probe"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn awaits_child_session(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: Value,
        _cancel: CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let _child = self.concurrency.subproc().await;
            ToolOutcome::ok("nested")
        })
    }
}

/// The deadlock this exemption exists for: a parent holding a leaf permit
/// across a child that needs one of its own never gets it back. With
/// `subprocs: 1` the turn only ends if the nesting call drew no permit.
#[tokio::test]
async fn a_nesting_tool_draws_no_subproc_permit() {
    let concurrency = SessionConcurrency::new(ConcurrencyLimits {
        subprocs: 1,
        ..Default::default()
    });
    let mut registry = Registry::builtin();
    registry.register(Box::new(NestingProbe {
        concurrency: concurrency.clone(),
    }));
    let batch = vec![
        Ok(StreamEvent::Started),
        Ok(StreamEvent::Completed {
            stop: StopReason::ToolUse,
            usage: TokenUsage::default(),
            blocks: vec![json!({"type": "tool_use", "id": "n1", "name": "nest", "input": {}})],
        }),
    ];
    run_one_turn(registry, concurrency, batch, Duration::from_secs(10)).await;
}
