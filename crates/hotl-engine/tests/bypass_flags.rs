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

/// One prompt per batch under Bypass, collecting every `ToolFlagged` as
/// (summary, denied) — the instrument for the 0037 D5 notice memo. Reads and
/// writes land in/against `dir`-relative absolute paths the caller prepares.
async fn run_flag_batches(batches: Vec<Vec<(String, &str, Value)>>) -> Vec<(String, bool)> {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let mut scripts = Vec::new();
    for batch in &batches {
        let blocks: Vec<Value> = batch
            .iter()
            .map(|(id, name, input)| {
                serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
            })
            .collect();
        scripts.push(vec![
            Ok(hotl_provider::StreamEvent::Started),
            Ok(hotl_provider::StreamEvent::Completed {
                stop: hotl_types::StopReason::ToolUse,
                usage: hotl_types::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                    ..Default::default()
                },
                blocks,
            }),
        ]);
        scripts.push(ScriptedProvider::text_reply("done"));
    }
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default().with_mode(PermissionMode::Bypass)),
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
    let mut flags = Vec::new();
    for _ in 0..batches.len() {
        s.handle.prompt("go".into()).await;
        loop {
            match next_event(&mut s).await {
                EngineEvent::ToolFlagged {
                    summary, denied, ..
                } => flags.push((summary, denied)),
                EngineEvent::Ask { .. } => panic!("bypass must never ask"),
                EngineEvent::TurnDone { .. } => break,
                _ => {}
            }
        }
    }
    flags
}

/// 0037 D5: repeated page reads of ONE outside file are one boundary
/// decision — one ⚑ notice per turn, not one per page — while a distinct
/// file still gets its own, and a NEW prompt re-flags (the memo is per
/// prompt, so nothing stays buried across a session).
#[tokio::test]
async fn repeat_page_reads_flag_once_per_turn_and_reflag_next_prompt() {
    if bypass_unavailable() {
        return;
    }
    let outside = tempfile::tempdir().expect("tempdir");
    let a = outside.path().join("notes-a.vtt");
    let b = outside.path().join("notes-b.vtt");
    std::fs::write(&a, "line\n".repeat(50)).expect("fixture a");
    std::fs::write(&b, "line\n").expect("fixture b");
    let a_path = a.to_str().unwrap().to_string();
    let b_path = b.to_str().unwrap().to_string();

    let page = |id: &str, path: &str, offset: u64| {
        (
            id.to_string(),
            "read",
            json!({"path": path, "offset": offset, "limit": 10}),
        )
    };
    let flags = run_flag_batches(vec![
        // Turn 1: three pages of A plus one read of B — 2 unique decisions.
        vec![
            page("p1", &a_path, 1),
            page("p2", &a_path, 11),
            page("p3", &a_path, 21),
            ("p4".to_string(), "read", json!({"path": b_path.clone()})),
        ],
        // Turn 2: A again — a fresh prompt must re-surface the notice.
        vec![page("p5", &a_path, 31)],
    ])
    .await;

    let (turn1, turn2): (Vec<_>, Vec<_>) = {
        let split = flags
            .iter()
            .position(|(s, _)| s.contains("from line 31"))
            .expect("turn 2's flag must exist");
        (flags[..split].to_vec(), flags[split..].to_vec())
    };
    assert_eq!(
        turn1.len(),
        2,
        "one notice per distinct file, not per page: {turn1:?}"
    );
    assert!(turn1.iter().any(|(s, _)| s.contains("notes-a.vtt")));
    assert!(turn1.iter().any(|(s, _)| s.contains("notes-b.vtt")));
    assert_eq!(turn2.len(), 1, "a new prompt re-flags: {turn2:?}");
    assert!(flags.iter().all(|(_, denied)| !denied));
}

/// 0037 D5: a flagged REFUSAL is never buried by an earlier allow-notice on
/// the same file — `denied` (with the class) is part of the memo key.
#[tokio::test]
async fn a_flagged_refusal_is_not_buried_by_a_prior_allow_flag() {
    if bypass_unavailable() {
        return;
    }
    let outside = tempfile::tempdir().expect("tempdir");
    let target = outside.path().join("notes.txt");
    std::fs::write(&target, "content\n").expect("fixture");
    let path = target.to_str().unwrap().to_string();

    let flags = run_flag_batches(vec![vec![
        ("t1".to_string(), "read", json!({"path": path.clone()})),
        (
            "t2".to_string(),
            "write",
            json!({"path": path.clone(), "content": "overwrite"}),
        ),
    ]])
    .await;
    assert_eq!(flags.len(), 2, "{flags:?}");
    assert!(
        flags.iter().any(|(_, denied)| !denied),
        "the read's allow-notice: {flags:?}"
    );
    assert!(
        flags.iter().any(|(_, denied)| *denied),
        "the write's refusal notice must surface: {flags:?}"
    );
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
