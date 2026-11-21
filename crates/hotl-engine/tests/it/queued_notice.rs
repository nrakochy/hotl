use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{
    spawn_session, AckMode, AskReply, EngineConfig, EngineEvent, Outcome, SessionCmd, SessionDeps,
    SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, SyntheticReason};
use serde_json::json;

struct Recording {
    inner: ScriptedProvider,
    seen: Mutex<Vec<Vec<String>>>,
}

impl Recording {
    fn new(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Arc<Self> {
        Arc::new(Self {
            inner: ScriptedProvider::new(scripts),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<Vec<String>> {
        self.seen.lock().expect("seen").clone()
    }
}

impl Provider for Recording {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        self.seen
            .lock()
            .expect("seen")
            .push(req.items.iter().map(|i| label(i)).collect());
        self.inner.stream(req)
    }
}

fn label(item: &Item) -> String {
    match item {
        Item::User { text, .. } => format!("user:{text}"),
        Item::Assistant { blocks, .. } => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("tool_use") => format!("call:{}", b["id"].as_str().unwrap_or("")),
                _ => format!("reply:{}", b["text"].as_str().unwrap_or("")),
            })
            .collect::<Vec<_>>()
            .join(","),
        Item::ToolResults { results } => format!(
            "results:{}",
            results
                .iter()
                .map(|r| r.tool_use_id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => format!("{other:?}"),
    }
}

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<Recording>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
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
    Session {
        handle,
        log_path,
        dir,
    }
}

fn logged(s: &Session) -> Vec<String> {
    std::fs::read_to_string(&s.log_path)
        .expect("read log")
        .lines()
        .map(|line| serde_json::from_str::<hotl_types::Entry>(line).expect("entry"))
        .filter_map(|entry| match entry.payload {
            EntryPayload::Item { item } => Some(label(&item)),
            _ => None,
        })
        .collect()
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

#[derive(Debug, PartialEq)]
enum Seen {
    Started(Vec<u64>),
    Done(String),
}

async fn settle(s: &mut Session, turns: usize) -> Vec<Seen> {
    let mut seen = Vec::new();
    while seen.iter().filter(|e| matches!(e, Seen::Done(_))).count() < turns {
        match next_event(s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnStarted { tags } => seen.push(Seen::Started(tags)),
            EngineEvent::TurnDone { outcome, .. } => seen.push(Seen::Done(match outcome {
                Outcome::Done { text } => text,
                other => format!("{other:?}"),
            })),
            _ => {}
        }
    }
    seen
}

fn assert_every_request_is_committed_history(s: &Session, requests: &[Vec<String>]) {
    let log = logged(s);
    for (n, req) in requests.iter().enumerate() {
        assert!(
            log.starts_with(req),
            "request {n} is not a prefix of the committed history\nrequest: {req:#?}\nlog: {log:#?}"
        );
    }
}

async fn notice_while_a_tool_runs(
    synthetic: SyntheticReason,
    key: Option<String>,
    config: EngineConfig,
) {
    let provider = Recording::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("one"),
        ScriptedProvider::text_reply("two"),
    ]);
    let mut s = session(Arc::clone(&provider), config);
    s.handle.prompt_answering(1, "p1".into(), Vec::new()).await;
    let reply = loop {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            break reply;
        }
    };
    s.handle.prompt_answering(2, "p2".into(), Vec::new()).await;
    s.handle
        .commands()
        .upgrade()
        .expect("actor alive")
        .send(SessionCmd::Notify {
            text: "notice".into(),
            synthetic,
            key,
            charge: None,
        })
        .await
        .expect("notify");
    let _ = reply.send(AskReply::Allow);
    let seen = settle(&mut s, 2).await;
    assert_eq!(
        seen,
        vec![
            Seen::Done("one".into()),
            Seen::Started(vec![2]),
            Seen::Done("two".into()),
        ]
    );
    assert_eq!(
        logged(&s),
        vec![
            "user:p1",
            "call:t1",
            "results:t1",
            "user:notice",
            "reply:one",
            "user:p2",
            "reply:two",
        ]
    );
    let requests = provider.requests();
    assert_every_request_is_committed_history(&s, &requests);
    assert_eq!(
        requests.last().expect("p2 sampled"),
        &logged(&s)[..6].to_vec(),
        "p2's turn sampled without its prompt or the reply before it"
    );
}

#[tokio::test]
async fn a_job_notice_released_mid_turn_is_answered_by_that_turn_and_the_queued_prompt_sees_everything(
) {
    notice_while_a_tool_runs(
        SyntheticReason::BackgroundJob,
        None,
        EngineConfig::default(),
    )
    .await;
}

#[tokio::test]
async fn a_child_notice_released_mid_turn_is_answered_by_that_turn_and_the_queued_prompt_sees_everything(
) {
    notice_while_a_tool_runs(
        SyntheticReason::SubagentResult,
        Some("child-1".into()),
        EngineConfig::default(),
    )
    .await;
}

#[tokio::test]
async fn a_mid_turn_notice_under_sync_acks_keeps_the_same_order() {
    notice_while_a_tool_runs(
        SyntheticReason::BackgroundJob,
        None,
        EngineConfig {
            ack_mode: AckMode::Sync,
            ..EngineConfig::default()
        },
    )
    .await;
}

#[tokio::test]
async fn a_real_background_job_ending_under_a_foreground_tool_keeps_the_queued_prompt_in_context() {
    let provider = Recording::new(vec![
        ScriptedProvider::tool_call(
            "bg",
            "bash",
            json!({"command": "sleep 0.1", "background": true}),
        ),
        ScriptedProvider::tool_call("fg", "bash", json!({"command": "sleep 1"})),
        ScriptedProvider::text_reply("one"),
        ScriptedProvider::text_reply("two"),
    ]);
    let mut s = session(Arc::clone(&provider), EngineConfig::default());
    s.handle.prompt_answering(1, "p1".into(), Vec::new()).await;
    let mut asks = 0;
    while asks < 2 {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            asks += 1;
            if asks == 2 {
                s.handle.prompt_answering(2, "p2".into(), Vec::new()).await;
            }
            let _ = reply.send(AskReply::Allow);
        }
    }
    let seen = settle(&mut s, 2).await;
    assert_eq!(
        seen,
        vec![
            Seen::Done("one".into()),
            Seen::Started(vec![2]),
            Seen::Done("two".into()),
        ]
    );
    let log = logged(&s);
    let notice = log
        .iter()
        .position(|l| l.starts_with("user:") && l != "user:p1" && l != "user:p2")
        .expect("the job's notice was committed");
    let p2 = log.iter().position(|l| l == "user:p2").expect("p2");
    assert!(notice < p2, "{log:#?}");
    let requests = provider.requests();
    assert_every_request_is_committed_history(&s, &requests);
    let last = requests.last().expect("p2 sampled");
    assert_eq!(last.last().map(String::as_str), Some("user:p2"));
    assert!(last.iter().any(|l| l == "reply:one"), "{last:#?}");
}
