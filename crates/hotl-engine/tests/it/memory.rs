//! 0069 — the memory write path end to end: `/remember` asks through the
//! ordinary permission ask and appends one dated line; the `remember` tool's
//! gate is the same confirm; the dial refuses without asking; the cap refuses
//! after a yes; a child registry never carries the tool.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session_with_channels, AskReply, EngineConfig, EngineEvent, MemoryFiles, MemoryMode,
    SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry, RememberTool};
use hotl_types::{Entry, EntryPayload};
use serde_json::json;

struct Built {
    handle: SessionHandle,
    /// Taken off the handle so a `remember(&self)` future and the event
    /// reader can run side by side.
    events: tokio::sync::mpsc::Receiver<EngineEvent>,
    log_path: std::path::PathBuf,
    mem_dir: std::path::PathBuf,
}

fn build(provider: Arc<ScriptedProvider>, dir: &std::path::Path, mode: MemoryMode) -> Built {
    let mem_dir = dir.join("data/memory/proj");
    let config = EngineConfig {
        memory_files: Some(MemoryFiles {
            project: "proj".into(),
            dir: mem_dir.clone(),
            mode,
        }),
        ask_expiry: Duration::from_secs(5),
        ..Default::default()
    };
    let log = SessionLog::create(dir, &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
    let (event_tx, event_rx) = hotl_engine::event_channel();
    let mut registry = Registry::builtin();
    // Registered the way the binary does under `read-write`: the gate asks,
    // the sink goes straight to the writer.
    registry.register(Box::new(RememberTool::new(hotl_engine::memory_sink(
        cmd_tx.downgrade(),
    ))));
    let mut handle = spawn_session_with_channels(
        SessionDeps {
            concurrency: Default::default(),
            provider,
            registry: Arc::new(registry),
            rules: Arc::new(Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "sys".into(),
            cwd: dir.to_path_buf(),
            hooks: None,
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            initial_decisions: Vec::new(),
            plan_files: None,
            initial_goal: None,
            config,
        },
        cmd_tx,
        cmd_rx,
        event_tx,
        event_rx,
        hotl_engine::hooks::NotificationDrain::new(),
    );
    let (_unused_tx, parked) = hotl_engine::event_channel();
    let events = std::mem::replace(&mut handle.events, parked);
    Built {
        handle,
        events,
        log_path,
        mem_dir,
    }
}

fn payloads(path: &std::path::Path) -> Vec<EntryPayload> {
    std::fs::read_to_string(path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .map(|e| e.payload)
        .collect()
}

async fn next_event(events: &mut tokio::sync::mpsc::Receiver<EngineEvent>) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// Drive `remember` from the handle, answer the one ask, return the result
/// and whether `MemoryChanged` followed.
async fn remember_answering(
    built: &mut Built,
    note: &str,
    answer: AskReply,
) -> (Result<(), String>, Option<EngineEvent>) {
    let handle_cmd = built.handle.remember(note.to_string());
    let events = async {
        let ev = next_event(&mut built.events).await;
        let EngineEvent::Ask {
            summary,
            protected_why,
            reply,
        } = ev
        else {
            panic!("expected the confirm ask, got {ev:?}");
        };
        assert!(summary.contains("remember → project memory:"), "{summary}");
        assert!(protected_why.is_some(), "the why names the file");
        let _ = reply.send(answer);
    };
    // Both must progress together: the ask is emitted inside `remember`.
    let (result, ()) = tokio::join!(handle_cmd, events);
    let changed = match result {
        Ok(()) => Some(next_event(&mut built.events).await),
        Err(_) => None,
    };
    (result, changed)
}

#[tokio::test]
async fn remember_from_the_handle_asks_then_appends_logs_and_emits() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut built = build(provider, dir.path(), MemoryMode::ReadWrite);
    let (result, changed) =
        remember_answering(&mut built, "tests need nextest", AskReply::Allow).await;
    assert_eq!(result, Ok(()));
    let Some(EngineEvent::MemoryChanged {
        note, bytes, path, ..
    }) = changed
    else {
        panic!("expected MemoryChanged, got {changed:?}");
    };
    assert_eq!(note, "tests need nextest");
    let file = std::fs::read_to_string(built.mem_dir.join("MEMORY.md")).unwrap();
    assert_eq!(bytes, file.len());
    assert!(path.ends_with("MEMORY.md"));
    assert!(file.ends_with(": tests need nextest\n"), "{file}");
    assert_eq!(file.lines().filter(|l| l.starts_with("- ")).count(), 1);
    let logged = payloads(&built.log_path);
    assert!(matches!(
        logged.iter().find(|p| matches!(p, EntryPayload::PendingAsk { .. })),
        Some(EntryPayload::PendingAsk { summary, .. }) if summary.contains("tests need nextest")
    ));
    assert!(logged
        .iter()
        .any(|p| matches!(p, EntryPayload::AskResolved { allowed: true, .. })));
    assert!(logged.iter().any(|p| matches!(
        p,
        EntryPayload::MemoryAppended { note, .. } if note == "tests need nextest"
    )));
}

#[tokio::test]
async fn remember_denied_writes_nothing_and_logs_the_denial() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut built = build(provider, dir.path(), MemoryMode::ReadWrite);
    let (result, _) =
        remember_answering(&mut built, "nope", AskReply::Deny { message: None }).await;
    assert_eq!(result, Err("not saved".to_string()));
    assert!(!built.mem_dir.join("MEMORY.md").exists());
    let logged = payloads(&built.log_path);
    assert!(logged
        .iter()
        .any(|p| matches!(p, EntryPayload::AskResolved { allowed: false, .. })));
    assert!(!logged
        .iter()
        .any(|p| matches!(p, EntryPayload::MemoryAppended { .. })));
}

#[tokio::test]
async fn remember_refuses_in_read_only_mode_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut built = build(provider, dir.path(), MemoryMode::ReadOnly);
    let result = built.handle.remember("x".into()).await;
    assert_eq!(
        result,
        Err("memory is read-only ([memory] mode)".to_string())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), built.events.recv())
            .await
            .is_err(),
        "no Ask, no event"
    );
    assert!(!payloads(&built.log_path)
        .iter()
        .any(|p| matches!(p, EntryPayload::PendingAsk { .. })));
}

#[tokio::test]
async fn remember_refuses_past_the_budget_after_a_yes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut built = build(provider, dir.path(), MemoryMode::ReadWrite);
    std::fs::create_dir_all(&built.mem_dir).unwrap();
    let mut big = String::from("## Notes\n");
    while big.len() < hotl_context::MEMORY_BUDGET_BYTES - 5 {
        big.push_str("- 2026-01-01: filler\n");
    }
    std::fs::write(built.mem_dir.join("MEMORY.md"), &big).unwrap();
    let (result, _) = remember_answering(&mut built, "one more", AskReply::Allow).await;
    let err = result.unwrap_err();
    assert!(
        err.contains(&format!("{}-byte cap", hotl_context::MEMORY_BUDGET_BYTES)),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(built.mem_dir.join("MEMORY.md")).unwrap(),
        big
    );
    assert!(!payloads(&built.log_path)
        .iter()
        .any(|p| matches!(p, EntryPayload::MemoryAppended { .. })));
}

#[tokio::test]
async fn the_remember_tool_writes_once_after_the_gate_allows() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "remember",
            json!({"note": "cargo nextest, never cargo test"}),
        ),
        ScriptedProvider::text_reply("noted"),
    ]));
    let mut built = build(Arc::clone(&provider), dir.path(), MemoryMode::ReadWrite);
    built.handle.prompt("go".into()).await;
    let mut asks = 0;
    let mut changed = 0;
    loop {
        match next_event(&mut built.events).await {
            EngineEvent::Ask { summary, reply, .. } => {
                assert!(summary.starts_with("remember: \""), "{summary}");
                asks += 1;
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::MemoryChanged { .. } => changed += 1,
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert_eq!(asks, 1, "the gate asks once; the sink never asks again");
    assert_eq!(changed, 1);
    let file = std::fs::read_to_string(built.mem_dir.join("MEMORY.md")).unwrap();
    assert_eq!(
        file.lines().filter(|l| l.starts_with("- ")).count(),
        1,
        "{file}"
    );
    // The model saw the confirmation text.
    let last = provider.requests().pop().unwrap();
    let results = last
        .items
        .iter()
        .filter_map(|i| match &**i {
            hotl_types::Item::ToolResults { results } => Some(results.clone()),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        results
            .iter()
            .any(|r| r.content.contains("loads next session")),
        "{results:?}"
    );
}

#[tokio::test]
async fn the_remember_tool_denied_at_the_gate_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "remember", json!({"note": "secret stuff"})),
        ScriptedProvider::text_reply("ok"),
    ]));
    let mut built = build(provider, dir.path(), MemoryMode::ReadWrite);
    built.handle.prompt("go".into()).await;
    loop {
        match next_event(&mut built.events).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Deny { message: None });
            }
            EngineEvent::MemoryChanged { .. } => panic!("nothing may be written"),
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(!built.mem_dir.join("MEMORY.md").exists());
}

#[test]
fn a_child_registry_has_no_remember_tool() {
    let mut registry = Registry::builtin();
    registry.register(Box::new(RememberTool::new(Arc::new(|_| {
        Box::pin(async { Ok(()) })
    }))));
    assert!(registry.get("remember").is_some());
    let child = registry.filtered(|_| true);
    assert!(
        child.get("remember").is_none(),
        "children never write memory"
    );
}

#[tokio::test]
async fn an_unanswered_confirm_re_surfaces_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut built = build(provider, dir.path(), MemoryMode::ReadWrite);
    let remember = built.handle.remember("pending".into());
    let drop_reply = async {
        let ev = next_event(&mut built.events).await;
        let EngineEvent::Ask { reply, .. } = ev else {
            panic!("{ev:?}");
        };
        drop(reply);
    };
    let (result, ()) = tokio::join!(remember, drop_reply);
    assert!(result.is_err());
    // The resolution is logged as a no, so a *finished* confirm never warns…
    let logged = payloads(&built.log_path);
    assert!(logged
        .iter()
        .any(|p| matches!(p, EntryPayload::AskResolved { allowed: false, .. })));
    // …but a log that stopped between the two entries re-surfaces it.
    let text = std::fs::read_to_string(&built.log_path).unwrap();
    let cut: String = text
        .lines()
        .filter(|l| !l.contains("\"ask_resolved\""))
        .map(|l| format!("{l}\n"))
        .collect();
    let truncated = dir.path().join("truncated.jsonl");
    std::fs::write(&truncated, cut).unwrap();
    let replayed = hotl_store::replay(&truncated).expect("replay");
    assert!(
        replayed
            .warnings
            .iter()
            .any(|w| w.contains("unanswered permission request") && w.contains("pending")),
        "{:?}",
        replayed.warnings
    );
}

#[tokio::test]
async fn memory_entries_reads_without_appending() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let built = build(provider, dir.path(), MemoryMode::ReadOnly);
    std::fs::create_dir_all(&built.mem_dir).unwrap();
    std::fs::write(
        built.mem_dir.join("MEMORY.md"),
        "## Notes\n- 2026-10-01: one\n- 2026-10-02: two\n",
    )
    .unwrap();
    let report = built.handle.memory_report().await.expect("a report");
    assert_eq!(report.mode, MemoryMode::ReadOnly);
    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.entries[1].note, "two");
    assert_eq!(report.bytes, 45);
    assert_eq!(report.cap, hotl_context::MEMORY_BUDGET_BYTES);
    let log_len = payloads(&built.log_path).len();
    let _ = built.handle.memory_report().await;
    assert_eq!(
        payloads(&built.log_path).len(),
        log_len,
        "a read appends nothing"
    );
}
