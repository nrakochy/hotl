//! 0059 T2 — the utility-model role. Compaction digests and goal evaluations
//! are calls the session pays for and never shows; both take one model, and
//! the resolution (`utility_model` → `fast_model` → the session model) lives
//! in one place so the two can never drift apart.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
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
