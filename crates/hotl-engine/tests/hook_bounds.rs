//! T2-12: the two per-tool hook events reach the engine only through
//! `hooks::call_pre_tool` / `call_post_tool`, which bound them by
//! `HOOK_CALL_TIMEOUT` and race them against the turn's cancellation. Awaited
//! raw, a hung `pre_tool` wedged the turn *and* swallowed the interrupt.
//!
//! The wall-clock bound itself is unit-tested in `hooks.rs` (a paused clock can
//! no longer drive the actor's write path — 0011's standing constraint). What
//! is proven here is the part that needs a real session: the interrupt lands,
//! and the capped hook view never reaches the tool.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::hooks::{Hooks, InProcessHooks, PreToolDecision, HOOK_PAYLOAD_CAP};
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::{json, Value};

struct Session {
    handle: SessionHandle,
    /// Kept alive for the session's lifetime — the log lives in it.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, hooks: Arc<dyn Hooks>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
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
        hooks: Some(hooks),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
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

struct HangingPreTool;

impl Hooks for HangingPreTool {
    fn pre_tool<'a>(&'a self, _n: &'a str, _i: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        Box::pin(std::future::pending())
    }
    fn post_tool<'a>(&'a self, _n: &'a str, _r: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(std::future::ready(None))
    }
}

/// A hung `pre_tool` must not swallow the interrupt while it hangs.
#[tokio::test]
async fn an_interrupt_lands_while_a_pre_tool_hook_hangs() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("must never be reached"),
    ]));
    let mut s = session(provider, Arc::new(HangingPreTool), EngineConfig::default());

    s.handle.prompt("go".into()).await;
    // The gate runs before any ToolStart event, so there is nothing to wait on
    // but the settle: the turn is parked inside the hook by now.
    tokio::time::sleep(Duration::from_millis(200)).await;
    s.handle.interrupt();
    let outcome = loop {
        if let EngineEvent::TurnDone { outcome, .. } = next_event(&mut s).await {
            break outcome;
        }
    };
    assert_eq!(outcome, Outcome::Cancelled);
}

/// T2-12b: the hook sees a capped view, the tool runs on the full input.
#[tokio::test]
async fn the_hook_sees_a_capped_input_but_the_tool_gets_the_full_one() {
    let seen: Arc<Mutex<Option<usize>>> = Arc::default();
    let recorder = {
        let seen = Arc::clone(&seen);
        InProcessHooks::new().on_pre_tool(move |_name, input| {
            if let Some(content) = input.get("content").and_then(Value::as_str) {
                *seen.lock().expect("lock") = Some(content.len());
            }
            PreToolDecision::Continue
        })
    };
    let body = "z".repeat(5_000);
    let dir_probe = tempfile::tempdir().expect("tempdir");
    let target = dir_probe.path().join("written.txt");
    let path = target.to_str().expect("utf8 path").to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": path, "content": body.clone()}),
        ),
        ScriptedProvider::text_reply("wrote it"),
    ]));
    let mut s = session(provider, Arc::new(recorder), EngineConfig::default());

    s.handle.prompt("write the file".into()).await;
    let outcome = loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "wrote it".into()
        }
    );

    let recorded = seen.lock().expect("lock").expect("the hook must have run");
    assert!(
        recorded <= HOOK_PAYLOAD_CAP + 64,
        "hook saw an uncapped payload: {recorded} bytes"
    );
    assert_eq!(
        std::fs::read_to_string(&target)
            .expect("the file must exist")
            .len(),
        5_000,
        "capping must not truncate the write"
    );
}
