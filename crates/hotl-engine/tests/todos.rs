//! `SessionCmd::SetTodos` appends a durable `todos` entry (mirrors
//! `rename.rs`/`set_mode.rs`) and emits `TodosChanged`. The list itself is
//! ephemeral session context: it rides the snapshot a turn samples against
//! (like the MOIM turn-context block) but never lands in the durable
//! projection replay reconstructs.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, SyntheticReason, Todo, TodoStatus};

fn todo(content: &str, status: TodoStatus) -> Todo {
    Todo {
        content: content.into(),
        status,
        active_form: None,
    }
}

#[tokio::test]
async fn set_todos_appends_a_durable_entry_and_emits_todos_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let mut handle = spawn_session(SessionDeps {
        provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )])),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        config,
    });

    let sent = vec![
        todo("wire the gate", TodoStatus::InProgress),
        todo("write docs", TodoStatus::Pending),
    ];
    handle.set_todos(sent.clone()).await;

    // Drain events until TodosChanged shows up (SetTodos is processed FIFO
    // by the actor, ahead of anything sent after it).
    let mut saw_changed = None;
    for _ in 0..8 {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TodosChanged { items } = ev {
            saw_changed = Some(items);
            break;
        }
    }
    assert_eq!(saw_changed, Some(sent.clone()));

    // (a)+(reload): the durable log carries exactly the one `Todos` entry,
    // last-wins shape (log-only, like `Rename`/`ModeSet`).
    let logged: Vec<Vec<Todo>> = std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::Todos { items } => Some(items),
            _ => None,
        })
        .collect();
    assert_eq!(logged, vec![sent]);
}

#[tokio::test]
async fn the_todo_reminder_rides_the_snapshot_but_never_the_durable_projection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let mut handle = spawn_session(SessionDeps {
        provider: provider.clone(),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        config,
    });

    handle
        .set_todos(vec![todo("wire the gate", TodoStatus::InProgress)])
        .await;
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }

    // The model saw the reminder as the last item of its request...
    let request = provider.last_request().expect("one request");
    let last = request.items.last().expect("non-empty snapshot");
    match last {
        Item::User { text, synthetic } => {
            assert_eq!(*synthetic, Some(SyntheticReason::Todos));
            assert!(text.contains("[~] wire the gate"), "text: {text}");
        }
        other => panic!("expected the todo reminder last, got {other:?}"),
    }

    // ...but replay's projection (what the durable log actually holds)
    // carries no such item — the reminder never got committed.
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert!(
        !replayed.items.iter().any(|i| matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::Todos),
                ..
            }
        )),
        "the todo reminder must never land in the durable projection"
    );
}
