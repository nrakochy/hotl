//! `Notification` (tier-1 gap #7, the `hotl watch`/desktop seam): the engine
//! tells hooks when the agent blocks on a human (`Blocked`), completes a turn
//! (`Done`), and goes idle awaiting the next prompt (`Idle`) — fire-and-forget,
//! never awaited on the turn's hot path.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::hooks::InProcessHooks;
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, NotificationKind, Outcome, SessionDeps,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test]
async fn notification_hook_sees_blocked_then_done_then_idle_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    // `write` always needs an ask under the default (Ask-mode) rules — the
    // permission surfacing that must fire `Blocked` before it reaches the
    // human.
    let write_path = dir.path().join("x.txt");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": write_path.to_str().unwrap(), "content": "hi"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let (tx, mut rx) = mpsc::unbounded_channel::<(NotificationKind, String)>();
    let hooks = InProcessHooks::new().on_notification(move |kind, detail| {
        let _ = tx.send((kind, detail.to_string()));
    });
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: Some(Arc::new(hooks)),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });

    handle.prompt("go".into()).await;

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => {
                assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
                break;
            }
            _ => {}
        }
    }

    // Fire-and-forget: the notifications land on their own detached tasks,
    // so poll the channel rather than assume they're already there.
    let mut seen = Vec::new();
    for _ in 0..3 {
        let (kind, detail) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a notification must arrive")
            .expect("channel closed early");
        seen.push((kind, detail));
    }
    assert!(
        matches!(seen[0].0, NotificationKind::Blocked),
        "the permission ask must notify Blocked first: {:?}",
        seen
    );
    assert!(
        matches!(seen[1].0, NotificationKind::Done),
        "turn completion must notify Done second: {:?}",
        seen
    );
    assert!(seen[1].1.contains("done"), "{:?}", seen[1]);
    assert!(
        matches!(seen[2].0, NotificationKind::Idle),
        "an empty queue must notify Idle last: {:?}",
        seen
    );
}

#[tokio::test]
async fn a_slow_notification_hook_never_stalls_turn_done() {
    // A notifier that would take far longer than any reasonable turn budget
    // must not be awaited on the hot path — `TurnDone` must still arrive
    // promptly.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "done",
    )]));
    let hooks = InProcessHooks::new().on_notification(|_kind, _detail| {
        std::thread::sleep(Duration::from_millis(50));
    });
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: Some(Arc::new(hooks)),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });

    handle.prompt("go".into()).await;
    let start = std::time::Instant::now();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .expect("TurnDone must not be stalled by a slow notification hook")
            .expect("event channel closed");
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "TurnDone must not wait on the notification hook: {:?}",
        start.elapsed()
    );
}
