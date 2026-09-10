//! 0029: `EngineConfig::effort` must reach the composed `SamplingRequest`, and
//! the compaction summarize must never inherit it — a `max`-effort session
//! folding its own history at `max` rates is a cost surprise nobody opted into.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{
    Effort, Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent,
};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

/// Routes summarize requests (identified by the compaction system prompt) to
/// their own script, so each side's request can be inspected separately.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
}

impl Provider for Router {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        if req.system.contains("compress") {
            self.summarize.stream(req)
        } else {
            self.main.stream(req)
        }
    }
}

fn session(
    provider: Arc<dyn Provider>,
    config: EngineConfig,
) -> (SessionHandle, tempfile::TempDir) {
    let (handle, dir, _) = session_logged(provider, config);
    (handle, dir)
}

fn session_logged(
    provider: Arc<dyn Provider>,
    config: EngineConfig,
) -> (SessionHandle, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        config,
    });
    (handle, dir, log_path)
}

async fn run_one_turn(handle: &mut SessionHandle) {
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_configured_effort_reaches_the_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(
        provider.clone(),
        EngineConfig {
            effort: Some(Effort::High),
            ..Default::default()
        },
    );
    run_one_turn(&mut handle).await;
    let req = provider.last_request().expect("one request");
    assert_eq!(req.effort, Some(Effort::High));
}

// Engine contract only: since 0030 the CLI layer injects a session default
// (`xhigh` on catalogued Anthropic), so `None` here is not the product behavior.
#[tokio::test]
async fn an_unconfigured_session_sends_no_effort() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(provider.clone(), EngineConfig::default());
    run_one_turn(&mut handle).await;
    assert_eq!(provider.last_request().expect("one request").effort, None);
}

/// Ledger 12: compaction is pinned to no effort regardless of the session's.
///
/// The fold happens mid-turn, so the fixture needs tool calls to reach a
/// second step — a plain text reply ends the turn before the trigger is read.
#[tokio::test]
async fn compaction_never_inherits_the_sessions_effort() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "DIGEST",
    )]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
    });
    let (mut handle, dir) = session(
        provider,
        EngineConfig {
            effort: Some(Effort::Max),
            context_window: 1000,
            max_turns: 4,
            ..Default::default()
        },
    );
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    for i in 0..10 {
        let mut script =
            ScriptedProvider::tool_call(&format!("t{i}"), "read", json!({"path": path}));
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.input_tokens = if i == 0 { 650 } else { 850 };
        }
        main.push_script(script);
    }
    run_one_turn(&mut handle).await;
    let folded = summarize.last_request().expect("the fixture must compact");
    assert_eq!(
        folded.effort, None,
        "compaction must not pay the session's rate"
    );
    assert_eq!(
        main.last_request().expect("a main request").effort,
        Some(Effort::Max)
    );
}

#[tokio::test]
async fn set_effort_reaches_the_next_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(provider.clone(), EngineConfig::default());
    handle.set_effort(Some(Effort::XHigh)).await;
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("one request").effort,
        Some(Effort::XHigh)
    );
}

/// Mirrors `set_mode.rs`: the command writes its own entry kind and no other.
#[tokio::test]
async fn set_effort_writes_an_effort_set_and_nothing_else() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir, log_path) = session_logged(provider, EngineConfig::default());
    handle.set_effort(Some(Effort::Max)).await;
    handle.set_effort(None).await;
    handle.set_effort(Some(Effort::Low)).await;
    run_one_turn(&mut handle).await;

    let entries: Vec<Option<String>> = std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            hotl_types::EntryPayload::EffortSet { effort } => Some(effort),
            hotl_types::EntryPayload::ModeSet { .. } | hotl_types::EntryPayload::PlanSet { .. } => {
                panic!("SetEffort must not write a mode_set or plan_set")
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        entries,
        vec![Some("max".to_string()), None, Some("low".to_string())]
    );
}

/// Clearing is its own act: `SetEffort(None)` after `Some(Max)` restores the
/// provider's default rather than leaving `Max` in place.
#[tokio::test]
async fn unset_round_trips_through_the_log() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir, log_path) = session_logged(
        provider.clone(),
        EngineConfig {
            effort: Some(Effort::High),
            ..Default::default()
        },
    );
    handle.set_effort(Some(Effort::Max)).await;
    handle.set_effort(None).await;
    run_one_turn(&mut handle).await;
    assert_eq!(provider.last_request().expect("one request").effort, None);

    // And replay agrees: "cleared" survives as its own value, distinct from
    // "never set" (which is the outer `None`).
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert_eq!(replayed.effort, Some(None));
}

// ---------------------------------------------------------------------------
// 0059 T1 — effort as a schedule over phases.

fn schedule() -> hotl_provider::EffortSchedule {
    hotl_provider::EffortSchedule {
        plan: Some(Effort::XHigh),
        implement: Some(Effort::High),
        verify: Some(Effort::Max),
    }
}

fn scheduled_config() -> EngineConfig {
    EngineConfig {
        effort: Some(Effort::Low),
        effort_schedule: Some(schedule()),
        ..Default::default()
    }
}

/// Plan mode alone decides the `plan` phase (0056's active node is the seam
/// this will read when it lands).
#[tokio::test]
async fn plan_mode_opens_the_turn_at_the_plan_rung() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());
    handle.set_plan(true).await;
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("one request").effort,
        Some(Effort::XHigh)
    );
}

/// A turn that ran a `bash cargo test` and edited nothing puts the NEXT turn
/// in `verify`; a batch that also edited does not.
#[tokio::test]
async fn a_verify_only_batch_puts_the_next_turn_at_the_verify_rung() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    // The rung is read off the command *string*; the subprocess is incidental,
    // so it is bounded — an unbounded `cargo test` here really does run one.
    provider.push_script(ScriptedProvider::tool_call(
        "t1",
        "bash",
        json!({"command": "cargo test --workspace", "timeout_ms": 2000}),
    ));
    provider.push_script(ScriptedProvider::text_reply("checked"));
    provider.push_script(ScriptedProvider::text_reply("next"));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());
    run_one_turn(&mut handle).await;
    // Turn 1 opened in `implement` — nothing had run yet.
    assert_eq!(provider.requests()[0].effort, Some(Effort::High));
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("a third request").effort,
        Some(Effort::Max),
        "a verify-only batch moves the next turn to the verify rung"
    );
}

/// An edit in the same batch means the turn was implementing, whatever else
/// it ran alongside.
#[tokio::test]
async fn a_batch_that_edits_stays_at_the_implement_rung() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "body").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    // Bounded like its siblings: green here only because the runner has no
    // nextest, which is not something to depend on.
    let mut batch = ScriptedProvider::tool_call(
        "t1",
        "bash",
        json!({"command": "cargo nextest run", "timeout_ms": 2000}),
    );
    if let Some(Ok(StreamEvent::Completed { blocks, .. })) = batch.last_mut() {
        blocks.push(json!({
            "type": "tool_use", "id": "t2", "name": "write",
            "input": {"path": path, "content": "new"}
        }));
    }
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    provider.push_script(batch);
    provider.push_script(ScriptedProvider::text_reply("done"));
    provider.push_script(ScriptedProvider::text_reply("next"));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());
    run_one_turn(&mut handle).await;
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("a request").effort,
        Some(Effort::High),
        "a batch that edited is implementation, not verification"
    );
}

/// The rung moves at turn start and nowhere else: every sample inside one
/// turn carries the same depth even when plan mode flips mid-turn.
#[tokio::test]
async fn the_rung_changes_only_at_turn_start() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    // Bounded for the same reason as above: the string is what is under test.
    provider.push_script(ScriptedProvider::tool_call(
        "t1",
        "bash",
        json!({"command": "cargo test", "timeout_ms": 2000}),
    ));
    provider.push_script(ScriptedProvider::tool_call(
        "t2",
        "bash",
        json!({"command": "cargo test", "timeout_ms": 2000}),
    ));
    provider.push_script(ScriptedProvider::text_reply("done"));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());
    run_one_turn(&mut handle).await;
    let rungs: Vec<_> = provider.requests().iter().map(|r| r.effort).collect();
    assert!(rungs.len() >= 3, "{rungs:?}");
    assert!(
        rungs.iter().all(|r| *r == Some(Effort::High)),
        "one turn, one rung: {rungs:?}"
    );
}

/// `/effort` pins a scalar for the session: the schedule stops writing.
#[tokio::test]
async fn a_hand_set_rung_pins_the_session_and_stops_the_schedule() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("a"),
        ScriptedProvider::text_reply("b"),
    ]));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());
    handle.set_effort(Some(Effort::Medium)).await;
    run_one_turn(&mut handle).await;
    handle.set_plan(true).await;
    run_one_turn(&mut handle).await;
    assert!(
        provider
            .requests()
            .iter()
            .all(|r| r.effort == Some(Effort::Medium)),
        "a pinned rung outranks the schedule"
    );
}

/// A phase with no rung inherits the session's own effort rather than
/// clearing it.
#[tokio::test]
async fn a_phase_with_no_rung_inherits_the_sessions_effort() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(
        provider.clone(),
        EngineConfig {
            effort: Some(Effort::Max),
            effort_schedule: Some(hotl_provider::EffortSchedule {
                plan: Some(Effort::Low),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("one request").effort,
        Some(Effort::Max)
    );
}

/// No `EffortSet` entry: the schedule is configuration, not a user decision,
/// and `hotl resume` re-derives it.
#[tokio::test]
async fn the_schedule_writes_no_effort_set_entry() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir, log_path) = session_logged(provider, scheduled_config());
    run_one_turn(&mut handle).await;
    let effort_sets = std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter(|e| matches!(e.payload, hotl_types::EntryPayload::EffortSet { .. }))
        .count();
    assert_eq!(effort_sets, 0);
}

// ---------------------------------------------------------------------------
// 0059 T1 + 0056: the plan's own state decides the `plan` phase.

fn node(content: &str, status: hotl_types::TodoStatus) -> hotl_types::Todo {
    hotl_types::Todo {
        content: content.into(),
        status,
        ..Default::default()
    }
}

/// Work left with nothing in progress is the planning moment: the last step
/// landed and the next has not been chosen.
#[tokio::test]
async fn a_plan_between_steps_opens_the_turn_at_the_plan_rung() {
    use hotl_types::TodoStatus::{Completed, InProgress, Pending};
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("a"),
        ScriptedProvider::text_reply("b"),
        ScriptedProvider::text_reply("c"),
    ]));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());

    // Between steps: one done, one still to do, none in progress.
    handle
        .set_todos(vec![node("first", Completed), node("second", Pending)])
        .await;
    run_one_turn(&mut handle).await;
    // The last request of a turn, not an index into the whole run: a turn's
    // first sample may be built twice (speculated, then rebuilt), and every
    // request inside one turn carries one rung anyway.
    assert_eq!(
        provider.last_request().expect("a request").effort,
        Some(Effort::XHigh),
        "open work and nothing in progress is planning"
    );

    // A step in progress is implementation, plan mode off.
    handle
        .set_todos(vec![node("first", Completed), node("second", InProgress)])
        .await;
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("a request").effort,
        Some(Effort::High),
        "a step in progress is implementation"
    );

    // Every node completed is finished work, not an unfinished plan.
    handle
        .set_todos(vec![node("first", Completed), node("second", Completed)])
        .await;
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("a request").effort,
        Some(Effort::High),
        "an all-completed plan is finished work, not planning"
    );
}

/// A session that never writes a plan must not sit in `plan` forever — an
/// empty list trivially has no node in progress.
#[tokio::test]
async fn an_empty_plan_is_not_a_plan() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(provider.clone(), scheduled_config());
    run_one_turn(&mut handle).await;
    assert_eq!(
        provider.last_request().expect("one request").effort,
        Some(Effort::High)
    );
}

/// A node that failed, or turned out to need splitting, is open work — and
/// precisely the kind worth thinking about rather than grinding at.
#[tokio::test]
async fn a_failed_or_oversized_node_counts_as_open_work() {
    use hotl_types::TodoStatus::{Failed, NeedsMoreSteps};
    for status in [Failed, NeedsMoreSteps] {
        let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )]));
        let (mut handle, _dir) = session(provider.clone(), scheduled_config());
        handle.set_todos(vec![node("stuck", status)]).await;
        run_one_turn(&mut handle).await;
        assert_eq!(
            provider.last_request().expect("one request").effort,
            Some(Effort::XHigh),
            "{status:?} is open work"
        );
    }
}
