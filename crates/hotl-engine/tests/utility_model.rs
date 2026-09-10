//! 0059 T2 — the utility-model role. Compaction digests and goal evaluations
//! are calls the session pays for and never shows; both take one model, and
//! the resolution (`utility_model` → `fast_model` → the session model) lives
//! in one place so the two can never drift apart.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, RetryScope, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

fn session(
    provider: Arc<ScriptedProvider>,
    config: EngineConfig,
    dir: &std::path::Path,
) -> SessionHandle {
    let log = SessionLog::create(dir, &config.model, None, Masker::empty(), 0).expect("log");
    spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        initial_decisions: Vec::new(),
        plan_files: None,
        config,
    })
}

async fn drain_to_turn_done(handle: &mut SessionHandle) {
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
}

/// The one-off calls, identified by the system prompt each one carries.
fn one_off_models(requests: &[SamplingRequest]) -> (Vec<String>, Vec<String>) {
    let pick = |needle: &str| -> Vec<String> {
        requests
            .iter()
            .filter(|r| r.system.contains(needle))
            .map(|r| r.model.clone())
            .collect()
    };
    (pick("compress"), pick("You judge whether"))
}

fn utility_config(utility: Option<&str>, fast: Option<&str>) -> EngineConfig {
    EngineConfig {
        model: "session-model".into(),
        utility_model: utility.map(str::to_string),
        fast_model: fast.map(str::to_string),
        context_window: 1000,
        max_turns: 6,
        ..Default::default()
    }
}

/// A fixture that both compacts and evaluates a goal inside one turn.
fn scripts(path: &str) -> Vec<Vec<Result<StreamEvent, ProviderError>>> {
    let mut scripts = Vec::new();
    for i in 0..6 {
        let mut script =
            ScriptedProvider::tool_call(&format!("t{i}"), "read", json!({"path": path}));
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.input_tokens = if i == 0 { 650 } else { 850 };
        }
        scripts.push(script);
    }
    scripts
}

async fn run(utility: Option<&str>, fast: Option<&str>) -> (Vec<String>, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    // The digest and the verdict are served from the same queue as the main
    // samples — this asserts on the model each request carried, not on which
    // provider answered.
    let mut all = scripts(&path);
    all.push(ScriptedProvider::text_reply("DIGEST"));
    all.push(ScriptedProvider::text_reply("done"));
    all.push(ScriptedProvider::text_reply(
        "VERDICT: met\nREASON: the work is done",
    ));
    let provider = Arc::new(ScriptedProvider::new(all));
    let mut handle = session(
        Arc::clone(&provider),
        utility_config(utility, fast),
        dir.path(),
    );
    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;
    drain_to_turn_done(&mut handle).await;
    one_off_models(&provider.requests())
}

#[tokio::test]
async fn summarize_and_goal_eval_take_the_utility_model() {
    let (digests, verdicts) = run(Some("utility-1"), Some("fast-1")).await;
    assert!(!digests.is_empty(), "the fixture must compact");
    assert!(!verdicts.is_empty(), "the fixture must evaluate the goal");
    assert!(
        digests.iter().all(|m| m == "utility-1"),
        "compaction: {digests:?}"
    );
    assert!(
        verdicts.iter().all(|m| m == "utility-1"),
        "goal eval: {verdicts:?}"
    );
}

#[tokio::test]
async fn an_unset_utility_model_falls_through_to_fast_then_to_the_session() {
    let (digests, verdicts) = run(None, Some("fast-1")).await;
    assert!(digests.iter().all(|m| m == "fast-1"), "{digests:?}");
    assert!(verdicts.iter().all(|m| m == "fast-1"), "{verdicts:?}");

    let (digests, verdicts) = run(None, None).await;
    assert!(digests.iter().all(|m| m == "session-model"), "{digests:?}");
    assert!(
        verdicts.iter().all(|m| m == "session-model"),
        "{verdicts:?}"
    );
}

/// Routes by the system prompt each one-off carries, so a scripted backoff
/// lands on the call it was written for rather than on whichever sample
/// happened to be next in the queue.
struct ByRole {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    goal_eval: Arc<ScriptedProvider>,
}

impl hotl_provider::Provider for ByRole {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let inner = if req.system.contains("compress") {
            Arc::clone(&self.summarize)
        } else if req.system.contains("You judge whether") {
            Arc::clone(&self.goal_eval)
        } else {
            Arc::clone(&self.main)
        };
        inner.stream(req)
    }
}

/// 0061 T16: the utility model has its own retry ladder, and a human waiting
/// through it used to see nothing at all — the `Ok(_) => {}` arm swallowed
/// every `Retrying` the digest and the evaluator produced.
async fn run_collecting_retries(retry_digest: bool, retry_verdict: bool) -> Vec<RetryScope> {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    let backoff = || {
        Ok(StreamEvent::Retrying {
            attempt: 1,
            max: 5,
            reason: "HTTP 429: slow down".into(),
            delay_ms: 0,
            status: Some(429),
        })
    };
    let mut main = scripts(&path);
    main.push(ScriptedProvider::text_reply("done"));
    // Two: the speculative digest and the inline fold both draw from here.
    let mut summarize = Vec::new();
    for _ in 0..2 {
        let mut digest = ScriptedProvider::text_reply("DIGEST");
        if retry_digest {
            digest.insert(0, backoff());
        }
        summarize.push(digest);
    }
    let mut verdict = ScriptedProvider::text_reply("VERDICT: met\nREASON: the work is done");
    if retry_verdict {
        verdict.insert(0, backoff());
    }
    let provider = Arc::new(ByRole {
        main: Arc::new(ScriptedProvider::new(main)),
        summarize: Arc::new(ScriptedProvider::new(summarize)),
        goal_eval: Arc::new(ScriptedProvider::new(vec![verdict])),
    });
    // Room for the seven main samples: the turn has to *finish* for the goal
    // gate to run at all.
    let config = EngineConfig {
        max_turns: 12,
        ..utility_config(Some("utility-1"), None)
    };
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let mut handle = spawn_session(SessionDeps {
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
    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;

    let mut scopes = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::Retrying { scope, .. } => scopes.push(scope),
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    scopes
}

#[tokio::test]
async fn a_summarize_retry_is_forwarded_with_scope_summarize() {
    let scopes = run_collecting_retries(true, false).await;
    assert!(
        scopes.contains(&RetryScope::Summarize),
        "the fold's own ladder was swallowed: {scopes:?}"
    );
    assert!(!scopes.contains(&RetryScope::GoalEval), "{scopes:?}");
}

#[tokio::test]
async fn a_goal_eval_retry_is_forwarded_with_scope_goal_eval() {
    let scopes = run_collecting_retries(false, true).await;
    assert!(
        scopes.contains(&RetryScope::GoalEval),
        "the evaluator's ladder was swallowed: {scopes:?}"
    );
    assert!(!scopes.contains(&RetryScope::Summarize), "{scopes:?}");
}

/// A quiet baseline: nothing to forward, nothing forwarded.
#[tokio::test]
async fn a_clean_utility_call_forwards_nothing() {
    assert!(run_collecting_retries(false, false).await.is_empty());
}
