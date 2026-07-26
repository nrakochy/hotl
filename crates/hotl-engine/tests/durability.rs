//! commit-protocol.md's test matrix, cases 4, 5 and 8, at the engine level.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog, WriteFault};
use hotl_tools::{rules::Rules, Registry};

fn deps(dir: &std::path::Path, log: SessionLog, config: EngineConfig) -> SessionDeps {
    SessionDeps {
        provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )])),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    }
}

/// A permission-free, read-only call that stays running long enough for a
/// test to arm the writer fault while the turn is between commits — the
/// window case 8 needs, without a `bash` ask committing an entry into it.
struct SlowTool;

impl hotl_tools::Tool for SlowTool {
    fn name(&self) -> &'static str {
        "dawdle"
    }
    fn description(&self) -> &str {
        "waits, then answers"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
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
            tokio::time::sleep(Duration::from_millis(300)).await;
            hotl_tools::ToolOutcome::ok("waited")
        })
    }
}

async fn next_turn_done(handle: &mut hotl_engine::SessionHandle) -> Outcome {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { outcome, .. } = ev {
            return outcome;
        }
    }
}

/// Matrix case 5: "Disk-full → log-sealed state, clean error surface, no torn
/// entries."
#[tokio::test]
async fn disk_full_seals_the_session_with_a_clean_error_and_no_torn_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    log.inject_fault(WriteFault::TearThenFail);
    let mut handle = spawn_session(deps(dir.path(), log, config));

    handle.prompt("go".into()).await;
    let outcome = next_turn_done(&mut handle).await;
    match outcome {
        Outcome::Error { message } => {
            assert!(
                message.contains("sealed"),
                "user-facing error must say so: {message}"
            )
        }
        other => panic!("a sealed log must end the turn with an error, got {other:?}"),
    }

    // No torn entries reached the user's disk, and the log still replays.
    let content = std::fs::read_to_string(&log_path).expect("read log");
    for line in content.lines() {
        serde_json::from_str::<hotl_types::Entry>(line).expect("whole line");
    }
    let replayed = hotl_store::replay(&log_path).expect("a sealed log still replays");
    assert!(replayed.warnings.is_empty(), "{:?}", replayed.warnings);
    handle.finish(Duration::from_millis(200)).await;
}

/// Matrix case 4: "Kill -9 between writer receive and fsync → replay shows the
/// projection never advanced (no divergence)." The writer takes the line and
/// dies before fsync, so the ack never arrives; the actor must not project an
/// entry it was never told was durable.
#[tokio::test]
async fn a_writer_death_before_fsync_never_leaves_the_projection_ahead_of_the_log() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    log.inject_fault(WriteFault::DropAckBeforeFsync);
    let mut handle = spawn_session(deps(dir.path(), log, config));

    handle.prompt("go".into()).await;
    match next_turn_done(&mut handle).await {
        Outcome::Error { message } => assert!(message.contains("sealed"), "{message}"),
        other => panic!("an unacked commit must not report success, got {other:?}"),
    }

    // The safe direction only: the log may hold bytes the projection never
    // took (recovery replays them), but the projection must never hold an item
    // the log does not. Nothing further is committed, so the on-disk log is a
    // superset of what the session ever projected.
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert!(replayed.warnings.is_empty(), "{:?}", replayed.warnings);
    handle.finish(Duration::from_millis(200)).await;
}

/// Matrix case 8: "Kill -9 between enqueue and sync, with tickets
/// outstanding → replay shows the projection never advanced past the last
/// acked entry; no ticket was resolved for bytes that are not on disk."
///
/// The tool-results commit is the pipelined one (see `Turn::run_tool_batch`),
/// so arming the fault while the tool is still running puts the writer death
/// exactly between a ticket being issued and its bytes being synced.
///
/// It is also the sealed-log-during-speculation case: that same boundary
/// optimistically dispatches the next sample, so the failing ticket has to
/// cancel an un-adopted stream — and adoption must never be reached, since
/// the refresh sits behind the drain that surfaced the seal.
#[tokio::test]
async fn a_writer_death_before_fsync_never_resolves_a_pipelined_ticket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let mut registry = Registry::builtin();
    registry.register(Box::new(SlowTool));
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "dawdle", serde_json::json!({})),
        ScriptedProvider::text_reply("never reached"),
    ]));
    // Armed later, once the tool is running: the log itself moves into the
    // session, so the arm has to be taken while it is still in hand.
    let injector = log.fault_injector();
    let mut deps = deps(dir.path(), log, config);
    deps.registry = Arc::new(registry);
    deps.provider = provider.clone();
    let mut handle = spawn_session(deps);

    handle.prompt("go".into()).await;
    // Arm it while the tool is still dawdling: the next append is the
    // pipelined tool-results commit.
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::ToolStart { .. } = ev {
            injector(WriteFault::DropAckBeforeFsync);
            break;
        }
    }
    match next_turn_done(&mut handle).await {
        Outcome::Error { message } => assert!(message.contains("sealed"), "{message}"),
        other => panic!("an unresolved ticket must not report success, got {other:?}"),
    }

    // The turn *dispatched* the next sample optimistically at the boundary
    // (commit-protocol.md §Causal groups (b): speculation runs ahead of
    // durability) — and then the ticket came back sealed, so that stream was
    // cancelled un-adopted. A cancelled speculative stream is not a turn
    // outcome and produces no entry: the log assertions below are what prove
    // nothing from it survived.
    assert_eq!(
        provider.requests().len(),
        2,
        "one sample, plus the optimistic dispatch the sealed ticket then cancelled"
    );
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert!(replayed.warnings.is_empty(), "{:?}", replayed.warnings);
    // Nothing was ever chained onto the entry the writer died on: the
    // projection never advanced past the last acked entry.
    let lines: Vec<String> = std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .map(String::from)
        .collect();
    assert!(
        lines.last().expect("a log").contains("tool_results"),
        "the last line is the un-acked commit, with nothing after it: {lines:#?}"
    );
    handle.finish(Duration::from_millis(200)).await;
}
