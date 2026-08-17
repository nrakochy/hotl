//! T3-1: a denial is a decision, not a tool malfunction — it must not draw
//! down the per-tool failure budget, and it must not carry the last-chance
//! `<system-hint>` warning that contradicts its own message.
//! T3-2: whether a batch is mutating is `Tool::read_only()`'s answer, not
//! `name != "read"` — a `glob`/`grep` batch takes no shadow snapshot.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
    Snapshotter,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

/// Records every shadow-snapshot label the engine enqueues and counts
/// `mutation_started` signals (the 0035 taint signal).
#[derive(Default)]
struct RecordingSnapshotter {
    labels: Arc<Mutex<Vec<String>>>,
    mutations: Arc<AtomicUsize>,
}

impl Snapshotter for RecordingSnapshotter {
    fn snapshot(&self, label: String) {
        self.labels.lock().expect("lock").push(label);
    }
    fn mutation_started(&self) {
        self.mutations.fetch_add(1, Ordering::SeqCst);
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
    session_with_rules(provider, snapshots, config, Rules::default())
}

fn session_with_rules(
    provider: Arc<dyn Provider>,
    snapshots: Option<Arc<dyn Snapshotter>>,
    config: EngineConfig,
    rules: Rules,
) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(rules),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        snapshots,
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
    let snapshotter = Arc::new(RecordingSnapshotter {
        labels: Arc::clone(&labels),
        ..Default::default()
    });
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

/// Vuln 6: `read` is `Permission::None` in-workspace and short-circuited the
/// gate before any rule ran, so a `[[deny]]` on it was silently dead. The gate
/// must now consult the deny tiers for a `Permission::None` tool too.
#[tokio::test]
async fn a_deny_rule_bites_a_permission_none_tool() {
    let rules = Rules::from_toml("[[deny]]\ntool = \"read\"\npath_prefix = \".env\"\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "read", json!({"path": ".env"})),
        ScriptedProvider::text_reply("ok"),
    ]));
    let mut s = session_with_rules(provider, None, EngineConfig::default(), rules);
    s.handle.prompt("read the env".into()).await;

    let mut denied = false;
    loop {
        match next_event(&mut s).await {
            EngineEvent::ToolDenied { .. } => denied = true,
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(
        denied,
        "a [[deny]] on read must refuse the call, not run it"
    );
}

/// The control: a mutating batch takes exactly one quiet-window snapshot at
/// its end, plus the taint signal before its first mutating execute (0035).
#[tokio::test]
async fn a_mutating_batch_takes_one_quiet_window_snapshot() {
    let labels = Arc::<Mutex<Vec<String>>>::default();
    let mutations = Arc::<AtomicUsize>::default();
    let snapshotter = Arc::new(RecordingSnapshotter {
        labels: Arc::clone(&labels),
        mutations: Arc::clone(&mutations),
    });
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
        vec!["state after batch 1".to_string()]
    );
    assert_eq!(mutations.load(Ordering::SeqCst), 1, "one signal per batch");
}

/// A fully-denied "mutating" batch mutated nothing: no signal, no snapshot
/// (0035 decision 1 — the old design took two pointless snapshots here).
#[tokio::test]
async fn a_denied_batch_fires_no_signal_and_takes_no_snapshot() {
    let labels = Arc::<Mutex<Vec<String>>>::default();
    let mutations = Arc::<AtomicUsize>::default();
    let snapshotter = Arc::new(RecordingSnapshotter {
        labels: Arc::clone(&labels),
        mutations: Arc::clone(&mutations),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
        ScriptedProvider::text_reply("gave up"),
    ]));
    let mut s = session(provider, Some(snapshotter), EngineConfig::default());

    s.handle.prompt("try it".into()).await;
    let outcome = run_answering(&mut s, || AskReply::Deny { message: None }).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "gave up".into()
        }
    );
    assert!(
        labels.lock().expect("lock").is_empty(),
        "denied batch snapshotted: {:?}",
        labels.lock().expect("lock")
    );
    assert_eq!(mutations.load(Ordering::SeqCst), 0, "denied batch signaled");
}

/// The signal fires only after the gate resolves (0035 decision 7): while
/// the ask is pending — a human moment that can last minutes — no mutation
/// has been signaled, so a concurrent capture stays clean.
#[tokio::test]
async fn an_ask_gated_batch_signals_only_after_the_gate_resolves() {
    let labels = Arc::<Mutex<Vec<String>>>::default();
    let mutations = Arc::<AtomicUsize>::default();
    let snapshotter = Arc::new(RecordingSnapshotter {
        labels: Arc::clone(&labels),
        mutations: Arc::clone(&mutations),
    });
    let dir_probe = tempfile::tempdir().expect("tempdir");
    let target = dir_probe.path().join("out.txt");
    let path = target.to_str().expect("utf8 path").to_string();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "write", json!({"path": path, "content": "hi"})),
        ScriptedProvider::text_reply("wrote it"),
    ]));
    let mut s = session(provider, Some(snapshotter), EngineConfig::default());

    s.handle.prompt("write something".into()).await;
    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                assert_eq!(
                    mutations.load(Ordering::SeqCst),
                    0,
                    "the pending ask must not have signaled a mutation"
                );
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert_eq!(mutations.load(Ordering::SeqCst), 1);
}

/// A registered `Stop` hook runs outside any batch and may write the
/// workspace, so dispatching it fires the taint signal (0035 decision 7).
#[tokio::test]
async fn a_registered_stop_hook_fires_the_taint_signal() {
    struct AllowStop;
    impl hotl_engine::hooks::Hooks for AllowStop {
        fn pre_tool<'a>(
            &'a self,
            _n: &'a str,
            _i: &'a serde_json::Value,
        ) -> futures_util::future::BoxFuture<'a, hotl_engine::hooks::PreToolDecision> {
            Box::pin(std::future::ready(
                hotl_engine::hooks::PreToolDecision::Continue,
            ))
        }
        fn post_tool<'a>(
            &'a self,
            _n: &'a str,
            _r: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Option<String>> {
            Box::pin(std::future::ready(None))
        }
        fn event_mask(&self) -> hotl_engine::hooks::EventMask {
            hotl_engine::hooks::EventMask::STOP
        }
    }

    let mutations = Arc::<AtomicUsize>::default();
    let snapshotter = Arc::new(RecordingSnapshotter {
        labels: Arc::default(),
        mutations: Arc::clone(&mutations),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "done",
    )]));
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
        snapshots: Some(snapshotter),
        hooks: Some(Arc::new(AllowStop)),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });
    let mut s = Session { handle, dir };

    s.handle.prompt("just answer".into()).await;
    let outcome = run_answering(&mut s, || AskReply::Allow).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done".into()
        }
    );
    assert_eq!(
        mutations.load(Ordering::SeqCst),
        1,
        "a registered stop hook must taint any overlapping capture"
    );
}
