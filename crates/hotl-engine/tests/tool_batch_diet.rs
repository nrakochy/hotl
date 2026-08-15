//! Task 3 (§S1 diet 1): a single-tool batch executes inline in the turn task
//! instead of going through `parallel_chunks()` + `join_all`. The inline path
//! must race the SAME tool-batch `CancellationToken` the (untouched)
//! multi-tool path does: a cancel that lands mid-execution — not just while
//! an ask is pending — must still end the turn `Cancelled`, never `Failed`,
//! and the committed tool-results entry must carry the tool's own
//! cancellation outcome (bash's own cancel race in `bash_impl`), the same
//! shape a multi-tool chunk's cancelled call would commit.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Entry, EntryPayload, Item};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    // Kept alive for the session's lifetime — the log lives in it.
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

/// The persisted conversation items, read straight off the durable log —
/// the same source of truth the golden suite checks.
fn items(log_path: &std::path::Path) -> Vec<Item> {
    std::fs::read_to_string(log_path)
        .expect("read log")
        .lines()
        .map(|l| serde_json::from_str::<Entry>(l).expect("parse entry"))
        .filter_map(|e| match e.payload {
            EntryPayload::Item { item } => Some(item),
            _ => None,
        })
        .collect()
}

/// A cancel that lands while the sole tool in a single-tool batch is
/// actually running (not while its ask is pending): the inline path (diet 1)
/// must still race `execute()`'s own `tool.run(..., self.cancel.clone())`
/// call exactly as the multi-tool path does, so the turn ends `Cancelled`
/// and the tool's own cancellation result — not a generic "not executed"
/// stand-in — is what the log commits.
#[tokio::test]
async fn interrupt_lands_while_the_lone_tool_in_a_single_tool_batch_is_running() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "sleep 5"})),
        ScriptedProvider::text_reply("must never be reached"),
    ]));
    let mut s = session(provider, EngineConfig::default());
    s.handle.prompt("go".into()).await;

    // Approve the ask so the tool actually starts, then interrupt right
    // after it does — the window this test is about is the tool RUNNING,
    // which `interrupt_ends_the_turn_while_an_ask_is_pending` (interrupt.rs)
    // does not cover: that test interrupts before the ask is ever answered.
    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::ToolStart { .. } => break,
            _ => {}
        }
    }
    s.handle.interrupt();

    let outcome = loop {
        if let EngineEvent::TurnDone { outcome, .. } = next_event(&mut s).await {
            break outcome;
        }
    };
    assert_eq!(
        outcome,
        Outcome::Cancelled,
        "Cancelled, never Failed (T1-5)"
    );

    let items = items(&s.log_path);
    let results = items
        .iter()
        .rev()
        .find_map(|i| match i {
            Item::ToolResults { results } => Some(results),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool-results entry committed: {items:#?}"));
    assert_eq!(
        results.len(),
        1,
        "a single-tool batch pairs exactly one result: {results:?}"
    );
    assert_eq!(results[0].tool_use_id, "t1");
    assert!(
        results[0].is_error,
        "a cancelled call must not read as success: {results:?}"
    );
    assert!(
        results[0].content.contains("cancel"),
        "bash's own cancellation message must reach the log unmodified: {:?}",
        results[0].content
    );
}
