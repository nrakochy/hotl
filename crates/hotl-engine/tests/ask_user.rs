//! `ask_user` end-to-end through the real turn loop (tier-1 gap #4): the
//! tool proposes a durable `PendingQuestion`, emits `EngineEvent::Question`,
//! and parks on the reply; a dropped reply (headless/no-human) must resolve
//! to the documented `NoHuman` guidance rather than hang the turn.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{EngineConfig, EngineEvent, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, AskUserTool, Registry};
use hotl_types::{Entry, EntryPayload, Item, QuestionAnswer};
use serde_json::json;

/// Build a session wired with `ask_user`, its sink reaching *this* session's
/// own actor via pre-split cmd/event channels — same "reach the actor before
/// it exists" shape `spawn_session_with_todos` uses for `todo_write`.
fn spawn_with_ask_user(
    provider: Arc<ScriptedProvider>,
    log: SessionLog,
    cwd: &std::path::Path,
) -> hotl_engine::SessionHandle {
    let config = EngineConfig::default();
    let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
    let (event_tx, event_rx) = hotl_engine::event_channel();
    let mut registry = Registry::builtin();
    registry.register(Box::new(AskUserTool::new(hotl_engine::question_sink(
        cmd_tx.downgrade(),
        event_tx.downgrade(),
    ))));
    hotl_engine::spawn_session_with_channels(
        SessionDeps {
            provider,
            registry: Arc::new(registry),
            rules: Arc::new(Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "sys".into(),
            cwd: cwd.to_path_buf(),
            snapshots: None,
            hooks: None,
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            config,
        },
        cmd_tx,
        cmd_rx,
        event_tx,
        event_rx,
    )
}

fn logged_payloads(path: &std::path::Path) -> Vec<EntryPayload> {
    std::fs::read_to_string(path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .map(|e| e.payload)
        .collect()
}

#[tokio::test]
async fn ask_user_resolves_from_a_selected_answer_and_logs_the_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
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
    let mut handle = spawn_with_ask_user(provider, log, dir.path());

    handle.prompt("go".into()).await;

    let mut answered = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Question { reply, .. } => {
                answered = true;
                let _ = reply.send(QuestionAnswer::Selected(vec!["MVP".into()]));
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(answered, "the engine must surface a Question event");

    let replayed = hotl_store::replay(&log_path).expect("replay");
    let saw_result = replayed.items.iter().any(|i| {
        matches!(i, Item::ToolResults { results } if results.iter().any(|r| !r.is_error && r.content.contains("MVP")))
    });
    assert!(saw_result, "the tool result must carry the selected label");

    let entries = logged_payloads(&log_path);
    assert!(
        entries
            .iter()
            .any(|p| matches!(p, EntryPayload::PendingQuestion { .. })),
        "a pending_question must be committed before surfacing"
    );
    assert!(
        entries
            .iter()
            .any(|p| matches!(p, EntryPayload::QuestionResolved { answer, .. } if answer == "MVP")),
        "the resolution must be committed after the human answers"
    );
}

#[tokio::test]
async fn ask_user_resolves_to_no_human_when_the_reply_channel_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
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
    let mut handle = spawn_with_ask_user(provider, log, dir.path());

    handle.prompt("go".into()).await;

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout — a dropped reply must resolve, not hang")
            .expect("event channel closed");
        match ev {
            // Nobody answers: drop the reply sender (headless/no-human).
            EngineEvent::Question { reply, .. } => drop(reply),
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }

    let replayed = hotl_store::replay(&log_path).expect("replay");
    let saw_guidance = replayed.items.iter().any(|i| {
        matches!(i, Item::ToolResults { results } if results.iter().any(|r| !r.is_error && r.content.contains("No human is available")))
    });
    assert!(
        saw_guidance,
        "a dropped reply must resolve to the documented NoHuman guidance, not hang the turn"
    );

    let entries = logged_payloads(&log_path);
    assert!(entries.iter().any(
        |p| matches!(p, EntryPayload::QuestionResolved { answer, .. } if answer.contains("No human is available"))
    ));
}
