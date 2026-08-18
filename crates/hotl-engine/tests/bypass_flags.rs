//! Bypass never blocks on a human (0036): the protected floor maps class →
//! flagged verdicts, the gate answers itself, and `ToolFlagged` is the
//! notification that replaces the ask. These tests pin the three floor
//! dispositions end-to-end — allow-with-flag on a protected in-root write and
//! an outside read, refuse-with-flag on an outside write — plus the control:
//! the same calls under `Ask` mode still prompt.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::rules::{PermissionMode, Rules};
use hotl_tools::Registry;
use hotl_types::{EntryPayload, Item};
use serde_json::{json, Value};

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

/// See `plan_mode.rs`: on a `security-enforced` build `Bypass` cannot exist
/// at runtime, so every bypass-premised test is void, not violated.
fn bypass_unavailable() -> bool {
    hotl_tools::rules::enforced_build()
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// What one scripted call surfaced: asks, flags (by direction), and the log.
struct Ran {
    asked: bool,
    flagged_allow: bool,
    flagged_deny: bool,
    log: String,
}

async fn run_one(mode: PermissionMode, tool: &str, input: Value, answer: AskReply) -> Ran {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", tool, input),
        ScriptedProvider::text_reply("done"),
    ]));
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default().with_mode(mode)),
        sandbox_enforced: true,
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
    let mut s = Session { handle, dir };
    s.handle.prompt("go".into()).await;

    let (mut asked, mut flagged_allow, mut flagged_deny) = (false, false, false);
    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                asked = true;
                let _ = reply.send(answer.clone());
            }
            EngineEvent::ToolFlagged { denied, .. } => {
                if denied {
                    flagged_deny = true;
                } else {
                    flagged_allow = true;
                }
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    drop(s.handle);

    let log_path = std::fs::read_dir(s.dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    let log = std::fs::read_to_string(&log_path).expect("read log");
    Ran {
        asked,
        flagged_allow,
        flagged_deny,
        log,
    }
}

/// The `t1` tool result recorded in the session log: (is_error, content).
fn result_of(log: &str) -> (bool, String) {
    for line in log.lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("entry");
        if let EntryPayload::Item {
            item: Item::ToolResults { results },
        } = entry.payload
        {
            for r in results {
                if r.tool_use_id == "t1" {
                    return (r.is_error, r.content);
                }
            }
        }
    }
    panic!("no tool result found for t1");
}

/// `write` resolves against the process-global fsguard workspace root, so a
/// scripted in-root write lands in the process cwd. Same shape as
/// `plan_mode.rs`: a unique name, asserted on, then removed.
fn cwd_target(name: &str) -> std::path::PathBuf {
    let target = std::env::current_dir().expect("cwd").join(name);
    let _ = std::fs::remove_file(&target);
    target
}

#[tokio::test]
async fn bypass_protected_write_runs_with_a_flag_and_no_ask() {
    if bypass_unavailable() {
        return;
    }
    // `Makefile` is execute-later by basename, at any depth.
    let target = cwd_target("Makefile");
    let r = run_one(
        PermissionMode::Bypass,
        "write",
        json!({"path": "Makefile", "content": "all:\n"}),
        AskReply::Deny { message: None },
    )
    .await;
    let landed = target.exists();
    let _ = std::fs::remove_file(&target);
    assert!(!r.asked, "bypass must not prompt for a protected write");
    assert!(r.flagged_allow, "the notice must replace the ask");
    assert!(!r.flagged_deny);
    assert!(landed, "the write must actually have run");
    let (is_error, content) = result_of(&r.log);
    assert!(!is_error, "{content}");
}

#[tokio::test]
async fn bypass_outside_write_is_refused_with_a_flag() {
    if bypass_unavailable() {
        return;
    }
    let outside = tempfile::tempdir().expect("tempdir");
    let target = outside.path().join("hotl-e2e-probe.txt");
    let r = run_one(
        PermissionMode::Bypass,
        "write",
        json!({"path": target.to_str().unwrap(), "content": "nope"}),
        AskReply::Deny { message: None },
    )
    .await;
    assert!(!r.asked, "bypass must not prompt for an outside write");
    assert!(r.flagged_deny, "the refusal must be flagged");
    assert!(!r.flagged_allow);
    assert!(!target.exists(), "a refused write must not land");
    let (is_error, content) = result_of(&r.log);
    assert!(is_error);
    assert!(
        content.contains("refused without prompting"),
        "the result must tell the model what happened and what to do: {content}"
    );
}

#[tokio::test]
async fn bypass_outside_read_flows_with_a_flag() {
    if bypass_unavailable() {
        return;
    }
    let outside = tempfile::tempdir().expect("tempdir");
    let target = outside.path().join("notes.txt");
    std::fs::write(&target, "outside-content\n").expect("write fixture");
    let r = run_one(
        PermissionMode::Bypass,
        "read",
        json!({"path": target.to_str().unwrap()}),
        AskReply::Deny { message: None },
    )
    .await;
    assert!(!r.asked, "bypass must not prompt for an outside read");
    assert!(r.flagged_allow, "the read must be flagged, not silent");
    assert!(!r.flagged_deny);
    let (is_error, content) = result_of(&r.log);
    assert!(!is_error, "{content}");
    assert!(content.contains("outside-content"), "{content}");
}

/// The control: under `Ask` mode all three calls still raise the blocking
/// prompt — the floor survives outside bypass.
#[tokio::test]
async fn ask_mode_is_unchanged_for_all_three() {
    let outside = tempfile::tempdir().expect("tempdir");
    let readable = outside.path().join("notes.txt");
    std::fs::write(&readable, "x").expect("write fixture");
    let cases = [
        ("write", json!({"path": "Makefile", "content": "all:\n"})),
        (
            "write",
            json!({"path": outside.path().join("probe.txt").to_str().unwrap(), "content": "x"}),
        ),
        ("read", json!({"path": readable.to_str().unwrap()})),
    ];
    for (tool, input) in cases {
        let r = run_one(
            PermissionMode::Ask,
            tool,
            input.clone(),
            AskReply::Deny { message: None },
        )
        .await;
        assert!(r.asked, "{tool} {input} must still ask under Ask mode");
        assert!(!r.flagged_allow && !r.flagged_deny, "{tool} {input}");
    }
}
