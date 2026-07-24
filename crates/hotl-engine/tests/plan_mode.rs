//! Plan mode through the real engine: a scripted turn calls `write` while
//! `Rules` is in `Plan` mode — the gate must consult `Tool::read_only()` and
//! deny the call with a plan-mode reason, without touching the filesystem.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_retrieval::{Hit, Query, RecallTool, Retriever, SourceRef};
use hotl_store::{Masker, SessionLog};
use hotl_tools::rules::{PermissionMode, Rules};
use hotl_tools::{Permission, Registry};
use hotl_types::{EntryPayload, Item};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

#[tokio::test]
async fn plan_mode_denies_write_tool_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": "should-not-exist.txt", "content": "nope"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default().with_mode(PermissionMode::Plan)),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });
    let mut s = Session { handle, dir };
    s.handle.prompt("go".into()).await;

    let mut saw_denied_write = false;
    loop {
        if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
            break;
        }
    }
    drop(s.handle); // flush is synchronous on append; nothing to await here

    // The log is the contract: the tool result for the write must be an
    // error mentioning plan mode, and the file must never have been created.
    let log_path = std::fs::read_dir(s.dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    let contents = std::fs::read_to_string(&log_path).expect("read log");
    for line in contents.lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("entry");
        if let EntryPayload::Item {
            item: Item::ToolResults { results },
        } = entry.payload
        {
            for r in results {
                if r.tool_use_id == "t1" {
                    assert!(r.is_error, "write must be denied in plan mode");
                    assert!(
                        r.content.contains("plan mode"),
                        "denial reason: {}",
                        r.content
                    );
                    saw_denied_write = true;
                }
            }
        }
    }
    assert!(saw_denied_write, "no tool result found for t1");
    assert!(
        !s.dir.path().join("should-not-exist.txt").exists(),
        "plan mode must never execute the write"
    );
}

/// A backend whose permission still asks (not `Permission::None`) but is
/// structurally read-only: `recall` never mutates. Plan mode must not
/// plan-block it — only *classify* it via `Tool::read_only()`, exactly what
/// the gate is supposed to consult.
struct AskingRetriever {
    hits: Vec<Hit>,
}

impl Retriever for AskingRetriever {
    fn name(&self) -> &str {
        "notes"
    }
    fn description(&self) -> &str {
        "test notes"
    }
    fn permission(&self, query: &str) -> Permission {
        Permission::Ask {
            summary: format!("recall: notes \"{query}\""),
        }
    }
    fn search<'a>(
        &'a self,
        _query: &'a Query,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        let hits = self.hits.clone();
        Box::pin(async move { Ok(hits) })
    }
}

#[tokio::test]
async fn plan_mode_does_not_block_a_read_only_tool_that_still_asks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let mut registry = Registry::builtin();
    registry.register(Box::new(RecallTool::new(vec![Box::new(AskingRetriever {
        hits: vec![Hit {
            source: SourceRef::File {
                path: "notes/rust.md".into(),
                line: Some(12),
            },
            excerpt: "Prefer thiserror for library errors.".into(),
            score: None,
            indexed_at_unix: None,
        }],
    })])));
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "recall", json!({"query": "error handling style"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(registry),
        rules: Arc::new(Rules::default().with_mode(PermissionMode::Plan)),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });
    let mut s = Session { handle, dir };
    s.handle.prompt("go".into()).await;

    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }

    let log_path = std::fs::read_dir(s.dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    let contents = std::fs::read_to_string(&log_path).expect("read log");
    let mut saw_recall_result = false;
    for line in contents.lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("entry");
        if let EntryPayload::Item {
            item: Item::ToolResults { results },
        } = entry.payload
        {
            for r in results {
                if r.tool_use_id == "t1" {
                    assert!(
                        !r.content.contains("plan mode"),
                        "a read-only tool must not be plan-blocked: {}",
                        r.content
                    );
                    assert!(!r.is_error, "recall must succeed: {}", r.content);
                    saw_recall_result = true;
                }
            }
        }
    }
    assert!(saw_recall_result, "no tool result found for t1");
}
