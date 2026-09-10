//! The context ladder's cheapest rung (0057 T1) through the real engine: at
//! 60% of the window old tool results become `<cleared/>` stubs, in one
//! entry, and no digest is paid for.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{assistant_text, Item};
use serde_json::json;

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
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        concurrency: Default::default(),
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

/// Drive one prompt to its outcome, reporting whether the ladder fired.
async fn run(s: &mut Session, prompt: &str) -> (Outcome, usize, bool) {
    let (outcome, cleared, compacted, _) = run_reporting_usage(s, prompt).await;
    (outcome, cleared, compacted)
}

/// The same, plus the prompt's total reported spend — `turn_done.usage` is
/// cumulative over the whole prompt, respawns included.
async fn run_reporting_usage(
    s: &mut Session,
    prompt: &str,
) -> (Outcome, usize, bool, hotl_types::TokenUsage) {
    s.handle.prompt(prompt.into()).await;
    let mut cleared = 0;
    let mut compacted = false;
    loop {
        match next_event(s).await {
            EngineEvent::Cleared { count } => cleared += count,
            EngineEvent::Compacted { .. } => compacted = true,
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, usage, .. } => {
                return (outcome, cleared, compacted, usage)
            }
            _ => {}
        }
    }
}

/// A window small enough that one big `read` result crosses 60% but not 80%:
/// clearing must fire, and the fold must not.
fn config() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        context_window: 1_000,
        // Only the current turn is protected, so the second prompt can clear
        // the first one's result. Production keeps four.
        keep_results_turns: 1,
        max_turns: 10,
        ..Default::default()
    }
}

#[tokio::test]
async fn old_results_are_cleared_to_stubs_before_any_fold() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, config());
    let file = s.dir.path().join("big.txt");
    // ~2100 ASCII chars ≈ 700 estimated tokens: past 60% of 1000, under 80%.
    std::fs::write(&file, "y".repeat(2_100)).expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();

    provider.push_script(ScriptedProvider::tool_call(
        "t1",
        "read",
        json!({ "path": path }),
    ));
    provider.push_script(ScriptedProvider::text_reply("read it"));
    let (outcome, cleared, compacted) = run(&mut s, "read the big file").await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "read it".into()
        }
    );
    assert_eq!(cleared, 0, "one turn in, nothing is old enough");
    assert!(!compacted);

    let before = provider.requests().len();
    provider.push_script(ScriptedProvider::text_reply("second"));
    let (outcome, cleared, compacted) = run(&mut s, "now something else").await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "second".into()
        }
    );
    assert_eq!(cleared, 1, "the first turn's read result is cleared");
    assert!(!compacted, "the cheap rung fires instead of the fold");

    // The request the model actually saw after the clear.
    let requests = provider.requests();
    let after = &requests[before..];
    assert!(!after.is_empty(), "the respawned turn re-sampled");
    let items = &after[after.len() - 1].items;
    let stubs: Vec<&str> = items
        .iter()
        .filter_map(|i| match &**i {
            Item::ToolResults { results } => Some(results),
            _ => None,
        })
        .flatten()
        .map(|r| r.content.as_str())
        .collect();
    assert_eq!(stubs.len(), 1, "one result in this history");
    assert!(
        stubs[0].starts_with("<cleared tool_use_id=\"t1\""),
        "the result is a stub: {}",
        stubs[0]
    );
    assert!(
        stubs[0].contains("recall t1"),
        "the stub says how to undo it"
    );
    assert!(
        !items.iter().any(|i| match &**i {
            Item::ToolResults { results } => results.iter().any(|r| r.content.contains("yyyy")),
            _ => false,
        }),
        "the body is gone from the projection"
    );
    // Assistant turns are untouched — only results are on this rung.
    let assistant: Vec<String> = items
        .iter()
        .filter_map(|i| match &**i {
            Item::Assistant { blocks } => Some(assistant_text(blocks)),
            _ => None,
        })
        .collect();
    assert!(
        assistant.iter().any(|t| t == "read it"),
        "assistant text survives verbatim: {assistant:?}"
    );
}

#[tokio::test]
async fn the_projection_estimate_drops_and_one_entry_covers_the_batch() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, config());
    let file = s.dir.path().join("big.txt");
    std::fs::write(&file, "y".repeat(2_100)).expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    provider.push_script(ScriptedProvider::tool_call(
        "t1",
        "read",
        json!({ "path": path }),
    ));
    provider.push_script(ScriptedProvider::text_reply("read it"));
    run(&mut s, "read the big file").await;
    provider.push_script(ScriptedProvider::text_reply("second"));
    run(&mut s, "now something else").await;

    let log = std::fs::read_to_string(
        std::fs::read_dir(s.dir.path())
            .expect("session dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .expect("session log"),
    )
    .expect("read log");
    assert_eq!(
        log.lines()
            .filter(|l| l.contains("\"kind\":\"cleared\""))
            .count(),
        1,
        "one entry per trigger, never one per result"
    );
    assert!(
        log.contains(&"y".repeat(2_000)),
        "the log keeps the body; only the projection lost it"
    );

    // Main-model requests only: the summarize calls carry their own system
    // prompt and their own (much smaller) transcript.
    let estimates: Vec<u64> = provider
        .requests()
        .iter()
        .filter(|r| r.system.contains("test-system"))
        .map(|r| hotl_context::tokens::estimate_items(&r.items))
        .collect();
    let peak = *estimates.iter().max().expect("requests were made");
    let last = *estimates.last().expect("requests were made");
    assert!(
        last * 2 < peak,
        "the estimate must fall across the clear: peak {peak}, after {last}"
    );
}

/// A stub is not a candidate, so the second prompt after a clear does not
/// re-clear — and, `TurnContinuation::cleared` aside, could not.
#[tokio::test]
async fn clearing_is_once_and_idempotent() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, config());
    let file = s.dir.path().join("big.txt");
    std::fs::write(&file, "y".repeat(2_100)).expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    provider.push_script(ScriptedProvider::tool_call(
        "t1",
        "read",
        json!({ "path": path }),
    ));
    provider.push_script(ScriptedProvider::text_reply("read it"));
    run(&mut s, "one").await;
    provider.push_script(ScriptedProvider::text_reply("two"));
    assert_eq!(run(&mut s, "two").await.1, 1);
    provider.push_script(ScriptedProvider::text_reply("three"));
    assert_eq!(run(&mut s, "three").await.1, 0, "nothing left to clear");
}

/// A clear ends the turn and respawns it, exactly as a fold does — so the
/// prompt's reported spend must cover the samples on *both* sides of it
/// (0051 decision 6 composed with this rung). A `carry_usage` that reset at
/// the respawn would under-report every cleared prompt.
#[tokio::test]
async fn a_clear_carries_the_turn_spend_across_the_respawn() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, config());
    let file = s.dir.path().join("big.txt");
    std::fs::write(&file, "y".repeat(2_100)).expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    provider.push_script(billed(
        ScriptedProvider::tool_call("t1", "read", json!({ "path": path })),
        700,
    ));
    provider.push_script(billed(ScriptedProvider::text_reply("read it"), 800));
    let (_, _, _, first) = run_reporting_usage(&mut s, "read the big file").await;
    assert_eq!(first.input_tokens, 1_500, "both samples counted");

    provider.push_script(billed(ScriptedProvider::text_reply("second"), 900));
    let (_, cleared, _, second) = run_reporting_usage(&mut s, "now something else").await;
    assert_eq!(cleared, 1, "the clear fired");
    assert_eq!(
        second.input_tokens, 900,
        "the sample after the clear is billed to this prompt, and nothing is double-counted"
    );
}

/// A sample whose `Completed` reports a chosen `input_tokens`.
fn billed(
    mut script: Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>>,
    input_tokens: u64,
) -> Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>> {
    if let Some(Ok(hotl_provider::StreamEvent::Completed { usage, .. })) = script.last_mut() {
        usage.input_tokens = input_tokens;
    }
    script
}
