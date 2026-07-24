//! `UserPromptSubmit` (tier-1 gap #7): a hook's `additionalContext` becomes
//! one tagged `SystemReminder` user item committed right after the prompt it
//! answers — never a system-prompt edit (prefix-cache stability).

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::hooks::InProcessHooks;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, SyntheticReason};

#[tokio::test]
async fn user_prompt_hook_injects_additional_context_after_the_prompt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "done",
    )]));
    let hooks = InProcessHooks::new().on_user_prompt(|prompt| {
        assert_eq!(prompt, "go");
        Some("remember: use pnpm".to_string())
    });
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: Some(Arc::new(hooks)),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });

    handle.prompt("go".into()).await;

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { outcome, .. } = ev {
            assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
            break;
        }
    }

    let replayed = hotl_store::replay(&log_path).expect("replay");
    // The prompt, then the injected reminder, positioned right after it.
    let prompt_idx = replayed
        .items
        .iter()
        .position(|i| matches!(i, Item::User { text, synthetic: None } if text == "go"))
        .expect("the prompt itself must be in the projection");
    let reminder_idx = replayed
        .items
        .iter()
        .position(|i| {
            matches!(
                i,
                Item::User {
                    synthetic: Some(SyntheticReason::SystemReminder),
                    text,
                } if text.contains("remember: use pnpm")
            )
        })
        .expect("the hook's additionalContext must land as a SystemReminder item");
    assert_eq!(
        reminder_idx,
        prompt_idx + 1,
        "the reminder must be positioned immediately after the prompt"
    );
}

#[tokio::test]
async fn multiple_user_prompt_hooks_concatenate_into_one_item() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "done",
    )]));
    let hooks = InProcessHooks::new()
        .on_user_prompt(|_p| Some("first".to_string()))
        .on_user_prompt(|_p| Some("second".to_string()));
    let mut handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: Some(Arc::new(hooks)),
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        config,
    });

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

    let replayed = hotl_store::replay(&log_path).expect("replay");
    let reminders: Vec<&Item> = replayed
        .items
        .iter()
        .filter(|i| {
            matches!(
                i,
                Item::User {
                    synthetic: Some(SyntheticReason::SystemReminder),
                    ..
                }
            )
        })
        .collect();
    // One item per commit point (Innovation #7) — not two adjacent items.
    assert_eq!(
        reminders.len(),
        1,
        "expected exactly one merged reminder item"
    );
    let Item::User { text, .. } = reminders[0] else {
        unreachable!()
    };
    assert!(text.contains("first") && text.contains("second"), "{text}");
}

#[tokio::test]
async fn no_hooks_means_no_injected_reminder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "done",
    )]));
    let mut handle = spawn_session(SessionDeps {
        provider,
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
        config,
    });

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
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert!(!replayed.items.iter().any(|i| matches!(
        i,
        Item::User {
            synthetic: Some(SyntheticReason::SystemReminder),
            ..
        }
    )));
}
