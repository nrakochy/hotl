//! §S1 HookRouter gate (Task 5): a session whose `Hooks` impl only registers
//! a subset of events must skip the wrapper entirely for every other event —
//! no payload construction, no hook call at all — while the registered
//! event still fires normally. Proven here by a hooks impl that reports a
//! narrow `event_mask()` and records every method it's actually called on.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::hooks::{EventMask, Hooks, NotificationKind, PreToolDecision, StopDecision};
use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::{json, Value};

/// Records every `Hooks` method actually invoked, and reports a fixed
/// `event_mask()` regardless — the engine, not this impl, is what must skip
/// the masked-off calls.
#[derive(Default)]
struct RecordingHooks {
    mask: EventMask,
    pre_tool: AtomicU32,
    post_tool: AtomicU32,
    user_prompt: AtomicU32,
    notification: AtomicU32,
    stop: AtomicU32,
    session_end: AtomicU32,
}

impl RecordingHooks {
    fn new(mask: EventMask) -> Self {
        Self {
            mask,
            ..Default::default()
        }
    }
}

impl Hooks for RecordingHooks {
    fn pre_tool<'a>(&'a self, _n: &'a str, _i: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        self.pre_tool.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(PreToolDecision::Continue))
    }
    fn post_tool<'a>(&'a self, _n: &'a str, _r: &'a str) -> BoxFuture<'a, Option<String>> {
        self.post_tool.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(None))
    }
    fn on_user_prompt<'a>(&'a self, _prompt: &'a str) -> BoxFuture<'a, Option<String>> {
        self.user_prompt.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(None))
    }
    fn on_notification<'a>(
        &'a self,
        _kind: NotificationKind,
        _detail: &'a str,
    ) -> BoxFuture<'a, ()> {
        self.notification.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }
    fn on_stop<'a>(&'a self, _outcome: &'a str) -> BoxFuture<'a, StopDecision> {
        self.stop.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(StopDecision::Allow))
    }
    fn on_session_end<'a>(&'a self) -> BoxFuture<'a, ()> {
        self.session_end.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }
    fn event_mask(&self) -> EventMask {
        self.mask
    }
}

struct Session {
    handle: hotl_engine::SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, hooks: Arc<dyn Hooks>) -> Session {
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
        hooks: Some(hooks),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });
    Session { handle, dir }
}

/// Drive a session to `TurnDone`, auto-allowing any ask along the way.
async fn run_to_done(s: &mut Session) -> Outcome {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => return outcome,
            _ => {}
        }
    }
}

/// A session with hooks registered ONLY for `pre_tool`: post_tool, stop,
/// user_prompt, notification, and session_end must all take the masked
/// branch (recorded call count stays zero) while pre_tool still fires.
#[tokio::test]
async fn only_the_registered_event_dispatches_the_rest_take_the_masked_branch() {
    let hooks = Arc::new(RecordingHooks::new(EventMask::PRE_TOOL));
    let write_dir = tempfile::tempdir().expect("tempdir");
    let target = write_dir.path().join("out.txt");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": target.to_str().unwrap(), "content": "hi"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider, hooks.clone());

    s.handle.prompt("go".into()).await;
    let outcome = run_to_done(&mut s).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    s.handle.finish(Duration::from_secs(5)).await;

    assert!(
        hooks.pre_tool.load(Ordering::SeqCst) >= 1,
        "the registered event must still fire"
    );
    assert_eq!(
        hooks.post_tool.load(Ordering::SeqCst),
        0,
        "post_tool is masked off — it must never be called"
    );
    assert_eq!(
        hooks.user_prompt.load(Ordering::SeqCst),
        0,
        "user_prompt is masked off — it must never be called"
    );
    assert_eq!(
        hooks.notification.load(Ordering::SeqCst),
        0,
        "notification is masked off — it must never be called"
    );
    assert_eq!(
        hooks.stop.load(Ordering::SeqCst),
        0,
        "stop is masked off — it must never be called"
    );
    assert_eq!(
        hooks.session_end.load(Ordering::SeqCst),
        0,
        "session_end is masked off — it must never be called"
    );
}

/// A `Hooks` impl that exposes a LIVE mask handle (the fix-1 mechanism) and
/// narrows its own `PRE_TOOL` bit after its third call — mirroring what
/// `ShellHooks::refresh_mask_bit` does internally after a real three-strike
/// eviction, but from an impl the engine has no special knowledge of.
struct SelfEvictingHooks {
    mask: Arc<AtomicU8>,
    pre_tool_calls: AtomicU32,
}

impl SelfEvictingHooks {
    fn new() -> Self {
        Self {
            mask: Arc::new(AtomicU8::new(EventMask::PRE_TOOL.bits())),
            pre_tool_calls: AtomicU32::new(0),
        }
    }
}

impl Hooks for SelfEvictingHooks {
    fn pre_tool<'a>(&'a self, _n: &'a str, _i: &'a Value) -> BoxFuture<'a, PreToolDecision> {
        let n = self.pre_tool_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 3 {
            self.mask
                .fetch_and(!EventMask::PRE_TOOL.bits(), Ordering::SeqCst);
        }
        Box::pin(std::future::ready(PreToolDecision::Continue))
    }
    fn post_tool<'a>(&'a self, _n: &'a str, _r: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(std::future::ready(None))
    }
    fn mask_handle(&self) -> Option<Arc<AtomicU8>> {
        Some(Arc::clone(&self.mask))
    }
}

/// Fix 1 (reviewer finding): the engine's gate must read the SAME atomic a
/// live-handle-exposing impl narrows — not a disconnected snapshot taken
/// once at session start. Four sequential `glob` calls (permission-free, so
/// no `Ask` interrupts the sequence) in one turn; the third call narrows the
/// impl's own mask, so the fourth must never reach `pre_tool` at all.
#[tokio::test]
async fn mid_session_narrowing_through_a_live_mask_handle_reaches_the_engine() {
    let hooks = Arc::new(SelfEvictingHooks::new());
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "glob", json!({"pattern": "*"})),
        ScriptedProvider::tool_call("t2", "glob", json!({"pattern": "*"})),
        ScriptedProvider::tool_call("t3", "glob", json!({"pattern": "*"})),
        ScriptedProvider::tool_call("t4", "glob", json!({"pattern": "*"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider, hooks.clone());

    s.handle.prompt("go".into()).await;
    let outcome = run_to_done(&mut s).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");

    assert_eq!(
        hooks.pre_tool_calls.load(Ordering::SeqCst),
        3,
        "the engine must stop calling pre_tool the instant the impl's own \
         live mask handle narrows mid-session — a 4th call means the engine \
         is reading a stale session-start snapshot instead of the shared handle"
    );
}

/// Regression guard: a session with no hooks at all behaves end-to-end
/// exactly as before this task — no masked-branch bookkeeping changes what a
/// zero-hook session does.
#[tokio::test]
async fn a_zero_hook_session_is_unaffected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "done",
    )]));
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });
    handle.prompt("go".into()).await;
    let outcome = loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { outcome, .. } = ev {
            break outcome;
        }
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
}
