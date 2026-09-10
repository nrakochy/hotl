//! 0059 T4 — the stagnation nudges end to end: a repeated failure, an
//! unproductive cycle and file churn each get one `SystemReminder` per turn,
//! committed at the batch boundary. The pure detectors are unit-tested in
//! `nudge.rs`; this pins that the engine actually reaches them and that the
//! reminders reach the next request.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

fn session(provider: Arc<ScriptedProvider>, dir: &std::path::Path) -> SessionHandle {
    let config = EngineConfig {
        max_turns: 20,
        // Wide enough that the failure budget never ends a turn before the
        // nudge lands — the two guards are separate voices on purpose.
        tool_failure_budget: 50,
        ..Default::default()
    };
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

async fn run(handle: &mut SessionHandle) {
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
}

/// Every reminder the turn's requests ended up carrying, deduplicated by the
/// last request (which carries them all).
fn reminders(provider: &ScriptedProvider) -> Vec<String> {
    let last = provider.requests().pop().expect("at least one request");
    last.items
        .iter()
        .filter_map(|i| match &**i {
            hotl_types::Item::User {
                text,
                synthetic: Some(hotl_types::SyntheticReason::SystemReminder),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn matching(provider: &ScriptedProvider, needle: &str) -> Vec<String> {
    reminders(provider)
        .into_iter()
        .filter(|r| r.contains(needle))
        .collect()
}

fn bash(id: &str, cmd: &str) -> Vec<Result<StreamEvent, ProviderError>> {
    ScriptedProvider::tool_call(id, "bash", json!({"command": cmd}))
}

#[tokio::test]
async fn two_identical_failing_bash_calls_are_named_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        bash("a", "exit 7"),
        bash("b", "exit 7"),
        // A third identical failure must not earn a second reminder.
        bash("c", "exit 7"),
        ScriptedProvider::text_reply("giving up"),
    ]));
    let mut handle = session(Arc::clone(&provider), dir.path());
    run(&mut handle).await;
    let hits = matching(&provider, "failed the same way");
    assert_eq!(hits.len(), 1, "exactly one streak reminder: {hits:?}");
    assert!(
        hits[0].contains("The last two bash calls failed the same way"),
        "{hits:?}"
    );
    assert!(
        hits[0].contains("Change the approach instead of retrying."),
        "{hits:?}"
    );
}

#[tokio::test]
async fn an_alternating_pair_whose_results_keep_changing_is_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("counter.txt");
    std::fs::write(&file, "0").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    // A-B-A-B with the SAME arguments each round, and results that keep
    // changing because the file grows — a cycle, not the doom detector's
    // identical-result loop.
    let grow = format!("printf x >> {path} && wc -c < {path}");
    let provider = Arc::new(ScriptedProvider::new(vec![
        bash("a1", &grow),
        ScriptedProvider::tool_call("b1", "read", json!({"path": path})),
        bash("a2", &grow),
        ScriptedProvider::tool_call("b2", "read", json!({"path": path})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle = session(Arc::clone(&provider), dir.path());
    run(&mut handle).await;
    let hits = matching(&provider, "alternating between");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].contains("stop and reassess."), "{hits:?}");
}

#[tokio::test]
async fn four_edits_to_one_file_in_a_turn_are_named_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("churn.txt");
    std::fs::write(&file, "v0").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    let mut scripts = Vec::new();
    for n in 0..5 {
        scripts.push(ScriptedProvider::tool_call(
            &format!("w{n}"),
            "write",
            json!({"path": path, "content": format!("v{n}")}),
        ));
    }
    scripts.push(ScriptedProvider::text_reply("done"));
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut handle = session(Arc::clone(&provider), dir.path());
    run(&mut handle).await;
    let hits = matching(&provider, "times this turn");
    assert_eq!(
        hits.len(),
        1,
        "one churn reminder, not one per edit: {hits:?}"
    );
    assert!(hits[0].contains("4 times this turn"), "{hits:?}");
    assert!(
        hits[0].contains("Reconsider the approach before the next edit."),
        "{hits:?}"
    );
}

/// A turn that is making progress says nothing: the nudges are for
/// stagnation, and a reminder on a healthy turn is pure noise.
#[tokio::test]
async fn a_productive_turn_earns_no_nudge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "body").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("r1", "read", json!({"path": path})),
        bash("b1", "true"),
        ScriptedProvider::tool_call("g1", "glob", json!({"pattern": "*.txt"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle = session(Arc::clone(&provider), dir.path());
    run(&mut handle).await;
    let noise: Vec<String> = reminders(&provider)
        .into_iter()
        .filter(|r| {
            r.contains("failed the same way")
                || r.contains("alternating between")
                || r.contains("times this turn")
        })
        .collect();
    assert!(noise.is_empty(), "{noise:?}");
}
