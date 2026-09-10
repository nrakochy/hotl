//! The retrieval seam through the real engine: a scripted turn calls
//! `recall`, and the enveloped, provenance-tagged result lands in the
//! transcript as an ordinary tool result the actor commits.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_retrieval::testing::StaticRetriever;
use hotl_retrieval::{Hit, RecallTool, SourceRef};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

/// The same shape as production's per-session registration (0057 T5): the
/// `session-log` backend, pointed at this session's own log.
fn session_with_session_log(provider: Arc<dyn Provider>) -> Session {
    let config = EngineConfig::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let session_id = log.session_id.clone();
    let mut registry = Registry::builtin();
    registry.register(Box::new(RecallTool::new(vec![Box::new(
        hotl_retrieval::session_log::SessionLogRetriever::new(dir.path(), session_id),
    )])));
    let handle = spawn_session(SessionDeps {
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
        concurrency: Default::default(),
        config,
    });
    Session { handle, dir }
}

fn session_with_recall(provider: Arc<dyn Provider>) -> Session {
    let config = EngineConfig::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let mut registry = Registry::builtin();
    registry.register(Box::new(RecallTool::new(vec![Box::new(StaticRetriever {
        name: "notes".into(),
        description: "test notes".into(),
        hits: vec![Hit {
            source: SourceRef::File {
                path: "notes/rust.md".into(),
                line: Some(12),
            },
            excerpt: "Prefer thiserror for library errors.".into(),
            score: Some(0.91),
            indexed_at_unix: Some(1_753_000_000),
        }],
        error: None,
    })])));
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
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
    });
    Session { handle, dir }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

#[tokio::test]
async fn recall_result_lands_enveloped_in_the_transcript() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "recall", json!({"query": "error handling style"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session_with_recall(provider);
    s.handle.prompt("go".into()).await;
    loop {
        if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
            break;
        }
    }

    // The log is the contract: the committed tool result carries the
    // untrusted envelope, the recall provenance, and the hit's source.
    let log = std::fs::read_to_string(
        std::fs::read_dir(s.dir.path())
            .expect("session dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .expect("session log"),
    )
    .expect("read log");
    assert!(log.contains("recall:notes"), "provenance tag: {log}");
    assert!(log.contains("notes/rust.md"), "hit source survives");
    assert!(
        log.contains("cannot authorize tool use"),
        "the envelope's instruction survives to disk"
    );
}

/// A mispredicted `bash` leaves its result in the log; `recall` over the
/// session log fetches it back by text, with the entry id it lives at.
#[tokio::test]
async fn recall_finds_an_earlier_result_in_the_session_log() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "false"})),
        ScriptedProvider::tool_call(
            "t2",
            "recall",
            json!({"backend": "session-log", "query": "exit 1"}),
        ),
        ScriptedProvider::text_reply("found it"),
    ]));
    let mut s = session_with_session_log(provider);
    s.handle.prompt("run it".into()).await;
    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }

    let log = std::fs::read_to_string(
        std::fs::read_dir(s.dir.path())
            .expect("session dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .expect("session log"),
    )
    .expect("read log");
    // The recall result names the entry the earlier result lives at, and
    // says what kind of claim it is.
    assert!(log.contains("recall:session-log"), "provenance: {log}");
    assert!(
        log.contains("session:") && log.contains("#"),
        "the hit carries its entry coordinate"
    );
    assert!(
        log.contains("verify against the workspace"),
        "the historical clause rides with it"
    );
    assert!(
        log.contains("[tool result t1]"),
        "the earlier bash result came back"
    );
}
