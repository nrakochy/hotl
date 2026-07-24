//! `SessionCmd::SetMode` appends a durable `mode_set` entry (mirrors
//! `rename.rs`) and takes effect immediately on the running session — no
//! `Arc<Rules>` reallocation, no waiting for resume.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::rules::{PermissionMode, Rules};
use hotl_tools::Registry;
use hotl_types::EntryPayload;
use serde_json::json;

#[tokio::test]
async fn set_mode_appends_a_durable_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let mut handle = spawn_session(SessionDeps {
        provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )])),
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
        config,
    });

    handle.set_mode(PermissionMode::Plan).await;
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }

    let modes: Vec<String> = std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::ModeSet { mode } => Some(mode),
            _ => None,
        })
        .collect();
    assert_eq!(modes, vec!["plan".to_string()]);
}

/// The mode flip must gate the *running* session immediately: no resume, no
/// rebuilt `Rules`. A write issued after `set_mode(Plan)` is denied with a
/// plan-mode reason.
#[tokio::test]
async fn set_mode_takes_effect_on_the_running_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": "should-not-exist.txt", "content": "nope"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()), // starts in Ask, never Plan
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        config,
    });

    handle.set_mode(PermissionMode::Plan).await;
    handle.prompt("go".into()).await;

    let mut saw_denial = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed")
        {
            EngineEvent::ToolDenied { .. } => saw_denial = true,
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(saw_denial, "write must be plan-blocked after set_mode");
    assert!(!dir.path().join("should-not-exist.txt").exists());
}
