//! commit-protocol.md's test matrix, cases 4 and 5, at the engine level.

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
