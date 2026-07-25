//! T2-1: with a todo list active the snapshot carries an ephemeral reminder
//! as its last item. Anchoring on snapshot length rather than durable length
//! makes the next estimate skip the tool results entirely, so the compaction
//! trigger never fires in the default (todo-driven) workflow.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Todo, TodoStatus};
use serde_json::json;

/// Routes summarize requests (identified by the compaction system prompt) to
/// their own script; everything else goes to the scripted main provider.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
}

impl Provider for Router {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        if req.system.contains("compress") {
            self.summarize.stream(req)
        } else {
            self.main.stream(req)
        }
    }
}

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
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
    Session { handle, dir }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// A one-tool-call sample whose Completed reports a chosen input_tokens — the
/// anchor (A12b) the engine's context estimate builds on.
fn tool_call_reporting(
    id: &str,
    name: &str,
    input: serde_json::Value,
    input_tokens: u64,
) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut script = ScriptedProvider::tool_call(id, name, input);
    if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
        usage.input_tokens = input_tokens;
    }
    script
}

#[tokio::test]
async fn compaction_still_triggers_with_todos_active() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "DIGEST",
    )]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize,
    });
    let mut s = session(
        provider,
        EngineConfig {
            context_window: 1000,
            max_turns: 10,
            ..Default::default()
        },
    );

    // A COMPLETED todo: the reminder is rendered (the list is non-empty), so
    // the ephemeral item is present, but the TodoGate never fires and cannot
    // add samples the script doesn't have.
    s.handle
        .set_todos(vec![Todo {
            content: "already done".into(),
            status: TodoStatus::Completed,
            active_form: None,
        }])
        .await;

    // A ~1200-byte tool result ≈ 400 estimated tokens (3 chars/token). The
    // anchor reports 500, so only counting the results pushes the next request
    // past COMPACT_TRIGGER (800 of a 1000-token window).
    let file = s.dir.path().join("big.txt");
    std::fs::write(&file, "y".repeat(1_200)).expect("fixture");
    let path = file.to_str().expect("utf8 path");
    main.push_script(tool_call_reporting(
        "t1",
        "read",
        json!({ "path": path }),
        500,
    ));
    main.push_script(ScriptedProvider::text_reply("done after compaction"));

    s.handle.prompt("read the big file".into()).await;
    let mut compacted = false;
    let outcome = loop {
        match next_event(&mut s).await {
            EngineEvent::Compacted { .. } => compacted = true,
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(compacted, "the tool result must count toward the estimate");
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done after compaction".into()
        }
    );
}
