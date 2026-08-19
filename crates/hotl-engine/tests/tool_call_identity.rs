//! 0037: tool lifecycle events carry the provider's `tool_use` id. N
//! concurrent same-name calls are indistinguishable by name alone — a surface
//! that settles "the newest `read` card" strands the other N−1 on a spinner
//! forever — so every `ToolStart`/`ToolDone` must name its own call and the
//! committed results must pair per id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Entry, EntryPayload, Item, StopReason, TokenUsage};
use serde_json::{json, Value};

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>) -> Session {
    let config = EngineConfig::default();
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

/// A sample whose assistant reply is one batch of several tool calls.
fn tool_batch(
    calls: &[(&str, &str, Value)],
) -> Vec<Result<StreamEvent, hotl_provider::ProviderError>> {
    let blocks: Vec<Value> = calls
        .iter()
        .map(
            |(id, name, input)| json!({"type": "tool_use", "id": id, "name": name, "input": input}),
        )
        .collect();
    vec![
        Ok(StreamEvent::Started),
        Ok(StreamEvent::Completed {
            stop: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 8,
                ..Default::default()
            },
            blocks,
        }),
    ]
}

fn committed_results(log_path: &std::path::Path) -> Vec<hotl_types::ToolResultItem> {
    std::fs::read_to_string(log_path)
        .expect("read log")
        .lines()
        .map(|l| serde_json::from_str::<Entry>(l).expect("parse entry"))
        .filter_map(|e| match e.payload {
            EntryPayload::Item {
                item: Item::ToolResults { results },
            } => Some(results),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Five distinct paged reads of one file in a single assistant batch — the
/// exact shape the 2026-08-19 screenshot came from. Every call's start and
/// settle must carry its own id, and every id must get exactly one result.
#[tokio::test]
async fn a_parallel_batch_emits_start_and_done_per_call_id() {
    let ids = ["p1", "p2", "p3", "p4", "p5"];
    // `read` resolves relative paths against the process-global workspace
    // root, so the crate's own manifest is an in-workspace (unprompted) read.
    // Distinct offsets keep the five calls distinct — identical calls are
    // D6's dedup territory, not this test's.
    let calls: Vec<(&str, &str, Value)> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                *id,
                "read",
                json!({"path": "Cargo.toml", "offset": 1 + i, "limit": 2}),
            )
        })
        .collect();
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_batch(&calls),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider);
    s.handle.prompt("go".into()).await;

    let mut started: Vec<String> = Vec::new();
    let mut done: HashMap<String, bool> = HashMap::new();
    loop {
        match next_event(&mut s).await {
            EngineEvent::ToolStart { id, name, .. } => {
                assert_eq!(name, "read");
                started.push(id);
            }
            EngineEvent::ToolDone { id, ok, .. } => {
                assert!(
                    done.insert(id.clone(), ok).is_none(),
                    "call {id} settled twice"
                );
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }

    let mut started_sorted = started.clone();
    started_sorted.sort();
    assert_eq!(
        started_sorted, ids,
        "every call must start under its own id"
    );
    for id in ids {
        assert_eq!(
            done.get(id),
            Some(&true),
            "call {id} must settle ok: {done:?}"
        );
    }

    let results = committed_results(&s.log_path);
    let result_ids: Vec<&str> = results.iter().map(|r| r.tool_use_id.as_str()).collect();
    assert_eq!(result_ids, ids, "one result per id, in source order");
    for r in &results {
        assert!(!r.is_error, "{}: {}", r.tool_use_id, r.content);
    }
}
