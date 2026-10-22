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
    /// `(max, delay_ms, status)` per retry (0061 T15).
    reported: Vec<(u32, u64, Option<u16>)>,
    /// The request id each retry frame carried (0071 T1).
    retry_ids: Vec<Option<String>>,
    text: String,
    log: String,
    requests: usize,
}

fn overloaded() -> ProviderError {
    ProviderError::Http {
        status: 529,
        message: "overloaded_error".into(),
        retry_after: None,
        request_id: None,
    }
}

async fn run(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Ran {
    run_with(scripts, EngineConfig::default()).await
}

async fn run_with(
    scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
    config: EngineConfig,
) -> Ran {
    let dir = tempfile::tempdir().expect("tempdir");
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
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        config,
    });
    handle.prompt("go".into()).await;

    let mut retries = Vec::new();
    let mut reported = Vec::new();
    let mut retry_ids = Vec::new();
    let mut text = String::new();
    let outcome = loop {
        let event = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match event {
            EngineEvent::Retrying {
                attempt,
                max,
                reason,
                delay_ms,
                status,
                request_id,
                discarded_partial,
                ..
            } => {
                retries.push((attempt, reason, discarded_partial));
                reported.push((max, delay_ms, status));
                retry_ids.push(request_id);
            }
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
        reported,
        retry_ids,
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

/// 0061 T15: nothing is computing during a backoff, so the countdown is the
/// only liveness there is — and it has to be the sleep actually taken, not
/// the un-jittered base.
#[tokio::test]
async fn a_mid_stream_retry_reports_status_and_the_delay_it_will_sleep() {
    let r = run(vec![
        ScriptedProvider::error_after(ScriptedProvider::partial_text("half a th"), overloaded()),
        ScriptedProvider::text_reply("the whole answer"),
    ])
    .await;
    assert_eq!(r.reported.len(), 1, "{:?}", r.reported);
    let (max, delay_ms, status) = r.reported[0];
    assert_eq!(status, Some(529), "the status is what the strip shows");
    assert!(max >= 1, "the ceiling rides the frame: {max}");
    assert!(
        (500..=1000).contains(&delay_ms),
        "full jitter of the 1s base: {delay_ms}"
    );
}

/// 0071 T1: the id the provider sent on `Started` lands in the sample's
/// `Usage` entry, so a log line can be matched to a support ticket.
#[tokio::test]
async fn a_sample_usage_entry_records_the_request_id() {
    let mut script = ScriptedProvider::text_reply("hi");
    script[0] = Ok(StreamEvent::Started {
        request_id: Some("req_1".into()),
    });
    let r = run(vec![script]).await;
    assert!(matches!(r.outcome, Outcome::Done { .. }));
    let ids: Vec<Option<String>> = r
        .log
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::Usage { request_id, .. } => Some(request_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![Some("req_1".into())]);
}

/// 0071 T1: the retry frame names the attempt that died, not the one about
/// to be sent; a provider that sent no id writes no key.
#[tokio::test]
async fn a_retry_frame_carries_the_request_id_of_the_attempt_that_died() {
    let mut dead = ScriptedProvider::partial_text("half a th");
    dead[0] = Ok(StreamEvent::Started {
        request_id: Some("req_dead".into()),
    });
    let r = run(vec![
        ScriptedProvider::error_after(dead, overloaded()),
        ScriptedProvider::text_reply("the whole answer"),
    ])
    .await;
    assert_eq!(r.retry_ids, vec![Some("req_dead".to_string())]);
    let r = run(vec![
        ScriptedProvider::error_after(ScriptedProvider::partial_text("half"), overloaded()),
        ScriptedProvider::text_reply("the whole answer"),
    ])
    .await;
    assert_eq!(r.retry_ids, vec![None]);
}

fn stall_config() -> EngineConfig {
    EngineConfig {
        thinking_stall: Duration::from_millis(50),
        ..EngineConfig::default()
    }
}

/// 0071 T2: a thinking block that goes quiet while the socket stays open is
/// dropped and re-sampled — the idle watchdog would never see it.
#[tokio::test]
async fn a_stalled_thinking_block_is_retried_before_the_idle_timeout() {
    let r = run_with(
        vec![
            ScriptedProvider::hang_after(ScriptedProvider::partial_thinking("let me think")),
            ScriptedProvider::text_reply("the answer"),
        ],
        stall_config(),
    )
    .await;
    assert!(matches!(r.outcome, Outcome::Done { .. }), "{:?}", r.outcome);
    assert_eq!(r.retries.len(), 1, "{:?}", r.retries);
    let (attempt, reason, discarded) = &r.retries[0];
    assert_eq!(*attempt, 1);
    assert_eq!(reason, "thinking block stalled for 0s");
    assert!(
        !discarded,
        "streamed reasoning stays on screen; no answer was rendered"
    );
    assert_eq!(r.requests, 2);
    assert_eq!(r.text, "the answer");
}

#[tokio::test]
async fn a_stalled_thinking_block_past_the_ceiling_ends_the_turn() {
    let scripts = (0..8)
        .map(|_| ScriptedProvider::hang_after(ScriptedProvider::partial_thinking("…")))
        .collect();
    let r = run_with(scripts, stall_config()).await;
    let Outcome::Error { message } = &r.outcome else {
        panic!("{:?}", r.outcome);
    };
    assert!(message.contains("thinking block stalled"), "{message}");
    assert_eq!(r.retries.len(), hotl_engine::STREAM_RETRY_MAX as usize);
    assert_eq!(r.requests, hotl_engine::STREAM_RETRY_MAX as usize + 1);
}

#[tokio::test]
async fn a_sealed_tool_use_vetoes_the_thinking_stall_retry() {
    let mut script = vec![
        Ok(StreamEvent::Started { request_id: None }),
        Ok(StreamEvent::BlockStart {
            index: 0,
            kind: "tool_use".into(),
        }),
        Ok(StreamEvent::BlockEnd { index: 0 }),
        Ok(StreamEvent::BlockStart {
            index: 1,
            kind: "thinking".into(),
        }),
    ];
    script = ScriptedProvider::hang_after(script);
    let r = run_with(
        vec![script, ScriptedProvider::text_reply("never reached")],
        stall_config(),
    )
    .await;
    assert!(
        matches!(r.outcome, Outcome::Error { .. }),
        "{:?}",
        r.outcome
    );
    assert!(r.retries.is_empty(), "{:?}", r.retries);
    assert_eq!(r.requests, 1);
}

/// A text block that goes quiet after the thinking block closed is the idle
/// watchdog's business, not this one's: the stream hangs until the test's
/// own bound, and no stall retry fires.
#[tokio::test]
async fn the_thinking_timer_is_disarmed_at_block_end() {
    let mut script = ScriptedProvider::partial_thinking("done thinking");
    script.push(Ok(StreamEvent::BlockEnd { index: 0 }));
    script.push(Ok(StreamEvent::BlockStart {
        index: 1,
        kind: "text".into(),
    }));
    script.push(Ok(StreamEvent::TextDelta {
        index: 1,
        text: "half".into(),
    }));
    let script = ScriptedProvider::hang_after(script);
    let provider = Arc::new(ScriptedProvider::new(vec![script]));
    let dir = tempfile::tempdir().expect("tempdir");
    let config = stall_config();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
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
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        config,
    });
    handle.prompt("go".into()).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    loop {
        let next = tokio::time::timeout_at(deadline, handle.events.recv()).await;
        match next {
            Ok(Some(EngineEvent::Retrying { reason, .. })) => {
                panic!("the stall timer fired after its block closed: {reason}")
            }
            Ok(Some(EngineEvent::TurnDone { outcome, .. })) => {
                panic!("the turn ended: {outcome:?}")
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("event channel closed"),
            Err(_) => break, // still hanging on the text block, as it should
        }
    }
    assert_eq!(provider.request_count(), 1);
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
        Ok(StreamEvent::Started { request_id: None }),
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
            EntryPayload::Usage {
                usage,
                request_id: None,
            } => Some(usage),
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
        initial_decisions: Vec::new(),
        plan_files: None,
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
