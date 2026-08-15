//! `SessionCmd::SetMode` appends a durable `mode_set` entry (mirrors
//! `rename.rs`) and takes effect immediately on the running session — no
//! `Arc<Rules>` reallocation, no waiting for resume. `SetPlan` is the same
//! contract on the other permission axis, via `plan_set`.

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
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });

    handle.set_mode(PermissionMode::DontAsk).await;
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
    assert_eq!(modes, vec!["dontask".to_string()]);
}

/// The plan axis gets its own durable entry, not a `mode_set`: the two are
/// independent, and folding plan into the mode string is exactly the
/// conflation this change removed.
#[tokio::test]
async fn set_plan_appends_its_own_durable_entry() {
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
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });

    handle.set_plan(true).await;
    handle.set_plan(false).await;
    handle.set_plan(true).await;
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

    let entries: Vec<bool> = std::fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::PlanSet { on } => Some(on),
            EntryPayload::ModeSet { .. } => panic!("SetPlan must not write a mode_set"),
            _ => None,
        })
        .collect();
    // Every toggle is recorded; replay takes the last, so the session resumes
    // with plan on.
    assert_eq!(entries, vec![true, false, true]);
}

/// The flip must gate the *running* session immediately: no resume, no
/// rebuilt `Rules`. A write issued after `set_plan(true)` stops for an ask it
/// would otherwise never have raised, and the declined call writes nothing.
#[tokio::test]
async fn set_plan_takes_effect_on_the_running_session() {
    // `write` resolves against the process-global fsguard root, not `cwd`.
    let stray = "set-plan-declined.txt";
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "write", json!({"path": stray, "content": "nope"})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        // Starts in Bypass with plan OFF: without the flip below this write
        // would auto-allow, so the ask is unambiguously `set_plan`'s doing.
        rules: Arc::new(Rules::default().with_mode(PermissionMode::Bypass)),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });

    handle.set_plan(true).await;
    handle.prompt("go".into()).await;

    let target = std::env::current_dir().expect("cwd").join(stray);
    let _ = std::fs::remove_file(&target);
    let (mut saw_ask, mut saw_auto) = (false, false);
    loop {
        match tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed")
        {
            EngineEvent::Ask { reply, .. } => {
                saw_ask = true;
                let _ = reply.send(hotl_engine::AskReply::Deny { message: None });
            }
            EngineEvent::ToolAutoAllowed { .. } => saw_auto = true,
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    let landed = target.exists();
    let _ = std::fs::remove_file(&target);
    assert!(saw_ask, "write must ask after set_plan(true)");
    assert!(!saw_auto, "plan's floor sits above the bypass tier");
    assert!(!landed);
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
///
/// "This crate has no `security-enforced` feature of its own" is why the guard
/// below is a **runtime** check and not a `#[cfg]`: a `cfg` here is always
/// false, but `cargo test --workspace --all-features` still compiles
/// `hotl-tools` with the feature through unification, and then the premise —
/// a normal build — is simply not the build being tested.
#[tokio::test]
async fn set_mode_auto_stays_auto_on_a_normal_build() {
    if hotl_tools::rules::enforced_build() {
        return;
    }
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
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });

    handle.set_mode(PermissionMode::Bypass).await;
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
