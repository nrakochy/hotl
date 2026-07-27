//! Images ride the prompt/steer plumbing end to end: `prompt_with` /
//! `steer_with` → SessionCmd → committed `Item::User { images }` → the
//! append-only log — and survive queue promotion, since a queued prompt's
//! attachments must not vanish while it waits its turn.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, StopReason, SyntheticReason, TokenUsage, UserImage};
use serde_json::json;

/// Emits a delta, then stalls before completing — deterministic room to
/// steer or queue mid-turn.
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

fn session() -> Session {
    let config = EngineConfig::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        provider: Arc::new(SlowFinish),
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

async fn wait_turn_done(s: &mut Session) {
    loop {
        if let EngineEvent::TurnDone { .. } = next_event(s).await {
            break;
        }
    }
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

fn png(data: &str) -> UserImage {
    UserImage {
        media_type: "image/png".into(),
        data: data.into(),
    }
}

#[tokio::test]
async fn prompt_with_images_persists_them_to_the_log() {
    let mut s = session();
    s.handle
        .prompt_with("look: [Image #1]".into(), vec![png("aW1nMQ==")])
        .await;
    wait_turn_done(&mut s).await;

    let items = replayed_items(&s.log_path);
    let Some(Item::User { text, images, .. }) = items.first() else {
        panic!("first item must be the prompt: {items:?}");
    };
    assert_eq!(text, "look: [Image #1]");
    assert_eq!(images, &vec![png("aW1nMQ==")]);
}

#[tokio::test]
async fn a_queued_prompt_keeps_its_images_through_promotion() {
    let mut s = session();
    s.handle.prompt("go".into()).await;
    // Queue the second prompt while the first turn is still streaming.
    loop {
        if let EngineEvent::TextDelta(_) = next_event(&mut s).await {
            break;
        }
    }
    s.handle
        .prompt_with("and this: [Image #1]".into(), vec![png("cXVldWVk")])
        .await;
    wait_turn_done(&mut s).await;
    wait_turn_done(&mut s).await;

    let items = replayed_items(&s.log_path);
    let queued = items
        .iter()
        .find_map(|i| match i {
            Item::User { text, images, .. } if text.starts_with("and this") => Some(images),
            _ => None,
        })
        .expect("the promoted prompt must be in the log");
    assert_eq!(queued, &vec![png("cXVldWVk")]);
}

#[tokio::test]
async fn a_mid_stream_steer_carries_its_images_steer_tagged() {
    let mut s = session();
    s.handle.prompt("go".into()).await;
    loop {
        if let EngineEvent::TextDelta(_) = next_event(&mut s).await {
            break;
        }
    }
    s.handle
        .steer_with("see [Image #1]".into(), vec![png("c3RlZXI=")])
        .await;
    wait_turn_done(&mut s).await;

    let items = replayed_items(&s.log_path);
    let steer = items
        .iter()
        .find_map(|i| match i {
            Item::User {
                synthetic: Some(SyntheticReason::Steer),
                images,
                ..
            } => Some(images),
            _ => None,
        })
        .expect("the steer must be in the log");
    assert_eq!(steer, &vec![png("c3RlZXI=")]);
}
