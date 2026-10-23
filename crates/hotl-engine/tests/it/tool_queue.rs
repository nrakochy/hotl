//! 0061 T17: the `subprocs` permit was the one wait in the whole turn path
//! that emitted nothing — a queued `bash` behind a saturated budget looked
//! exactly like a hung session. It now announces itself before it waits, and
//! `ToolStart` still marks the real start.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::concurrency::{ConcurrencyLimits, SessionConcurrency};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, concurrency: SessionConcurrency) -> Session {
    let config = EngineConfig::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        concurrency,
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
    Session { handle, dir }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

fn one_bash() -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("done"),
    ]))
}

/// One permit, held by the test: the call must say it is queued, then wait —
/// and only start once the permit is free.
#[tokio::test]
async fn a_held_subproc_permit_makes_the_next_bash_call_announce_queued_before_start() {
    let concurrency = SessionConcurrency::new(ConcurrencyLimits {
        agents: 1,
        requests: 1,
        subprocs: 1,
    });
    let held = concurrency.subproc().await;
    let mut s = session(one_bash(), concurrency.clone());
    s.handle.prompt("go".into()).await;

    // Allow the ask, then look for the queue notice.
    let queued = loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::ToolQueued { id, ahead, .. } => break (id, ahead),
            EngineEvent::ToolStart { .. } => panic!("the call started under a held permit"),
            _ => {}
        }
    };
    assert_eq!(queued, ("t1".to_string(), 0));

    // Nothing else moves while the permit is held.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), s.handle.events.recv())
            .await
            .is_err(),
        "the call ran without a permit"
    );

    drop(held);
    let mut started = false;
    loop {
        match next_event(&mut s).await {
            EngineEvent::ToolStart { id, .. } => {
                assert_eq!(id, "t1");
                started = true;
            }
            EngineEvent::ToolDone { id, .. } => {
                assert_eq!(id, "t1");
                assert!(started, "the settle came before the start");
                break;
            }
            _ => {}
        }
    }
}

/// The common case emits nothing new: an uncontended call is not a wait.
#[tokio::test]
async fn an_uncontended_call_emits_no_tool_queued() {
    let mut s = session(one_bash(), Default::default());
    s.handle.prompt("go".into()).await;
    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::ToolQueued { .. } => panic!("a free permit announced a queue"),
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
}

/// The permit now lives past the gate, so a denial takes none — it used to
/// draw one and never run.
#[tokio::test]
async fn a_denied_call_takes_no_permit() {
    let concurrency = SessionConcurrency::new(ConcurrencyLimits {
        agents: 1,
        requests: 1,
        subprocs: 1,
    });
    let _held = concurrency.subproc().await;
    let mut s = session(one_bash(), concurrency.clone());
    s.handle.prompt("go".into()).await;

    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match next_event(&mut s).await {
                EngineEvent::Ask { reply, .. } => {
                    let _ = reply.send(AskReply::Deny { message: None });
                }
                EngineEvent::ToolDenied { id, .. } => return id,
                EngineEvent::ToolQueued { .. } => panic!("a denial waited for a permit"),
                _ => {}
            }
        }
    })
    .await
    .expect("a denied call must settle without the permit");
    assert_eq!(settled, "t1");
}

/// A tool that blocks on a nested session takes no leaf permit — a parent
/// holding one across its child's own calls deadlocks a small budget.
#[tokio::test]
async fn a_spawn_class_tool_never_queues() {
    let concurrency = SessionConcurrency::new(ConcurrencyLimits {
        agents: 1,
        requests: 1,
        subprocs: 1,
    });
    let _held = concurrency.subproc().await;
    // `Registry::builtin()` has no spawn tool, so assert the property that
    // decides it rather than driving a child session here.
    for name in ["bash", "read", "grep"] {
        let tool = Registry::builtin()
            .get(name)
            .map(|t| t.awaits_child_session());
        assert_eq!(tool, Some(false), "{name} must draw a leaf permit");
    }
}
