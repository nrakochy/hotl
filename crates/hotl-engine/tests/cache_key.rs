//! 0045: every sample a session composes carries the session id as
//! `SamplingRequest::cache_key` (D1) — the routing key the OpenAI dialects
//! send as `prompt_cache_key`, so one session's samples land on one shard.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

#[tokio::test]
async fn every_sample_in_a_turn_carries_the_session_id_as_cache_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let session_id = log.session_id.clone();
    // A tool call then a reply: two samples in one turn, so the key is shown
    // stable across the continuation, not just present once.
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("done"),
    ]));
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
        initial_todos: Vec::new(),
        initial_goal: None,
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
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }

    let seen = provider.requests();
    assert!(seen.len() >= 2, "the turn should have sampled twice");
    for (i, req) in seen.iter().enumerate() {
        assert_eq!(
            req.cache_key.as_deref(),
            Some(session_id.as_str()),
            "sample {i} must carry the session id as its cache key"
        );
    }
}
