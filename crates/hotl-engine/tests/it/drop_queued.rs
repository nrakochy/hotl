use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionCmd, SessionDeps,
    SessionHandle, TurnEnd,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::TokenUsage;
use serde_json::json;
use tokio::sync::oneshot;

const QUEUE_MAX: u64 = 64;

struct Session {
    handle: Arc<SessionHandle>,
    events: tokio::sync::mpsc::Receiver<EngineEvent>,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<ScriptedProvider>) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
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
    let events = std::mem::replace(&mut handle.events, tokio::sync::mpsc::channel(1).1);
    Session {
        handle: Arc::new(handle),
        events,
        dir,
    }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

async fn parked_at_an_ask(scripts: usize) -> (Session, oneshot::Sender<AskReply>) {
    let mut all = vec![ScriptedProvider::tool_call(
        "t1",
        "bash",
        json!({"command": "echo hi"}),
    )];
    all.extend((0..scripts).map(|i| ScriptedProvider::text_reply(&format!("reply {i}"))));
    let mut s = session(Arc::new(ScriptedProvider::new(all)));
    s.handle.prompt_answering(1, "p1".into(), Vec::new()).await;
    let reply = loop {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            break reply;
        }
    };
    (s, reply)
}

async fn drain_started(s: &mut Session, window: Duration) -> Vec<Vec<u64>> {
    let mut started = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(ev)) = tokio::time::timeout_at(deadline, s.events.recv()).await {
        match ev {
            EngineEvent::TurnStarted { tags } => started.push(tags),
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            _ => {}
        }
    }
    started
}

async fn queued(s: &mut Session) {
    loop {
        if let EngineEvent::PromptQueued = next_event(s).await {
            return;
        }
    }
}

fn finished() -> SessionCmd {
    SessionCmd::TurnFinished {
        end: TurnEnd::Outcome(Outcome::Done {
            text: "turn one".into(),
        }),
        usage: TokenUsage::default(),
        mispredictions: 0,
        tools_ran: 0,
        unrecoverable: false,
    }
}

#[tokio::test]
async fn two_quick_cancels_hold_the_queue_until_the_second_one_lands() {
    let (mut s, _ask) = parked_at_an_ask(4).await;
    s.handle.prompt_answering(2, "p2".into(), Vec::new()).await;
    queued(&mut s).await;
    let cmd = s.handle.commands().upgrade().expect("actor alive");
    let first = s.handle.drop_queued();
    s.handle.prompt_answering(3, "p3".into(), Vec::new()).await;
    cmd.send(finished()).await.expect("send");
    let second = s.handle.drop_queued();
    assert_eq!(first.await.expect("first reply"), vec![2]);
    assert_eq!(second.await.expect("second reply"), vec![3]);
    let started = drain_started(&mut s, Duration::from_millis(500)).await;
    assert!(
        !started.contains(&vec![3]),
        "a prompt sent before the second cancel ran: {started:?}"
    );
}

#[tokio::test]
async fn a_drop_on_a_full_channel_never_takes_a_prompt_sent_after_it() {
    let (mut s, ask) = parked_at_an_ask(4).await;
    s.handle.prompt_answering(2, "p2".into(), Vec::new()).await;
    queued(&mut s).await;
    let cmd = s.handle.commands().upgrade().expect("actor alive");
    let (go, ready) = oneshot::channel::<()>();
    let later = Arc::clone(&s.handle);
    let sender = tokio::spawn(async move {
        let _ = ready.await;
        later.prompt_answering(3, "p3".into(), Vec::new()).await;
    });
    tokio::task::yield_now().await;
    while cmd.try_send(SessionCmd::Continue).is_ok() {}
    let _ = go.send(());
    let dropped = s.handle.drop_queued();
    sender.await.expect("sender");
    assert_eq!(dropped.await.expect("reply"), vec![2]);
    let _ = ask.send(AskReply::Allow);
    let started = drain_started(&mut s, Duration::from_millis(1500)).await;
    assert!(
        started.contains(&vec![3]),
        "the prompt sent after the cancel never ran: {started:?}"
    );
}

#[tokio::test]
async fn a_tagged_prompt_past_the_cap_never_folds_into_an_untagged_entry() {
    let (mut s, ask) = parked_at_an_ask(4).await;
    for tag in 2..QUEUE_MAX + 1 {
        s.handle
            .prompt_answering(tag, format!("p{tag}"), Vec::new())
            .await;
    }
    s.handle.prompt("engine work".into()).await;
    s.handle
        .prompt_answering(1000, "late".into(), Vec::new())
        .await;
    let dropped = s.handle.drop_queued().await.expect("reply");
    let mut expected: Vec<u64> = (2..QUEUE_MAX + 1).collect();
    expected.push(1000);
    assert_eq!(dropped, expected);
    let _ = ask.send(AskReply::Allow);
    let started = drain_started(&mut s, Duration::from_millis(1500)).await;
    assert_eq!(
        started,
        vec![Vec::<u64>::new()],
        "the untagged entry did not run alone"
    );
}

#[tokio::test]
async fn an_untagged_prompt_past_the_cap_never_folds_into_a_tagged_entry() {
    let (mut s, ask) = parked_at_an_ask(4).await;
    for tag in 2..QUEUE_MAX + 2 {
        s.handle
            .prompt_answering(tag, format!("p{tag}"), Vec::new())
            .await;
    }
    s.handle.prompt("engine work".into()).await;
    let dropped = s.handle.drop_queued().await.expect("reply");
    assert_eq!(dropped, (2..QUEUE_MAX + 2).collect::<Vec<_>>());
    let _ = ask.send(AskReply::Allow);
    let started = drain_started(&mut s, Duration::from_millis(1500)).await;
    assert_eq!(
        started,
        vec![Vec::<u64>::new()],
        "the untagged prompt was dropped with the tagged entry it folded into"
    );
}

#[tokio::test]
async fn a_prompt_past_the_cap_never_jumps_ahead_of_a_newer_entry_of_the_other_kind() {
    let (mut s, ask) = parked_at_an_ask(QUEUE_MAX as usize + 4).await;
    for tag in 2..QUEUE_MAX + 2 {
        s.handle
            .prompt_answering(tag, format!("p{tag}"), Vec::new())
            .await;
    }
    s.handle.prompt("engine work".into()).await;
    s.handle
        .prompt_answering(1000, "late".into(), Vec::new())
        .await;
    let _ = ask.send(AskReply::Allow);
    let mut started = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), s.events.recv()).await {
        match ev {
            EngineEvent::TurnStarted { tags } => started.push(tags),
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            _ => {}
        }
    }
    let mut expected: Vec<Vec<u64>> = (2..QUEUE_MAX + 2).map(|t| vec![t]).collect();
    expected.push(Vec::new());
    expected.push(vec![1000]);
    assert_eq!(started, expected);
}
