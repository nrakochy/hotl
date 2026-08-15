//! T1-5: a turn task that panics must not wedge the session. The actor's
//! `running` flag, the prompt queue, and every surface depend on exactly one
//! `TurnFinished` per spawned turn — a discarded `JoinHandle` cannot deliver
//! that, so the handle is supervised and a `JoinError` becomes a synthetic
//! `Outcome::Error`.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session_with(
    provider: Arc<dyn Provider>,
    registry: Arc<Registry>,
    config: EngineConfig,
) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        provider,
        registry,
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
    Session { handle, dir }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

async fn wait_outcome(s: &mut Session) -> Outcome {
    loop {
        if let EngineEvent::TurnDone { outcome, .. } = next_event(s).await {
            return outcome;
        }
    }
}

struct PanicTool;

impl hotl_tools::Tool for PanicTool {
    fn name(&self) -> &'static str {
        "boom"
    }
    fn description(&self) -> &str {
        "test-only: panics"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    fn permission(&self, _input: &serde_json::Value) -> hotl_tools::Permission {
        hotl_tools::Permission::None
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
        Box::pin(async { panic!("tool panicked on purpose") })
    }
}

/// The panic backtrace this prints on success is tokio's default hook, not a
/// failure — do not "fix" it by removing the panic. Tests build under the dev
/// profile, which unwinds; a future `panic = "abort"` profile would need this
/// gated, never deleted.
#[tokio::test]
async fn a_panicking_turn_reports_an_error_and_the_session_keeps_working() {
    let mut registry = Registry::builtin();
    registry.register(Box::new(PanicTool));
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "boom", json!({})),
        ScriptedProvider::text_reply("still alive"),
    ]));
    let mut s = session_with(provider, Arc::new(registry), EngineConfig::default());

    s.handle.prompt("trip the panic".into()).await;
    let first = wait_outcome(&mut s).await;
    assert!(
        matches!(first, Outcome::Error { .. }),
        "a panicked turn must report an error outcome, got {first:?}"
    );

    // The wedge test: `running` must have been cleared, so a second prompt
    // starts a turn instead of queueing behind a turn that never ended.
    s.handle.prompt("are you there".into()).await;
    let second = wait_outcome(&mut s).await;
    assert_eq!(
        second,
        Outcome::Done {
            text: "still alive".into()
        }
    );
}
