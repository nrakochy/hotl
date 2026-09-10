//! 0061 T13: a running bash call reports its own output while it runs. The
//! frames must pair with their call by id and land before its `ToolDone` —
//! a surface that cannot pair them settles the wrong card.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

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
    Session {
        handle,
        log_path,
        dir,
    }
}

/// Every event of a turn, with asks allowed.
async fn drain(s: &mut Session) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => return seen,
            other => seen.push(other),
        }
    }
}

fn log_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .expect("read log")
        .lines()
        .count()
}

#[tokio::test]
#[cfg(unix)]
async fn bash_progress_frames_carry_the_calls_id_and_all_precede_its_tool_done() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "printf 'a\nb\nc\n'"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider);
    s.handle.prompt("go".into()).await;
    let seen = drain(&mut s).await;

    let progress: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            EngineEvent::ToolProgress { id, name, .. } => {
                assert_eq!(id, "t1", "a frame that cannot be paired strands a card");
                assert_eq!(name, "bash");
                Some(i)
            }
            _ => None,
        })
        .collect();
    assert!(!progress.is_empty(), "no liveness at all: {seen:?}");
    let done = seen
        .iter()
        .position(|e| matches!(e, EngineEvent::ToolDone { .. }))
        .expect("the call settled");
    assert!(
        progress.iter().all(|i| *i < done),
        "a frame after the settle re-opens a finished card: {seen:?}"
    );
    let last = seen
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ToolProgress { tail, lines, .. } => Some((tail.clone(), *lines)),
            _ => None,
        })
        .next_back()
        .expect("a frame");
    assert_eq!(last, ("c".to_string(), 3));
}

/// Only bash has a sink; every other tool's silence means nothing, so it must
/// not pretend otherwise.
#[tokio::test]
async fn a_non_bash_tool_emits_no_progress() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "read", json!({"path": "Cargo.toml", "limit": 5})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider);
    s.handle.prompt("go".into()).await;
    let seen = drain(&mut s).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolProgress { .. })),
        "{seen:?}"
    );
}

/// The `ChildTool` precedent: liveness is a view of work in flight, never a
/// record. A session that printed a lot must log exactly what a quiet one did.
#[tokio::test]
#[cfg(unix)]
async fn tool_progress_is_never_a_log_entry() {
    let script = |command: &str| {
        vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": command})),
            ScriptedProvider::text_reply("done"),
        ]
    };
    let mut chatty = session(Arc::new(ScriptedProvider::new(script(
        "printf 'a\nb\nc\nd\ne\n'",
    ))));
    chatty.handle.prompt("go".into()).await;
    drain(&mut chatty).await;

    let mut quiet = session(Arc::new(ScriptedProvider::new(script("true"))));
    quiet.handle.prompt("go".into()).await;
    drain(&mut quiet).await;

    assert_eq!(
        log_lines(&chatty.log_path),
        log_lines(&quiet.log_path),
        "progress leaked into the session log"
    );
}
