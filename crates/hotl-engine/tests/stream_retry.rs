//! A provider stream that dies *after* the first byte (0050 T3, A3).
//!
//! The pre-stream retry ladder lives in the provider and is long past by the
//! time a delta has arrived, so before this every 529 mid-stream ended the
//! turn with `Error` — the single most expensive failure a long turn can
//! take, and the one users hit most. The engine now re-samples the same
//! request, bounded, and only when nothing irreversible was sealed.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::rules::Rules;
use hotl_tools::Registry;
use hotl_types::{EntryPayload, Item};
use hotl_types::{StopReason, TokenUsage};

struct Ran {
    outcome: Outcome,
    retries: Vec<(u32, String, bool)>,
    text: String,
    log: String,
    requests: usize,
}

fn overloaded() -> ProviderError {
    ProviderError::Http {
        status: 529,
        message: "overloaded_error".into(),
        retry_after: None,
    }
}

async fn run(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Ran {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut handle: SessionHandle = spawn_session(SessionDeps {
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
        initial_goal: None,
        config,
    });
    handle.prompt("go".into()).await;

    let mut retries = Vec::new();
    let mut text = String::new();
    let outcome = loop {
        let event = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match event {
            EngineEvent::Retrying {
                attempt,
                reason,
                discarded_partial,
            } => retries.push((attempt, reason, discarded_partial)),
            EngineEvent::TextDelta(t) => text.push_str(&t),
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    let requests = provider.request_count();
    drop(handle);

    let log_path = std::fs::read_dir(dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    Ran {
        outcome,
        retries,
        text,
        log: std::fs::read_to_string(&log_path).expect("read log"),
        requests,
    }
}

/// Assistant items the log actually carries, as their concatenated text.
fn assistant_items(log: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in log.lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("entry");
        if let EntryPayload::Item {
            item: Item::Assistant { blocks },
        } = entry.payload
        {
            out.push(
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
            );
        }
    }
    out
}

/// The headline: three deltas, then a 529. One retry notice, one completed
/// sample, and the partial text never reaches the log.
#[tokio::test]
async fn a_529_after_the_first_byte_is_resampled() {
    let r = run(vec![
        ScriptedProvider::error_after(ScriptedProvider::partial_text("half a th"), overloaded()),
        ScriptedProvider::text_reply("the whole answer"),
    ])
    .await;
    assert!(matches!(r.outcome, Outcome::Done { .. }), "{:?}", r.outcome);
    assert_eq!(r.retries.len(), 1, "{:?}", r.retries);
    assert_eq!(r.retries[0].0, 1);
    assert!(
        r.retries[0].1.contains("stream interrupted"),
        "{:?}",
        r.retries
    );
    assert!(r.retries[0].2, "the partial text was on screen");
    assert_eq!(r.requests, 2, "the request was re-sent exactly once");
    assert_eq!(
        assistant_items(&r.log),
        vec!["the whole answer".to_string()],
        "the discarded partial must never reach the log"
    );
    assert!(r.text.contains("the whole answer"), "{}", r.text);
}

/// A dead connection is the same class as a 529 — the idle watchdog reports
/// `Transport`, and that was equally fatal before.
#[tokio::test]
async fn a_transport_death_mid_stream_is_resampled() {
    let r = run(vec![
        ScriptedProvider::error_after(
            ScriptedProvider::partial_text("thinking"),
            ProviderError::Transport("stream idle for 300s".into()),
        ),
        ScriptedProvider::text_reply("recovered"),
    ])
    .await;
    assert!(matches!(r.outcome, Outcome::Done { .. }), "{:?}", r.outcome);
    assert_eq!(r.retries.len(), 1, "{:?}", r.retries);
    assert_eq!(assistant_items(&r.log), vec!["recovered".to_string()]);
}

/// Only the availability class is re-sampled. Auth mid-stream means the key
/// is wrong; retrying it burns the turn slower, not better.
#[tokio::test]
async fn an_auth_error_mid_stream_is_not_resampled() {
    let r = run(vec![
        ScriptedProvider::error_after(
            ScriptedProvider::partial_text("hi"),
            ProviderError::Auth("bad key".into()),
        ),
        ScriptedProvider::text_reply("never reached"),
    ])
    .await;
    assert!(
        matches!(r.outcome, Outcome::Error { .. }),
        "{:?}",
        r.outcome
    );
    assert!(r.retries.is_empty(), "{:?}", r.retries);
    assert_eq!(r.requests, 1);
}

/// The bound is real: a provider that dies every time ends the turn instead
/// of re-sampling forever.
#[tokio::test]
async fn repeated_interruptions_stop_at_the_cap() {
    let scripts = (0..8)
        .map(|_| ScriptedProvider::error_after(ScriptedProvider::partial_text("x"), overloaded()))
        .collect();
    let r = run(scripts).await;
    assert!(
        matches!(r.outcome, Outcome::Error { .. }),
        "{:?}",
        r.outcome
    );
    assert_eq!(
        r.retries.len(),
        hotl_engine::STREAM_RETRY_MAX as usize,
        "{:?}",
        r.retries
    );
    assert_eq!(r.requests, hotl_engine::STREAM_RETRY_MAX as usize + 1);
}

/// The one shape that must NOT be re-sampled: a sealed `tool_use` block. Its
/// side effects are the model's committed intent, and a second sample would
/// either duplicate them or silently drop them.
#[tokio::test]
async fn a_sealed_tool_use_block_is_not_resampled() {
    let sealed = vec![
        Ok(StreamEvent::Started),
        Ok(StreamEvent::BlockStart {
            index: 0,
            kind: "tool_use".into(),
        }),
        Ok(StreamEvent::BlockEnd { index: 0 }),
    ];
    let r = run(vec![
        ScriptedProvider::error_after(sealed, overloaded()),
        ScriptedProvider::text_reply("never reached"),
    ])
    .await;
    assert!(
        matches!(r.outcome, Outcome::Error { .. }),
        "{:?}",
        r.outcome
    );
    assert!(r.retries.is_empty(), "{:?}", r.retries);
    assert_eq!(r.requests, 1);
}

/// A stream that dies before any byte keeps the old path exactly: the
/// availability ladder (fallback models), not the mid-stream re-sample.
#[tokio::test]
async fn nothing_changes_before_the_first_byte() {
    let r = run(vec![
        vec![Err(overloaded())],
        ScriptedProvider::text_reply("after the fallback"),
    ])
    .await;
    assert!(r.retries.is_empty(), "{:?}", r.retries);
    assert_eq!(r.requests, 1, "no re-sample without a first byte");
    assert!(
        matches!(r.outcome, Outcome::Error { .. }),
        "{:?}",
        r.outcome
    );
}

/// A completed sample still commits exactly one assistant item and one usage
/// entry — the retry path must not double-count what it re-sampled.
#[tokio::test]
async fn a_resampled_turn_bills_only_the_sample_that_completed() {
    let r = run(vec![
        ScriptedProvider::error_after(ScriptedProvider::partial_text("no"), overloaded()),
        ScriptedProvider::text_reply_with_stop("done", StopReason::EndTurn),
    ])
    .await;
    let usages: Vec<TokenUsage> = r
        .log
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::Usage { usage } => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usages.len(), 1, "{usages:?}");
}

/// Ctrl-C during the backoff. A sleep that only raced the timer would hold
/// the session for a full second after the user asked it to stop.
#[tokio::test]
async fn cancel_during_the_backoff_ends_the_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::error_after(ScriptedProvider::partial_text("half"), overloaded()),
        ScriptedProvider::text_reply("never reached"),
    ]));
    let mut handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
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
        initial_goal: None,
        config,
    });
    handle.prompt("go".into()).await;

    let started = std::time::Instant::now();
    let outcome = loop {
        let event = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match event {
            // The notice is emitted before the sleep, so this is inside it.
            EngineEvent::Retrying { .. } => handle.interrupt(),
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(matches!(outcome, Outcome::Cancelled), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "the cancel waited out the backoff: {:?}",
        started.elapsed()
    );
}
