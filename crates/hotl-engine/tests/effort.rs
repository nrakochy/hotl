//! 0029: `EngineConfig::effort` must reach the composed `SamplingRequest`, and
//! the compaction summarize must never inherit it — a `max`-effort session
//! folding its own history at `max` rates is a cost surprise nobody opted into.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{
    Effort, Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent,
};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

/// Routes summarize requests (identified by the compaction system prompt) to
/// their own script, so each side's request can be inspected separately.
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

fn session(
    provider: Arc<dyn Provider>,
    config: EngineConfig,
) -> (SessionHandle, tempfile::TempDir) {
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
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });
    (handle, dir)
}

async fn run_one_turn(handle: &mut SessionHandle) {
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_configured_effort_reaches_the_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(
        provider.clone(),
        EngineConfig {
            effort: Some(Effort::High),
            ..Default::default()
        },
    );
    run_one_turn(&mut handle).await;
    let req = provider.last_request().expect("one request");
    assert_eq!(req.effort, Some(Effort::High));
}

#[tokio::test]
async fn an_unconfigured_session_sends_no_effort() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let (mut handle, _dir) = session(provider.clone(), EngineConfig::default());
    run_one_turn(&mut handle).await;
    assert_eq!(provider.last_request().expect("one request").effort, None);
}

/// Ledger 12: compaction is pinned to no effort regardless of the session's.
///
/// The fold happens mid-turn, so the fixture needs tool calls to reach a
/// second step — a plain text reply ends the turn before the trigger is read.
#[tokio::test]
async fn compaction_never_inherits_the_sessions_effort() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "DIGEST",
    )]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
    });
    let (mut handle, dir) = session(
        provider,
        EngineConfig {
            effort: Some(Effort::Max),
            context_window: 1000,
            max_turns: 4,
            ..Default::default()
        },
    );
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    for i in 0..10 {
        let mut script =
            ScriptedProvider::tool_call(&format!("t{i}"), "read", json!({"path": path}));
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.input_tokens = if i == 0 { 650 } else { 850 };
        }
        main.push_script(script);
    }
    run_one_turn(&mut handle).await;
    let folded = summarize.last_request().expect("the fixture must compact");
    assert_eq!(
        folded.effort, None,
        "compaction must not pay the session's rate"
    );
    assert_eq!(
        main.last_request().expect("a main request").effort,
        Some(Effort::Max)
    );
}
