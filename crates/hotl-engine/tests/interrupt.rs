//! Interrupt delivery in the windows that used to swallow it: a pending
//! permission ask, the inline compaction summarize, and the actor's own
//! lifetime once every handle is gone.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, config: EngineConfig) -> Session {
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
        hooks: None,
        initial_items: Vec::new(),
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

#[tokio::test]
async fn interrupt_ends_the_turn_while_an_ask_is_pending() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::tool_call(
        "t1",
        "bash",
        json!({"command": "echo hi"}),
    )]));
    let mut s = session(provider, EngineConfig::default());
    s.handle.prompt("go".into()).await;

    // Reach the ask and *hold* its reply channel open — the interrupt alone
    // must end the turn, not a dropped sender reading as a deny.
    let held_reply = loop {
        if let EngineEvent::Ask { reply, .. } = next_event(&mut s).await {
            break reply;
        }
    };
    s.handle.interrupt();
    let outcome = loop {
        if let EngineEvent::TurnDone { outcome, .. } = next_event(&mut s).await {
            break outcome;
        }
    };
    assert_eq!(outcome, Outcome::Cancelled);
    drop(held_reply);
}

/// Routes compaction summarize requests into a stream that never completes;
/// everything else goes to the scripted main provider.
struct HangingSummarize {
    main: Arc<ScriptedProvider>,
}

impl Provider for HangingSummarize {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        if req.system.contains("compress") {
            Box::pin(futures_util::stream::pending())
        } else {
            self.main.stream(req)
        }
    }
}

#[tokio::test]
async fn interrupt_lands_during_the_inline_compaction_summarize() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let provider = Arc::new(HangingSummarize {
        main: Arc::clone(&main),
    });
    // Reset-mode compaction never speculates, so the fold's summarize runs
    // inline in the actor — the window that used to swallow interrupts.
    let mut s = session(
        provider,
        EngineConfig {
            context_window: 1000,
            max_turns: 10,
            compaction_reset: true,
            ..Default::default()
        },
    );
    let file = s.dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    let with_usage = |id: &str, input_tokens: u64| {
        let mut script = ScriptedProvider::tool_call(id, "read", json!({"path": path}));
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.input_tokens = input_tokens;
        }
        script
    };
    // Walk the estimate over the 80% trigger: the second sample's report puts
    // the next request past it, ending the turn in a compaction request.
    main.push_script(with_usage("t1", 650));
    main.push_script(with_usage("t2", 850));
    main.push_script(ScriptedProvider::text_reply("must never be reached"));

    s.handle.prompt("start the long task".into()).await;
    let mut tools_done = 0;
    while tools_done < 2 {
        if let EngineEvent::ToolDone { .. } = next_event(&mut s).await {
            tools_done += 1;
        }
    }
    // Let the turn finish and the actor park in the hanging summarize, then
    // interrupt: the compaction window must honor it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    s.handle.interrupt();
    let outcome = loop {
        if let EngineEvent::TurnDone { outcome, .. } = next_event(&mut s).await {
            break outcome;
        }
    };
    assert_eq!(outcome, Outcome::Cancelled);
    assert!(
        main.request_count() <= 2,
        "the continuation must not sample after an interrupt"
    );
}

#[tokio::test]
async fn actor_exits_once_the_handle_drops() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut s = session(provider, EngineConfig::default());
    // Detach the event receiver (as the surfaces do), then drop the handle:
    // the actor's weak command sender must let the channel close and the
    // task exit — observed as the event stream ending — instead of leaking.
    let mut events = std::mem::replace(&mut s.handle.events, tokio::sync::mpsc::channel(1).1);
    drop(s.handle);
    let closed = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("actor task never exited after the handle dropped");
    assert!(closed.is_none(), "no events expected, just channel close");
}
