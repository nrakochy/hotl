//! `/btw` asides (0071 T5): one sample over the turn's byte-identical cached
//! prefix with the question on the ephemeral tail; nothing enters the log
//! but a `Usage` entry.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_testkit::wire::{durable_wire_body, is_structural_prefix, marker_positions};
use hotl_tools::rules::Rules;
use hotl_tools::Registry;

struct Session {
    handle: SessionHandle,
    provider: Arc<ScriptedProvider>,
    log_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn session(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: provider.clone(),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: true,
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
        config,
    });
    Session {
        handle,
        provider,
        log_path,
        _dir: dir,
    }
}

async fn next_matching(
    handle: &mut SessionHandle,
    mut pick: impl FnMut(&EngineEvent) -> bool,
) -> EngineEvent {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if pick(&event) {
            return event;
        }
    }
}

fn kinds(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .map(|e| format!("{:?}", hotl_store::EntryKind::from(&e.payload)).to_lowercase())
        .collect()
}

#[tokio::test]
async fn an_aside_samples_over_the_byte_identical_prefix() {
    let mut s = session(
        vec![
            ScriptedProvider::text_reply("the answer"),
            ScriptedProvider::text_reply("briefly: yes"),
        ],
        EngineConfig::default(),
    );
    s.handle.prompt("do the work".into()).await;
    next_matching(&mut s.handle, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    s.handle.aside("is it done?".into()).await;
    let done = next_matching(&mut s.handle, |e| {
        matches!(e, EngineEvent::AsideDone { .. })
    })
    .await;
    let EngineEvent::AsideDone {
        question,
        text,
        usage,
    } = done
    else {
        unreachable!()
    };
    assert_eq!(question, "is it done?");
    assert_eq!(text, "briefly: yes");
    assert_eq!(usage.input_tokens, 10);
    let reqs = s.provider.requests();
    assert_eq!(reqs.len(), 2);
    let (turn, aside) = (&reqs[0], &reqs[1]);
    assert!(
        is_structural_prefix(&durable_wire_body(turn), &durable_wire_body(aside)),
        "the aside's durable body extends the turn's"
    );
    assert_eq!(
        marker_positions(&durable_wire_body(turn)),
        marker_positions(&durable_wire_body(aside)),
        "same cache markers, same prefix"
    );
    assert_eq!(aside.cache_key, turn.cache_key);
    assert!(aside.tools.len() == turn.tools.len() && !aside.tools.is_empty());
    assert!(!aside.thinking);
    let last = aside
        .ephemeral_tail
        .last()
        .expect("the question rides the tail");
    assert!(
        matches!(&**last, hotl_types::Item::User { text, .. } if text.contains("<aside>") && text.contains("is it done?"))
    );
}

#[tokio::test]
async fn an_aside_writes_no_item_and_replay_shows_no_trace_of_it() {
    let mut s = session(
        vec![
            ScriptedProvider::text_reply("the answer"),
            ScriptedProvider::text_reply("aside answer"),
        ],
        EngineConfig::default(),
    );
    s.handle.prompt("go".into()).await;
    next_matching(&mut s.handle, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    let before = hotl_store::replay(&s.log_path).unwrap().items;
    let kinds_before = kinds(&std::fs::read_to_string(&s.log_path).unwrap());
    s.handle.aside("hm?".into()).await;
    next_matching(&mut s.handle, |e| {
        matches!(e, EngineEvent::AsideDone { .. })
    })
    .await;
    // The usage entry is appended through the actor; give the ack a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = std::fs::read_to_string(&s.log_path).unwrap();
    assert_eq!(hotl_store::replay(&s.log_path).unwrap().items, before);
    let mut expected = kinds_before;
    expected.push("usage".into());
    assert_eq!(kinds(&log), expected, "{log}");
    assert!(!log.contains("hm?"), "the question never enters the log");
}

#[tokio::test]
async fn an_aside_during_a_turn_does_not_block_the_actor() {
    let mut s = session(
        vec![
            ScriptedProvider::hang_after(ScriptedProvider::partial_text("working…")),
            ScriptedProvider::text_reply("meanwhile: yes"),
        ],
        EngineConfig::default(),
    );
    s.handle.prompt("go".into()).await;
    next_matching(&mut s.handle, |e| matches!(e, EngineEvent::TextDelta(_))).await;
    s.handle.aside("still there?".into()).await;
    let done = tokio::time::timeout(
        Duration::from_secs(5),
        next_matching(&mut s.handle, |e| {
            matches!(e, EngineEvent::AsideDone { .. })
        }),
    )
    .await
    .expect("the aside answered while the turn's stream was still open");
    assert!(matches!(done, EngineEvent::AsideDone { text, .. } if text == "meanwhile: yes"));
}

#[tokio::test]
async fn an_aside_tool_call_is_reported_not_run() {
    let mut s = session(
        vec![
            ScriptedProvider::text_reply("the answer"),
            ScriptedProvider::tool_call("t1", "bash", serde_json::json!({"command": "rm -rf /"})),
        ],
        EngineConfig::default(),
    );
    s.handle.prompt("go".into()).await;
    next_matching(&mut s.handle, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    s.handle.aside("run it?".into()).await;
    let done = next_matching(&mut s.handle, |e| {
        matches!(e, EngineEvent::AsideDone { .. })
    })
    .await;
    let EngineEvent::AsideDone { text, .. } = done else {
        unreachable!()
    };
    assert!(text.contains("asides can't"), "{text}");
    assert_eq!(s.provider.request_count(), 2, "no tool result round-trip");
}

#[tokio::test]
async fn an_aside_uses_the_session_model_not_the_utility_model() {
    let config = EngineConfig {
        utility_model: Some("tiny".into()),
        ..EngineConfig::default()
    };
    let session_model = config.model.clone();
    let mut s = session(
        vec![
            ScriptedProvider::text_reply("the answer"),
            ScriptedProvider::text_reply("aside"),
        ],
        config,
    );
    s.handle.prompt("go".into()).await;
    next_matching(&mut s.handle, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    s.handle.aside("q".into()).await;
    next_matching(&mut s.handle, |e| {
        matches!(e, EngineEvent::AsideDone { .. })
    })
    .await;
    assert_eq!(s.provider.requests()[1].model, session_model);
    assert!(kinds(&std::fs::read_to_string(&s.log_path).unwrap())
        .iter()
        .any(|k| k.as_str() == "usage"));
}
