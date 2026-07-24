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

/// Plan 2 review, Finding 1 (CRITICAL): the `security-enforced` build's
/// Auto→Ask coercion must apply on the runtime `SetMode` path, not just the
/// startup `with_mode` builder — `SharedDeps::set_mode` now routes through
/// the same `hotl_tools::rules::enforced_mode` helper. This crate has no
/// `security-enforced` feature of its own (that coercion is pinned directly
/// against the helper in `hotl-tools`'s own test suite instead — see
/// `enforced_mode_coerces_auto_to_ask`), so this is the mirror-image
/// regression check: on a normal (non-enforced) build, `SetMode(Auto)` must
/// still take effect as `Auto`, end to end through the real actor loop.
#[tokio::test]
async fn set_mode_auto_stays_auto_on_a_normal_build() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    // `write`'s path is resolved against the process's real cwd, not the
    // `cwd` field below (`workspace_contained` rejects absolute paths, so a
    // tempdir path can't be handed in directly either) — this deliberately
    // leaves the process cwd out of it (no `set_current_dir`: it's global
    // and would race every other test in this binary, same rationale as
    // `hotl-tools`'s `glob_walk`/`grep_search` tests). The file this
    // actually writes gets cleaned up below.
    let stray_file = std::env::current_dir()
        .expect("cwd")
        .join("auto-mode-stays-auto.txt");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "write",
            json!({"path": "auto-mode-stays-auto.txt", "content": "yo"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()), // starts in Ask, never Auto
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

    handle.set_mode(PermissionMode::Auto).await;
    handle.prompt("go".into()).await;

    let mut saw_auto_allow = false;
    let mut saw_ask_or_deny = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed")
        {
            EngineEvent::ToolAutoAllowed { .. } => saw_auto_allow = true,
            EngineEvent::ToolDenied { .. } | EngineEvent::Ask { .. } => saw_ask_or_deny = true,
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    let _ = std::fs::remove_file(&stray_file); // best-effort; see comment above
    assert!(
        saw_auto_allow,
        "write must auto-allow after set_mode(Auto) on a normal build"
    );
    assert!(!saw_ask_or_deny);
}
