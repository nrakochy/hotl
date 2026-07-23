//! The tool_use/tool_result adjacency the APIs require: a batch's results must
//! be the very next message after the assistant turn that called them. Steering
//! lands mid-batch by design, which is exactly when that invariant is easiest
//! to break.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{assistant_tool_uses, Item, ToolResultItem};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    _dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, initial_items: Vec<Item>) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
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
        initial_items,
        config,
    });
    Session { handle, _dir: dir }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// The index of a results item that does not sit directly behind the assistant
/// turn that called for it — what the APIs reject.
fn stranded_results(items: &[Item]) -> Option<usize> {
    items.iter().enumerate().position(|(i, item)| {
        matches!(item, Item::ToolResults { .. })
            && !matches!(
                i.checked_sub(1).and_then(|prev| items.get(prev)),
                Some(Item::Assistant { .. })
            )
    })
}

fn assistant(id: &str) -> Item {
    Item::Assistant {
        blocks: vec![json!({"type": "tool_use", "id": id, "name": "read", "input": {}})],
    }
}

fn results(id: &str) -> Item {
    Item::ToolResults {
        results: vec![ToolResultItem {
            tool_use_id: id.into(),
            content: "ok".into(),
            is_error: false,
        }],
    }
}

/// The reported bug: steering while a tool batch runs used to append the steer
/// between the assistant turn and its results, so the message carrying the
/// results was no longer the turn right after the calls — more tool_result
/// blocks than the preceding turn had tool_use blocks.
#[tokio::test]
async fn a_steer_mid_batch_lands_after_the_results_not_before() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, Vec::new());
    s.handle.prompt("go".into()).await;

    // The permission ask is a deterministic mid-batch pause: the assistant item
    // is committed, the results are not. Steer in exactly that window.
    let reply = loop {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            break reply;
        }
    };
    s.handle.steer("actually, be brief".into()).await;
    let _ = reply.send(AskReply::Allow);

    loop {
        if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
            break;
        }
    }

    let seen = provider.requests();
    assert!(seen.len() >= 2, "the turn should have sampled again");
    let items = &seen[seen.len() - 1].items;
    assert_eq!(
        stranded_results(items),
        None,
        "tool results must follow their assistant turn: {items:#?}"
    );
    // The steer still has to reach the model — placed, not dropped.
    assert!(
        items.iter().any(|i| matches!(
            i,
            Item::User { text, .. } if text.contains("actually, be brief")
        )),
        "the steer must still be in the projection: {items:#?}"
    );
}

/// Sessions already written with the broken order must not keep failing on
/// resume — the projection is repaired as it loads.
#[tokio::test]
async fn resumed_history_repairs_results_stranded_by_an_older_build() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let broken = vec![
        Item::User {
            text: "start".into(),
            synthetic: None,
        },
        assistant("t1"),
        Item::User {
            text: "a steer that landed in the gap".into(),
            synthetic: Some(hotl_types::SyntheticReason::Steer),
        },
        results("t1"),
    ];
    assert_eq!(
        stranded_results(&broken),
        Some(3),
        "fixture must reproduce the bad order"
    );

    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, broken);
    s.handle.prompt("continue".into()).await;
    loop {
        if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
            break;
        }
    }

    let items = &provider.last_request().expect("a request").items;
    assert_eq!(
        stranded_results(items),
        None,
        "resume must repair the stranded results: {items:#?}"
    );
    assert!(
        items.iter().any(|i| matches!(
            i,
            Item::User { text, .. } if text.contains("a steer that landed in the gap")
        )),
        "repair must move the steer, never drop it: {items:#?}"
    );
}

/// Every assistant turn that called tools is answered by exactly one results
/// item, holding exactly one result per call — the count the APIs compare.
#[tokio::test]
async fn every_call_is_answered_once() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(Arc::clone(&provider) as Arc<dyn Provider>, Vec::new());
    s.handle.prompt("go".into()).await;
    let reply = loop {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            break reply;
        }
    };
    s.handle.steer("one".into()).await;
    s.handle.steer("two".into()).await;
    let _ = reply.send(AskReply::Allow);
    loop {
        if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
            break;
        }
    }

    let items = &provider.last_request().expect("a request").items;
    for (i, item) in items.iter().enumerate() {
        let Item::ToolResults { results } = item else {
            continue;
        };
        let Some(Item::Assistant { blocks }) = i.checked_sub(1).and_then(|p| items.get(p)) else {
            panic!("results at {i} have no assistant turn: {items:#?}");
        };
        assert_eq!(
            results.len(),
            assistant_tool_uses(blocks).len(),
            "one result per call, no more: {items:#?}"
        );
    }
}
