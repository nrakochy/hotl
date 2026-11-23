use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, StopReason, SyntheticReason, TokenUsage};
use serde_json::json;

type Seen = Arc<std::sync::Mutex<Vec<Vec<Item>>>>;

struct SlowFinish {
    calls: Arc<AtomicUsize>,
    seen: Seen,
}

impl Provider for SlowFinish {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push(req.items.iter().map(|i| (**i).clone()).collect());
        Box::pin(futures_util::stream::unfold(0u8, |i| async move {
            match i {
                0 => Some((Ok(StreamEvent::Started { request_id: None }), 1)),
                1 => Some((
                    Ok(StreamEvent::TextDelta {
                        index: 0,
                        text: "thinking…".into(),
                    }),
                    2,
                )),
                2 => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Some((
                        Ok(StreamEvent::Completed {
                            stop: StopReason::EndTurn,
                            usage: TokenUsage::default(),
                            blocks: vec![json!({"type": "text", "text": "the reply"})],
                        }),
                        3,
                    ))
                }
                _ => None,
            }
        }))
    }
}

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
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
        config,
    });
    Session {
        handle,
        log_path,
        dir,
    }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

fn replayed_items(log_path: &Path) -> Vec<Item> {
    std::fs::read_to_string(log_path)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<hotl_types::Entry>(line).expect("parse entry"))
        .filter_map(|entry| match entry.payload {
            EntryPayload::Item { item } => Some(item),
            _ => None,
        })
        .collect()
}

fn counting() -> (SlowFinish, Arc<AtomicUsize>, Seen) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Seen::default();
    (
        SlowFinish {
            calls: calls.clone(),
            seen: seen.clone(),
        },
        calls,
        seen,
    )
}

fn is_note(item: &Item) -> bool {
    matches!(
        item,
        Item::User {
            synthetic: Some(SyntheticReason::ShellCommand),
            ..
        }
    )
}

#[tokio::test]
async fn an_idle_shell_note_is_logged_and_starts_no_turn() {
    let (provider, calls, _) = counting();
    let s = session(Arc::new(provider), EngineConfig::default());
    s.handle.shell_note("git status", "clean", Some(0)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let items = replayed_items(&s.log_path);
    let note = items.iter().find(|i| is_note(i)).expect("note logged");
    let Item::User { text, .. } = note else {
        unreachable!()
    };
    assert!(text.contains("$ git status"), "{text}");
    assert!(text.contains("clean"), "{text}");
}

#[tokio::test]
async fn a_shell_note_mid_reply_lands_after_it_and_earns_no_continuation() {
    let (provider, calls, _) = counting();
    let mut s = session(Arc::new(provider), EngineConfig::default());
    s.handle.prompt("go".into()).await;
    loop {
        if let EngineEvent::TextDelta(_) = next_event(&mut s).await {
            break;
        }
    }
    s.handle.shell_note("ls", "a b", Some(0)).await;
    let mut done = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(EngineEvent::TurnDone { .. })) =
            tokio::time::timeout(Duration::from_millis(200), s.handle.events.recv()).await
        {
            done += 1;
        }
    }
    assert_eq!(done, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let items = replayed_items(&s.log_path);
    let assistant = items
        .iter()
        .position(|i| matches!(i, Item::Assistant { .. }))
        .expect("assistant");
    let note = items.iter().position(is_note).expect("note");
    assert!(note > assistant, "{items:?}");
}

#[tokio::test]
async fn the_next_prompt_carries_the_note() {
    let (provider, _, seen) = counting();
    let mut s = session(Arc::new(provider), EngineConfig::default());
    s.handle
        .shell_note("cargo test", "FAILED one", Some(101))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    s.handle.prompt("what failed?".into()).await;
    loop {
        if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
            break;
        }
    }
    let seen = seen.lock().unwrap();
    let items = &seen[0];
    let note = items.iter().position(is_note).expect("note in request");
    let prompt = items
        .iter()
        .position(
            |i| matches!(i, Item::User { text, synthetic: None, .. } if text == "what failed?"),
        )
        .expect("prompt in request");
    assert!(note < prompt);
}

struct ToolThenSlow {
    calls: Arc<AtomicUsize>,
    seen: Seen,
}

impl Provider for ToolThenSlow {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            self.seen
                .lock()
                .unwrap()
                .push(req.items.iter().map(|i| (**i).clone()).collect());
            return Box::pin(futures_util::stream::iter(
                hotl_provider::ScriptedProvider::tool_call(
                    "t1",
                    "bash",
                    json!({"command": "echo hi"}),
                ),
            ));
        }
        SlowFinish {
            calls: Arc::new(AtomicUsize::new(0)),
            seen: self.seen.clone(),
        }
        .stream(req)
    }
}

fn is_steer(item: &Item) -> bool {
    matches!(
        item,
        Item::User {
            synthetic: Some(SyntheticReason::Steer),
            ..
        }
    )
}

#[tokio::test]
async fn a_shell_note_behind_an_answered_steer_starts_no_extra_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Seen::default();
    let mut s = session(
        Arc::new(ToolThenSlow {
            calls: calls.clone(),
            seen: seen.clone(),
        }),
        EngineConfig::default(),
    );
    s.handle.prompt("go".into()).await;
    let reply = loop {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            break reply;
        }
    };
    s.handle.steer("also check X".into()).await;
    let _ = reply.send(hotl_engine::AskReply::Allow);
    loop {
        if let EngineEvent::TextDelta(_) = next_event(&mut s).await {
            let answering = seen
                .lock()
                .unwrap()
                .last()
                .is_some_and(|items| items.iter().any(is_steer));
            if answering {
                break;
            }
        }
    }
    let answered_by = calls.load(Ordering::SeqCst);
    s.handle.shell_note("ls", "a b", Some(0)).await;
    let mut done = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(EngineEvent::TurnDone { .. })) =
            tokio::time::timeout(Duration::from_millis(200), s.handle.events.recv()).await
        {
            done += 1;
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), answered_by);
    assert_eq!(done, 1);
    let items = replayed_items(&s.log_path);
    assert!(is_note(items.last().expect("items")), "{items:?}");
}
