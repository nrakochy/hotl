//! T3-1: a denial is a decision, not a tool malfunction — it must not draw
//! down the per-tool failure budget, and it must not carry a
//! `<retry attempts_left>` hint that contradicts its own message.
//! T3-2: whether a batch is mutating is `Tool::read_only()`'s answer, not
//! `name != "read"` — a `glob`/`grep` batch takes no shadow snapshot.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
    Snapshotter,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

/// Records every shadow-snapshot label the engine asks for.
#[derive(Default)]
struct RecordingSnapshotter(Arc<Mutex<Vec<String>>>);

impl Snapshotter for RecordingSnapshotter {
    fn snapshot(&self, label: String) -> BoxFuture<'static, ()> {
        self.0.lock().expect("lock").push(label);
        Box::pin(std::future::ready(()))
    }
}

struct Session {
    handle: SessionHandle,
    /// Kept alive for the session's lifetime — the log lives in it.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(
    provider: Arc<dyn Provider>,
    snapshots: Option<Arc<dyn Snapshotter>>,
    config: EngineConfig,
) -> Session {
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
        snapshots,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
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

/// Drive to the terminal outcome, answering every ask with `answer`.
async fn run_answering(s: &mut Session, answer: impl Fn() -> AskReply) -> Outcome {
    loop {
        match next_event(s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(answer());
            }
            EngineEvent::TurnDone { outcome, .. } => return outcome,
            _ => {}
        }
    }
}

/// T3-1: six consecutive user denials must not end the turn with
/// `ToolFailureBudget` (default budget 5). A denial is a decision, not a tool
/// malfunction, and it is not retryable.
#[tokio::test]
async fn user_denials_are_not_charged_to_the_tool_failure_budget() {
    let scripts: Vec<_> = (0..6)
        .map(|i| {
            ScriptedProvider::tool_call(
                &format!("t{i}"),
                "bash",
                json!({"command": format!("echo {i}")}),
            )
        })
        .chain(std::iter::once(ScriptedProvider::text_reply(
            "gave up on bash",
        )))
        .collect();
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut s = session(
        provider,
        None,
        EngineConfig {
            max_turns: 20,
            ..Default::default()
        },
    );

    s.handle.prompt("try bash a lot".into()).await;
    let outcome = run_answering(&mut s, || AskReply::Deny { message: None }).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "gave up on bash".into()
        }
    );
}

/// T3-2: a batch of read-only tools takes no shadow snapshot.
#[tokio::test]
async fn a_read_only_batch_takes_no_shadow_snapshot() {
    let labels = Arc::<Mutex<Vec<String>>>::default();
    let snapshotter = Arc::new(RecordingSnapshotter(Arc::clone(&labels)));
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "glob", json!({"pattern": "*.txt"})),
        ScriptedProvider::text_reply("nothing to change"),
    ]));
    let mut s = session(provider, Some(snapshotter), EngineConfig::default());

    s.handle.prompt("look around".into()).await;
    let outcome = run_answering(&mut s, || AskReply::Allow).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "nothing to change".into()
        }
    );
    assert!(
        labels.lock().expect("lock").is_empty(),
        "read-only batch snapshotted: {:?}",
        labels.lock().expect("lock")
    );
}

/// T3-9: `DontAsk` is an unattended posture. The doom guard is a malfunction
/// brake, not a permission — it must stop the turn, never emit an `Ask` that
/// nobody is there to answer.
#[tokio::test]
async fn dont_ask_mode_hard_stops_on_a_doom_loop() {
    // The same call three times over: `CallSig` ignores the tool_use id, so
    // these are one repeating signature and the detector's period-1 rule fires.
    let repeat = |i: usize| {
        ScriptedProvider::tool_call(&format!("t{i}"), "bash", json!({"command": "echo same"}))
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        repeat(0),
        repeat(1),
        repeat(2),
        ScriptedProvider::text_reply("must never be reached"),
    ]));
    let mut s = session(
        provider,
        None,
        EngineConfig {
            max_turns: 20,
            ..Default::default()
        },
    );
    s.handle
        .set_mode(hotl_tools::rules::PermissionMode::DontAsk)
        .await;

    s.handle.prompt("loop forever".into()).await;
    let mut saw_ask = false;
    let outcome = loop {
        match next_event(&mut s).await {
            // Deliberately unanswered: nobody is watching in an unattended
            // posture, which is the whole point.
            EngineEvent::Ask { .. } => saw_ask = true,
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(
        matches!(outcome, Outcome::DoomLoop { .. }),
        "expected a hard stop, got {outcome:?}"
    );
    assert!(
        !saw_ask,
        "an unattended mode must never emit an Ask nobody can answer"
    );
}

/// Vuln 8: a tool's summary is model-authored text rendered into the human's
/// y/N prompt. The engine must flatten it to a single control-free line at the
/// ask chokepoint, so a `bash` command carrying `\r\x1b[2K` (a line-erase) or a
/// bidi override cannot spoof what the human is about to approve.
#[tokio::test]
async fn ask_summaries_are_sanitized_before_they_reach_the_human() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "bash",
            json!({"command": "echo hi\n\u{1b}[2Krm -rf / \u{202e}"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider, None, EngineConfig::default());
    s.handle.prompt("run bash".into()).await;

    let summary = loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { summary, reply, .. } => {
                let _ = reply.send(AskReply::Deny { message: None });
                break summary;
            }
            EngineEvent::TurnDone { .. } => panic!("expected an ask, got turn done"),
            _ => {}
        }
    };
    assert!(!summary.contains('\n'), "newline survived: {summary:?}");
    assert!(!summary.contains('\u{1b}'), "ESC survived: {summary:?}");
    assert!(
        !summary.contains('\u{202e}'),
        "bidi override survived: {summary:?}"
    );
}

/// The control: a mutating batch still brackets itself with the pre/post pair.
#[tokio::test]
async fn a_mutating_batch_still_takes_the_pre_and_post_snapshots() {
    let labels = Arc::<Mutex<Vec<String>>>::default();
    let snapshotter = Arc::new(RecordingSnapshotter(Arc::clone(&labels)));
    let dir_probe = tempfile::tempdir().expect("tempdir");
    let target = dir_probe.path().join("out.txt");
    let path = target.to_str().expect("utf8 path").to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "write", json!({"path": path, "content": "hi"})),
        ScriptedProvider::text_reply("wrote it"),
    ]));
    let mut s = session(provider, Some(snapshotter), EngineConfig::default());

    s.handle.prompt("write something".into()).await;
    let outcome = run_answering(&mut s, || AskReply::Allow).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "wrote it".into()
        }
    );
    assert_eq!(
        *labels.lock().expect("lock"),
        vec!["pre batch 1".to_string(), "post batch 1".to_string()]
    );
}
