use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskOrigin, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<ScriptedProvider>) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig {
        max_turns: 20,
        ..EngineConfig::default()
    };
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
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
        initial_goal: None,
        initial_decisions: Vec::new(),
        plan_files: None,
        config,
    });
    Session { handle, dir }
}

async fn asks_until_done(s: &mut Session) -> Vec<(String, AskOrigin)> {
    let mut asks = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask {
                call_id,
                origin,
                reply,
                ..
            } => {
                asks.push((call_id, origin));
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => return asks,
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_tool_ask_names_the_tool_as_its_origin_whatever_its_id_looks_like() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("loop-7", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider);
    s.handle.prompt("go".into()).await;
    assert_eq!(
        asks_until_done(&mut s).await,
        vec![("loop-7".to_string(), AskOrigin::Tool)]
    );
}

#[tokio::test]
async fn the_doom_guard_ask_names_the_loop_as_its_origin() {
    let repeat = |i: usize| {
        ScriptedProvider::tool_call(&format!("t{i}"), "bash", json!({"command": "echo same"}))
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        repeat(0),
        repeat(1),
        repeat(2),
        repeat(3),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider);
    s.handle.prompt("loop".into()).await;
    let asks = asks_until_done(&mut s).await;
    let loops: Vec<_> = asks
        .iter()
        .filter(|(_, origin)| *origin == AskOrigin::Loop)
        .collect();
    assert_eq!(loops.len(), 1, "{asks:?}");
    assert!(
        asks.iter()
            .filter(|(_, origin)| *origin != AskOrigin::Loop)
            .all(|(_, origin)| *origin == AskOrigin::Tool),
        "{asks:?}"
    );
}
