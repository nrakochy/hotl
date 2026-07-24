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
use hotl_tools::{rules::Rules, AskUserTool, Registry};
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

/// Finding 2 (IMPORTANT): `Notification::Blocked` must also fire at the
/// `ask_user` structured-question surface (`hotl_engine::question_sink`),
/// not just `Turn::ask` (the permission-ask surface) — this is the dominant
/// "agent needs input" signal, the exact thing `hotl watch` exists to
/// surface. Wires `question_sink` the same way `ask_user.rs` does (a
/// pre-split command/event channel, since the sink needs live senders
/// before the actor exists), but also threads a `hooks` handle and the
/// session's own `NotificationDrain` through — the two arguments this test
/// exists to prove actually reach `question_sink` and actually fire.
#[tokio::test]
async fn ask_user_question_fires_a_blocked_notification() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "ask_user",
            json!({
                "header": "Scope", "prompt": "How far?",
                "options": [{"label": "MVP"}, {"label": "Full"}]
            }),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let (tx, mut rx) = mpsc::unbounded_channel::<(NotificationKind, String)>();
    let hooks: Arc<dyn hotl_engine::hooks::Hooks> =
        Arc::new(InProcessHooks::new().on_notification(move |kind, detail| {
            let _ = tx.send((kind, detail.to_string()));
        }));

    let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
    let (event_tx, event_rx) = hotl_engine::event_channel();
    let notifications = hotl_engine::hooks::NotificationDrain::new();
    let mut registry = Registry::builtin();
    registry.register(Box::new(AskUserTool::new(hotl_engine::question_sink(
        cmd_tx.downgrade(),
        event_tx.downgrade(),
        Some(hooks.clone()),
        notifications.clone(),
    ))));
    let mut handle = hotl_engine::spawn_session_with_channels(
        SessionDeps {
            provider,
            registry: Arc::new(registry),
            rules: Arc::new(Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "sys".into(),
            cwd: dir.path().to_path_buf(),
            snapshots: None,
            hooks: Some(hooks),
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            config,
        },
        cmd_tx,
        cmd_rx,
        event_tx,
        event_rx,
        notifications,
    );

    handle.prompt("go".into()).await;

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Question { reply, .. } => {
                let _ = reply.send(hotl_types::QuestionAnswer::Selected(vec!["MVP".into()]));
            }
            EngineEvent::TurnDone { outcome, .. } => {
                assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
                break;
            }
            _ => {}
        }
    }

    // Fire-and-forget: poll rather than assume it's already landed.
    let (kind, detail) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a Blocked notification must arrive from the ask_user surface")
        .expect("channel closed early");
    assert!(
        matches!(kind, NotificationKind::Blocked),
        "ask_user surfacing a question must notify Blocked: {kind:?}"
    );
    assert_eq!(
        detail, "Scope",
        "the detail should be the question's header"
    );
}
