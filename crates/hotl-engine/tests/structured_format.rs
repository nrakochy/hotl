//! 0073 T6 — a `prompt_structured` turn puts its schema on every sample of
//! that turn (`SamplingRequest::output_format`), the next plain prompt
//! clears it, a compaction respawn keeps it, and the request an adopted
//! speculation sent is the one the sequential rebuild would have built.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::SyntheticReason;
use serde_json::{json, Value};

struct Session {
    handle: SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<ScriptedProvider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
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

fn config() -> EngineConfig {
    EngineConfig {
        model: "claude-opus-5".into(),
        ..EngineConfig::default()
    }
}

fn schema() -> Arc<Value> {
    Arc::new(json!({
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
        "additionalProperties": false
    }))
}

/// Drain to `TurnDone`, allowing every ask.
async fn drain(s: &mut Session) -> Outcome {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => return outcome,
            _ => {}
        }
    }
}

fn read_call(
    dir: &std::path::Path,
    id: &str,
) -> Vec<Result<StreamEvent, hotl_provider::ProviderError>> {
    let file = dir.join("f.txt");
    std::fs::write(&file, "body").expect("fixture");
    ScriptedProvider::tool_call(id, "read", json!({"path": file.to_str().unwrap()}))
}

#[tokio::test]
async fn a_structured_prompt_puts_the_schema_on_every_sample_of_that_turn() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("{}"),
        read_call(dir.path(), "r1"),
        ScriptedProvider::text_reply(r#"{"name":"ok"}"#),
        ScriptedProvider::text_reply("plain again"),
    ]));
    let mut s = session(provider.clone(), config());

    s.handle.prompt("go".into()).await;
    assert!(matches!(drain(&mut s).await, Outcome::Done { .. }));
    assert_eq!(provider.request_count(), 1);
    assert!(
        provider.requests()[0].output_format.is_none(),
        "the first attempt is unconstrained"
    );

    // The retry turn: a tool call, then the answer — both samples carry it.
    s.handle
        .prompt_structured("fix it".into(), SyntheticReason::RetryFeedback, schema())
        .await;
    assert!(matches!(drain(&mut s).await, Outcome::Done { .. }));
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3, "{reqs:#?}");
    for r in &reqs[1..3] {
        assert_eq!(
            r.output_format.as_deref(),
            Some(&*schema()),
            "every sample of the retry turn"
        );
    }
    // The committed item is a tagged RetryFeedback user item, as before.
    assert!(reqs[1].items.iter().any(|i| matches!(
        &**i,
        hotl_types::Item::User {
            synthetic: Some(SyntheticReason::RetryFeedback),
            ..
        }
    )));

    // The next plain prompt clears it.
    s.handle.prompt("next".into()).await;
    assert!(matches!(drain(&mut s).await, Outcome::Done { .. }));
    assert!(
        provider.requests()[3].output_format.is_none(),
        "a new prompt starts clean"
    );
}

/// An admission-speculated request (the default `Pipelined` ack mode sends
/// the prompt's first sample before its fsync resolves) must carry the same
/// format the sequential build would — otherwise adoption would change the
/// wire. Compared against the request the provider actually saw.
#[tokio::test]
async fn an_adopted_speculative_request_carries_the_same_format() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        r#"{"name":"ok"}"#,
    )]));
    let mut s = session(provider.clone(), config());
    s.handle
        .prompt_structured("fix it".into(), SyntheticReason::RetryFeedback, schema())
        .await;
    assert!(matches!(drain(&mut s).await, Outcome::Done { .. }));
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 1, "one sample, adopted or rebuilt: {reqs:#?}");
    assert_eq!(reqs[0].output_format.as_deref(), Some(&*schema()));
}

/// A fold keeps the constraint: the history is seeded past the window so the
/// retry turn's first sample compacts first, and the sample after the fold
/// (the respawned continuation) still carries the schema. The digest call
/// goes to its own script and carries none.
#[tokio::test]
async fn a_compaction_respawn_keeps_the_format() {
    use hotl_provider::{Provider, ProviderError, SamplingRequest};
    use hotl_types::{Item, ToolResultItem};

    struct Router {
        main: Arc<ScriptedProvider>,
        summarize: Arc<ScriptedProvider>,
    }
    impl Provider for Router {
        fn stream(
            &self,
            req: SamplingRequest,
        ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>> {
            if req.system.contains("compress") {
                self.summarize.stream(req)
            } else {
                self.main.stream(req)
            }
        }
    }
    let call = |id: &str| Item::Assistant {
        blocks: vec![
            json!({"type": "tool_use", "id": id, "name": "bash", "input": {"command": "ls"}}),
        ],
        model: None,
    };
    let result = |id: &str, content: String| Item::ToolResults {
        results: vec![ToolResultItem {
            tool_use_id: id.into(),
            content,
            is_error: false,
        }],
    };
    let seeded = vec![
        Item::User {
            text: "first".into(),
            synthetic: None,
            images: Vec::new(),
        },
        call("t1"),
        result("t1", "y".repeat(2_600)),
        Item::Assistant {
            blocks: vec![json!({"type": "text", "text": "listed"})],
            model: None,
        },
    ];
    let main = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        r#"{"name":"ok"}"#,
    )]));
    // Two: the speculative digest and the inline fold both want a script.
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("GOAL: keep going"),
        ScriptedProvider::text_reply("GOAL: keep going"),
    ]));
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig {
        context_window: 1_000,
        keep_results_turns: 0,
        ..config()
    };
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: Arc::new(Router {
            main: Arc::clone(&main),
            summarize: Arc::clone(&summarize),
        }),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items: seeded,
        initial_todos: Vec::new(),
        initial_goal: None,
        initial_decisions: Vec::new(),
        plan_files: None,
        config,
    });
    let mut s = Session { handle, dir };
    s.handle
        .prompt_structured("fix it".into(), SyntheticReason::RetryFeedback, schema())
        .await;
    let mut compacted = false;
    let outcome = loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Compacted { .. } => compacted = true,
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(
        compacted,
        "the seeded history must fold on the first sample"
    );
    let reqs = main.requests();
    assert_eq!(reqs.len(), 1, "{reqs:#?}");
    assert_eq!(
        reqs[0].output_format.as_deref(),
        Some(&*schema()),
        "the continuation after the fold still carries the schema"
    );
    assert!(
        summarize
            .requests()
            .iter()
            .all(|r| r.output_format.is_none()),
        "the digest call is not a sample of this turn"
    );
}
