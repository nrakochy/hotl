//! T3-3: a steer that arrives while the model is *streaming* used to pass the
//! `awaiting_tool_results` check and be appended immediately, ahead of the
//! assistant item for the sample already in flight. The durable projection then
//! read `[…, steer, assistant]` — the reply appearing to answer a steer it never
//! saw, and a prefix-cache break on every later request. `pair_tool_results`
//! repairs only the tool-result case, so this survived a resume.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, StopReason, SyntheticReason, TokenUsage};
use serde_json::json;

/// Emits a `TextDelta`, then stalls before completing — so a test can steer
/// *mid-stream* deterministically rather than racing the sample.
struct SlowFinish;

impl Provider for SlowFinish {
    fn stream(
        &self,
        _req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        Box::pin(futures_util::stream::unfold(0u8, |i| async move {
            match i {
                0 => Some((Ok(StreamEvent::Started), 1)),
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
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
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

/// The durable items, in log order.
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

#[tokio::test]
async fn a_mid_stream_steer_commits_after_the_reply_it_did_not_see() {
    let mut s = session(Arc::new(SlowFinish), EngineConfig::default());
    s.handle.prompt("go".into()).await;

    // The first delta proves the sample is in flight: the snapshot has been
    // granted and the request built, but no assistant item has committed.
    loop {
        if let EngineEvent::TextDelta(_) = next_event(&mut s).await {
            break;
        }
    }
    s.handle.steer("actually, do it differently".into()).await;

    let outcome = loop {
        if let EngineEvent::TurnDone { outcome, .. } = next_event(&mut s).await {
            break outcome;
        }
    };
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "the reply".into()
        }
    );

    let items = replayed_items(&s.log_path);
    let assistant = items
        .iter()
        .position(|i| matches!(i, Item::Assistant { .. }))
        .expect("assistant");
    let steer = items
        .iter()
        .position(|i| {
            matches!(
                i,
                Item::User {
                    synthetic: Some(SyntheticReason::Steer),
                    ..
                }
            )
        })
        .expect("steer");
    assert!(
        steer > assistant,
        "a steer must never precede the reply that could not have seen it; got {items:?}"
    );
}
