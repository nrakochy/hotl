//! 0064 T3 — background jobs through the real turn loop: a job's completion is
//! a harness notification that starts a tagged turn when idle, lands after the
//! open batch when a turn is running (never inside it), survives a cancelled
//! turn, dies with the session unless detached, and holds the goal gate.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_testkit::Harness;
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, SyntheticReason};
use serde_json::json;

fn bg(
    id: &str,
    command: &str,
) -> Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>> {
    ScriptedProvider::tool_call(id, "bash", json!({"command": command, "background": true}))
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) only probes.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// `(kind, synthetic)` per item, the coarse order proof.
fn shape(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::User { synthetic, .. } => format!("user {synthetic:?}"),
            Item::Assistant { blocks, .. } => {
                if blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                {
                    "assistant tool_use".into()
                } else {
                    "assistant".into()
                }
            }
            Item::ToolResults { .. } => "tool_results".into(),
            other => format!("{other:?}").chars().take(12).collect(),
        })
        .collect()
}

#[tokio::test]
async fn a_notify_while_idle_starts_a_tagged_turn() {
    let mut h = Harness::new(
        vec![
            bg("t1", "echo hello-from-job; exit 0"),
            ScriptedProvider::text_reply("started it"),
            ScriptedProvider::text_reply("I see it finished"),
        ],
        EngineConfig::default(),
    );
    h.prompt_and_wait("run it").await;
    // The job is short; its completion starts the second turn on its own.
    h.wait_for_outcome().await;
    let items = h.items();
    let s = shape(&items);
    assert_eq!(
        &s[s.len() - 2..],
        &[
            "user Some(BackgroundJob)".to_string(),
            "assistant".to_string()
        ],
        "{s:?}"
    );
    let Some(Item::User { text, .. }) = items.iter().rev().find(|i| {
        matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::BackgroundJob),
                ..
            }
        )
    }) else {
        unreachable!()
    };
    assert!(text.contains("exit=\"0\""), "{text}");
    assert!(text.contains("hello-from-job"), "{text}");
    assert!(text.contains("trust=\"untrusted\""), "{text}");
    assert!(text.contains("full log: read"), "{text}");
    assert!(
        h.seen.iter().any(|e| e.starts_with("JobStarted(")),
        "{:?}",
        h.seen
    );
    assert!(
        h.seen
            .iter()
            .any(|e| e.starts_with("JobDone(") && e.contains("rc=0")),
        "{:?}",
        h.seen
    );
}

/// The job ends while the second sample is still streaming (paused
/// completion): the notification waits for the boundary and lands after the
/// batch's results, before the next assistant item — and the turn goes on.
#[tokio::test]
async fn a_notify_while_running_lands_after_the_batch_and_the_turn_continues() {
    let mut h = Harness::with_paused_completion(
        vec![
            bg("t1", "sleep 0.3; echo done"),
            // The job ends during this sample's pause, so the notification
            // is held; this sample opens a second batch, and the hold lands
            // right after that batch's results.
            ScriptedProvider::tool_call("t2", "bash", json!({"command": "true"})),
            ScriptedProvider::text_reply("answered the job"),
        ],
        EngineConfig::default(),
        Duration::from_millis(1500),
    );
    h.prompt_and_wait("go").await;
    let s = shape(&h.items());
    assert_eq!(
        s,
        vec![
            "user None",
            "assistant tool_use",
            "tool_results",
            "assistant tool_use",
            "tool_results",
            "user Some(BackgroundJob)",
            "assistant",
        ],
        "the notification sits after the batch it waited on, before the next assistant"
    );
    assert_eq!(
        h.seen.iter().filter(|e| e.starts_with("TurnDone")).count(),
        1,
        "one turn"
    );
}

#[tokio::test]
async fn a_notify_never_splits_a_tool_batch() {
    let mut h = Harness::with_paused_completion(
        vec![
            bg("t1", "sleep 0.3; echo done"),
            ScriptedProvider::text_reply("ok"),
            ScriptedProvider::text_reply("ok again"),
        ],
        EngineConfig::default(),
        Duration::from_millis(1500),
    );
    h.prompt_and_wait("go").await;
    let items = h.items();
    for (i, item) in items.iter().enumerate() {
        if let Item::Assistant { blocks, .. } = item {
            if blocks.iter().any(|b| b["type"] == "tool_use") {
                assert!(
                    matches!(items.get(i + 1), Some(Item::ToolResults { .. })),
                    "a tool batch must be followed by its results: {:?}",
                    shape(&items)
                );
            }
        }
    }
    assert!(
        items.iter().any(|i| matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::BackgroundJob),
                ..
            }
        )),
        "{:?}",
        shape(&items)
    );
}

#[tokio::test]
async fn a_cancelled_turn_leaves_the_job_running() {
    let mut h = Harness::with_paused_completion(
        vec![
            bg("t1", "sleep 60"),
            ScriptedProvider::text_reply("never read"),
        ],
        EngineConfig::default(),
        Duration::from_millis(1500),
    );
    h.handle.prompt("go".into()).await;
    // Answer the ask ourselves and wait for the job to start, then interrupt
    // the next sample mid-pause.
    let pid = loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), h.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::JobStarted { pid, .. } => break pid,
            EngineEvent::TurnDone { .. } => panic!("the turn ended before the job started"),
            _ => {}
        }
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.handle.interrupt();
    let outcome = h.wait_for_outcome().await;
    assert!(format!("{outcome:?}").contains("Cancelled"), "{outcome:?}");
    assert!(alive(pid), "esc leaves the job running");
    let jobs = h.handle.jobs().await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, hotl_tools::jobs::JobState::Running);
    assert!(!jobs[0].rc_path().exists());
    let Harness { handle, .. } = h;
    handle.finish(Duration::from_secs(5)).await;
    for _ in 0..50 {
        if !alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(pid), "session end kills the job");
}

#[tokio::test]
async fn jobs_die_with_the_session_unless_detached() {
    let mut h = Harness::new(
        vec![
            hotl_testkit::tool_batch(&[
                (
                    "a",
                    "bash",
                    json!({"command": "sleep 60", "background": true}),
                ),
                ("b", "bash", json!({"command": "sleep 60", "detach": true})),
            ]),
            ScriptedProvider::text_reply("two jobs"),
        ],
        EngineConfig::default(),
    );
    h.prompt_and_wait("go").await;
    let jobs = h.handle.jobs().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    let attached = jobs.iter().find(|j| !j.detached).unwrap().clone();
    let detached = jobs.iter().find(|j| j.detached).unwrap().clone();
    assert!(alive(attached.pid) && alive(detached.pid));
    let Harness { handle, .. } = h;
    handle.finish(Duration::from_secs(5)).await;
    for _ in 0..50 {
        if !alive(attached.pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !alive(attached.pid),
        "the attached job died with the session"
    );
    assert!(alive(detached.pid), "the detached job outlived it");
    // SAFETY: our own child's group.
    unsafe { libc::kill(-(detached.pid as i32), libc::SIGKILL) };
}

#[tokio::test]
async fn the_goal_gate_waits_for_a_running_job() {
    let mut h = Harness::new(
        vec![
            bg("t1", "sleep 0.5"),
            ScriptedProvider::text_reply("started the suite"),
            // The job's completion turn, then the evaluator.
            ScriptedProvider::text_reply("suite passed"),
            ScriptedProvider::text_reply("VERDICT: met\nREASON: the job finished"),
        ],
        EngineConfig::default(),
    );
    h.handle.set_goal(Some("the suite passes".into())).await;
    h.prompt_and_wait("run the suite in the background").await;
    assert!(
        !h.seen.iter().any(|e| e.starts_with("GoalEvaluating")),
        "the gate must wait while the job runs: {:?}",
        h.seen
    );
    h.wait_for_outcome().await;
    assert!(
        h.seen.iter().any(|e| e.starts_with("GoalEvaluating")),
        "the completion turn re-enters the gate: {:?}",
        h.seen
    );
    assert!(
        h.seen
            .iter()
            .any(|e| e.contains("GoalVerdict") && e.contains("Met")),
        "{:?}",
        h.seen
    );
}

#[tokio::test]
async fn a_job_notification_is_masked_like_any_entry() {
    let dir = tempfile::tempdir().unwrap();
    let config = EngineConfig::default();
    let masker = Masker::empty().with_value("API_TOKEN", "s3cr3t-value-xyz");
    let log = SessionLog::create(dir.path(), &config.model, None, masker, 0).unwrap();
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![
        bg("t1", "echo token=s3cr3t-value-xyz"),
        ScriptedProvider::text_reply("started"),
        ScriptedProvider::text_reply("seen"),
    ]));
    let mut handle: SessionHandle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        config,
    });
    handle.prompt("go".into()).await;
    let mut dones = 0;
    while dones < 2 {
        let ev = tokio::time::timeout(Duration::from_secs(20), handle.events.recv())
            .await
            .expect("timeout")
            .expect("closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => dones += 1,
            _ => {}
        }
    }
    let text = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        text.contains("background_job"),
        "the notification was logged"
    );
    assert!(
        !text.contains("s3cr3t-value-xyz"),
        "the secret must not reach the log"
    );
    assert!(text.contains("API_TOKEN"), "the mask names the variable");
}
