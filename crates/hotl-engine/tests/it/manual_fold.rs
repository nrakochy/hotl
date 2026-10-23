//! `/compact [focus]` (0066): a human fold runs now at idle, waits for the
//! turn boundary while a turn runs, carries its focus into the digest prompt
//! and the log entry, and meets the ladder's fold as one fold, not two.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, EngineConfig, EngineEvent, FoldAdmission, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, StopReason, TokenUsage, ToolResultItem};

type BoxStream = futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>;

/// Routes the compaction summarize to one of two scripts — by whether the
/// prompt carries a focus — so the digest says which path ran. The main
/// script streams slowly, so a test can act while a turn is open.
struct Router {
    main: Arc<ScriptedProvider>,
    unfocused: Arc<ScriptedProvider>,
    focused: Arc<ScriptedProvider>,
}

impl Provider for Router {
    fn stream(&self, req: SamplingRequest) -> BoxStream {
        if req.system.contains("compress") {
            let focused = req
                .items
                .iter()
                .any(|i| matches!(&**i, Item::User { text, .. } if text.contains("keep one thing in focus")));
            if focused {
                self.focused.stream(req)
            } else {
                self.unfocused.stream(req)
            }
        } else {
            let inner = self.main.stream(req);
            // Hold the turn open: a pause after the first delta.
            let mut first = true;
            Box::pin(futures_util::StreamExt::then(inner, move |ev| {
                let pause = first && matches!(ev, Ok(StreamEvent::TextDelta { .. }));
                if pause {
                    first = false;
                }
                async move {
                    if pause {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                    ev
                }
            }))
        }
    }
}

fn config(window: u64) -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        context_window: window,
        // The cheap rung would reclaim the same tokens first.
        keep_results_turns: 0,
        max_turns: 10,
        ..Default::default()
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.into(),
        synthetic: None,
        images: Vec::new(),
    }
}

fn call(id: &str) -> Item {
    Item::Assistant {
        blocks: vec![serde_json::json!({
            "type": "tool_use", "id": id, "name": "bash", "input": {"command": "ls"}
        })],
        model: None,
    }
}

fn result(id: &str, content: &str) -> Item {
    Item::ToolResults {
        results: vec![ToolResultItem {
            tool_use_id: id.into(),
            content: content.into(),
            is_error: false,
        }],
    }
}

/// Two finished turns, ~900 tokens: past the 80% rung of a 1 000 window,
/// below it at 1 600 (where only a human fold runs).
fn seeded() -> Vec<Item> {
    vec![
        user("first"),
        call("t1"),
        result("t1", "THE-ONE-THING-WORTH-KEEPING"),
        call("t2"),
        result("t2", &"y".repeat(2_600)),
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "listed"})],
            model: None,
        },
        user("second"),
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "noted"})],
            model: None,
        },
    ]
}

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
    main: Arc<ScriptedProvider>,
    unfocused: Arc<ScriptedProvider>,
    focused: Arc<ScriptedProvider>,
}

fn digest_reply(text: &str, input: u64, output: u64) -> Vec<Result<StreamEvent, ProviderError>> {
    vec![
        Ok(StreamEvent::Started { request_id: None }),
        Ok(StreamEvent::Completed {
            stop: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
            blocks: vec![serde_json::json!({"type": "text", "text": text})],
        }),
    ]
}

fn session(window: u64, initial_items: Vec<Item>, main_replies: usize) -> Session {
    let main = Arc::new(ScriptedProvider::new(
        (0..main_replies)
            .map(|n| ScriptedProvider::text_reply(&format!("reply {n}")))
            .collect(),
    ));
    let unfocused = Arc::new(ScriptedProvider::new(vec![
        digest_reply("GOAL: unfocused", 300, 40),
        digest_reply("GOAL: unfocused", 300, 40),
    ]));
    let focused = Arc::new(ScriptedProvider::new(vec![
        digest_reply("GOAL: focused", 300, 40),
        digest_reply("GOAL: focused", 300, 40),
    ]));
    let config = config(window);
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        provider: Arc::new(Router {
            main: Arc::clone(&main),
            unfocused: Arc::clone(&unfocused),
            focused: Arc::clone(&focused),
        }),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items,
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        concurrency: Default::default(),
        config,
    });
    Session {
        handle,
        log_path,
        _dir: dir,
        main,
        unfocused,
        focused,
    }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// Event names until (and including) the one `until` picks.
async fn events_until(s: &mut Session, until: impl Fn(&EngineEvent) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let ev = next_event(s).await;
        let name = format!("{ev:?}").split('(').next().unwrap().to_string();
        let done = until(&ev);
        out.push(name);
        if done {
            return out;
        }
    }
}

fn compaction_entries(log_path: &std::path::Path) -> Vec<Option<String>> {
    std::fs::read_to_string(log_path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::Compaction { focus, .. } => Some(focus),
            _ => None,
        })
        .collect()
}

fn log_lines(log_path: &std::path::Path) -> usize {
    std::fs::read_to_string(log_path)
        .expect("read log")
        .lines()
        .count()
}

fn digest_text(items: &[Arc<Item>]) -> Option<String> {
    items.iter().find_map(|i| match &**i {
        Item::User { text, .. } if text.contains("GOAL:") => Some(text.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn an_idle_fold_runs_now_and_announces_itself() {
    let mut s = session(1_600, seeded(), 1);
    let before = log_lines(&s.log_path);
    assert_eq!(s.handle.fold(None).await, Some(FoldAdmission::Started));
    let order = events_until(&mut s, |e| matches!(e, EngineEvent::Compacted { .. })).await;
    assert_eq!(order, ["Compacting", "Compacted"]);
    assert_eq!(compaction_entries(&s.log_path), vec![None]);
    assert_eq!(log_lines(&s.log_path), before + 1);
    s.handle.prompt("next".into()).await;
    events_until(&mut s, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    let req = s.main.requests().pop().expect("the next request");
    assert!(
        digest_text(&req.items).unwrap().contains("GOAL: unfocused"),
        "the next request reads the digest"
    );
    assert!(req.items.len() < seeded().len() + 1, "history was folded");
}

#[tokio::test]
async fn the_focus_reaches_the_summarize_prompt_and_the_entry() {
    let mut s = session(1_600, seeded(), 0);
    s.handle.fold(Some("keep the auth notes".into())).await;
    let ev = loop {
        let ev = next_event(&mut s).await;
        if matches!(ev, EngineEvent::Compacted { .. }) {
            break ev;
        }
    };
    assert!(
        matches!(&ev, EngineEvent::Compacted { focus: Some(f), .. } if f == "keep the auth notes")
    );
    let req = s
        .focused
        .requests()
        .pop()
        .expect("the focused summarize ran");
    let Item::User { text, .. } = &*req.items[0] else {
        panic!()
    };
    assert!(
        text.contains("keep one thing in focus:\n\nkeep the auth notes\n"),
        "{text}"
    );
    assert!(s.unfocused.requests().is_empty());
    assert_eq!(
        compaction_entries(&s.log_path),
        vec![Some("keep the auth notes".to_string())]
    );
}

#[tokio::test]
async fn nothing_to_fold_is_reported_not_folded() {
    let mut s = session(1_600, vec![user("hi")], 0);
    let before = log_lines(&s.log_path);
    assert_eq!(s.handle.fold(None).await, Some(FoldAdmission::Started));
    let ev = next_event(&mut s).await;
    let EngineEvent::FoldSkipped { reason } = ev else {
        panic!("{ev:?}");
    };
    assert!(reason.contains("nothing"), "{reason}");
    assert_eq!(log_lines(&s.log_path), before);
    assert!(s.unfocused.requests().is_empty());
}

#[tokio::test]
async fn a_fold_during_a_turn_waits_for_the_boundary() {
    let mut s = session(1_600, seeded(), 1);
    s.handle.prompt("go".into()).await;
    events_until(&mut s, |e| matches!(e, EngineEvent::TextDelta(_))).await;
    assert_eq!(s.handle.fold(None).await, Some(FoldAdmission::Queued));
    let order = events_until(&mut s, |e| matches!(e, EngineEvent::Compacted { .. })).await;
    let done = order
        .iter()
        .position(|n| n == "TurnDone")
        .expect("the turn ended");
    let compacting = order.iter().position(|n| n == "Compacting").unwrap();
    assert!(done < compacting, "{order:?}");
    assert_eq!(
        &order[done..],
        ["TurnDone", "Compacting", "Compacted"],
        "{order:?}"
    );
}

#[tokio::test]
async fn a_pending_fold_runs_before_the_queued_prompt() {
    let mut s = session(1_600, seeded(), 2);
    s.handle.prompt("one".into()).await;
    events_until(&mut s, |e| matches!(e, EngineEvent::TextDelta(_))).await;
    assert_eq!(s.handle.fold(None).await, Some(FoldAdmission::Queued));
    s.handle.prompt("two".into()).await;
    let order = events_until(&mut s, |e| matches!(e, EngineEvent::Compacted { .. })).await;
    assert_eq!(
        order.iter().filter(|n| *n == "TurnDone").count(),
        1,
        "the fold landed between the two turns: {order:?}"
    );
    events_until(&mut s, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    let second = s.main.requests().pop().unwrap();
    assert!(
        digest_text(&second.items).is_some(),
        "the second prompt saw the folded history: {order:?}"
    );
}

#[tokio::test]
async fn a_focused_request_at_the_ladder_boundary_pays_for_a_fresh_digest() {
    let mut s = session(1_000, seeded(), 1);
    s.handle.prompt("go".into()).await;
    assert_eq!(
        s.handle.fold(Some("the auth refactor".into())).await,
        Some(FoldAdmission::Queued)
    );
    let order = events_until(&mut s, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    assert_eq!(
        order.iter().filter(|n| *n == "Compacted").count(),
        1,
        "{order:?}"
    );
    assert_eq!(
        compaction_entries(&s.log_path),
        vec![Some("the auth refactor".to_string())]
    );
    let req = s.main.requests().pop().unwrap();
    assert!(digest_text(&req.items).unwrap().contains("GOAL: focused"));
    assert!(
        s.unfocused.requests().is_empty(),
        "no unfocused digest was adopted"
    );
}

#[tokio::test]
async fn an_unfocused_request_at_the_ladder_boundary_adopts_the_speculative_digest() {
    let mut s = session(1_000, seeded(), 1);
    s.handle.prompt("go".into()).await;
    assert_eq!(s.handle.fold(None).await, Some(FoldAdmission::Queued));
    let order = events_until(&mut s, |e| matches!(e, EngineEvent::TurnDone { .. })).await;
    assert_eq!(
        order.iter().filter(|n| *n == "Compacted").count(),
        1,
        "{order:?}"
    );
    assert_eq!(compaction_entries(&s.log_path), vec![None]);
    let req = s.main.requests().pop().unwrap();
    assert!(digest_text(&req.items).unwrap().contains("GOAL: unfocused"));
    assert_eq!(s.unfocused.request_count(), 1, "one summarize, not two");
    assert!(s.focused.requests().is_empty());
}

#[tokio::test]
async fn the_newest_pending_focus_wins() {
    let mut s = session(1_600, seeded(), 1);
    s.handle.prompt("go".into()).await;
    events_until(&mut s, |e| matches!(e, EngineEvent::TextDelta(_))).await;
    s.handle.fold(Some("first idea".into())).await;
    s.handle.fold(Some("second idea".into())).await;
    let order = events_until(&mut s, |e| matches!(e, EngineEvent::Compacted { .. })).await;
    assert_eq!(order.iter().filter(|n| *n == "Compacted").count(), 1);
    assert_eq!(
        compaction_entries(&s.log_path),
        vec![Some("second idea".to_string())]
    );
}

#[tokio::test]
async fn an_idle_folds_digest_spend_rides_the_next_turn_done() {
    let mut s = session(1_600, seeded(), 1);
    s.handle.fold(None).await;
    events_until(&mut s, |e| matches!(e, EngineEvent::Compacted { .. })).await;
    s.handle.prompt("next".into()).await;
    let usage = loop {
        if let EngineEvent::TurnDone { usage, outcome, .. } = next_event(&mut s).await {
            assert!(matches!(outcome, Outcome::Done { .. }));
            break usage;
        }
    };
    // The prompt's own sample bills 10/5; the digest 300/40.
    assert!(usage.input_tokens >= 310, "{usage:?}");
    assert!(usage.output_tokens >= 45, "{usage:?}");
}
