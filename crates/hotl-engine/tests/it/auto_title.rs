//! Auto session titles (0071 T4): one ~50-token call on the utility model
//! after the first completed turn, written as a log-only `Title` entry that
//! a `/rename` always outranks.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::rules::Rules;
use hotl_tools::Registry;
use hotl_types::{EntryPayload, TokenUsage};

struct Ran {
    outcomes: Vec<Outcome>,
    titled: Vec<(Option<String>, TokenUsage)>,
    log: String,
    requests: Vec<hotl_provider::SamplingRequest>,
}

fn config() -> EngineConfig {
    EngineConfig {
        auto_title: true,
        utility_model: Some("utility-model".into()),
        ..EngineConfig::default()
    }
}

/// Drives `prompts` in order; `rename_after_first_prompt` sends a `/rename`
/// right behind the first prompt, before any title can land.
async fn run(
    scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
    config: EngineConfig,
    prompts: &[&str],
    rename_after_first_prompt: Option<&str>,
) -> Ran {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut handle: SessionHandle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: provider.clone(),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: true,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        config,
    });
    let mut outcomes = Vec::new();
    let mut titled = Vec::new();
    for (i, prompt) in prompts.iter().enumerate() {
        handle.prompt(prompt.to_string()).await;
        if i == 0 {
            if let Some(name) = rename_after_first_prompt {
                handle.rename(name.to_string()).await;
            }
        }
        loop {
            let event = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
                .await
                .expect("event timeout")
                .expect("event channel closed");
            match event {
                EngineEvent::SessionTitled { title, usage } => titled.push((title, usage)),
                EngineEvent::TurnDone { outcome, .. } => {
                    outcomes.push(outcome);
                    break;
                }
                _ => {}
            }
        }
    }
    // The title call is detached: give it a moment to report, then stop
    // waiting — a test that expects no title must not hang.
    let settle = tokio::time::Instant::now() + Duration::from_millis(400);
    while let Ok(Some(event)) = tokio::time::timeout_at(settle, handle.events.recv()).await {
        if let EngineEvent::SessionTitled { title, usage } = event {
            titled.push((title, usage));
        }
    }
    handle.finish(Duration::from_secs(1)).await;
    Ran {
        outcomes,
        titled,
        log: std::fs::read_to_string(&log_path).expect("read log"),
        requests: provider.requests(),
    }
}

fn titles_in(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
        .filter_map(|e| match e.payload {
            EntryPayload::Title { title } => Some(title),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_first_completed_turn_writes_one_title_entry_on_the_utility_model() {
    let r = run(
        vec![
            ScriptedProvider::text_reply("the answer"),
            ScriptedProvider::text_reply("Fix the auth flow"),
        ],
        config(),
        &["help me fix auth"],
        None,
    )
    .await;
    assert!(matches!(r.outcomes[0], Outcome::Done { .. }));
    assert_eq!(r.requests.len(), 2, "one turn, one title call");
    let title_req = &r.requests[1];
    assert_eq!(title_req.model, "utility-model");
    assert!(!title_req.thinking);
    assert_eq!(title_req.max_tokens, 24);
    assert!(title_req.system.contains("at most six words"));
    assert_eq!(titles_in(&r.log), vec!["Fix the auth flow".to_string()]);
    assert_eq!(r.titled.len(), 1);
    assert_eq!(r.titled[0].0.as_deref(), Some("Fix the auth flow"));
}

#[tokio::test]
async fn a_second_turn_does_not_retitle() {
    let r = run(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("First title"),
            ScriptedProvider::text_reply("two"),
        ],
        config(),
        &["first", "second"],
        None,
    )
    .await;
    assert_eq!(r.requests.len(), 3);
    assert_eq!(titles_in(&r.log).len(), 1);
}

#[tokio::test]
async fn a_rename_that_lands_first_outranks_the_title() {
    let r = run(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("Generated title"),
        ],
        config(),
        &["first"],
        Some("my-name"),
    )
    .await;
    assert!(titles_in(&r.log).is_empty(), "{}", r.log);
    assert!(r.log.contains("\"my-name\""));
}

#[tokio::test]
async fn an_errored_first_turn_is_not_titled() {
    let r = run(
        vec![
            vec![Err(ProviderError::Auth("no key".into()))],
            ScriptedProvider::text_reply("never"),
        ],
        config(),
        &["first"],
        None,
    )
    .await;
    assert!(matches!(r.outcomes[0], Outcome::Error { .. }));
    assert_eq!(r.requests.len(), 1, "no title call after an error");
    assert!(r.titled.is_empty());
}

#[tokio::test]
async fn an_unusable_title_writes_nothing() {
    let r = run(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("   "),
        ],
        config(),
        &["first"],
        None,
    )
    .await;
    assert!(titles_in(&r.log).is_empty());
    assert_eq!(r.titled.len(), 1, "the spend is still reported");
    assert_eq!(r.titled[0].0, None);
}

#[tokio::test]
async fn title_spend_reaches_the_session_totals() {
    let r = run(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("A title"),
        ],
        config(),
        &["first"],
        None,
    )
    .await;
    let (_, usage) = &r.titled[0];
    // `text_reply` bills 10 in / 5 out.
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 5));
}

/// A named or resumed session opens with `auto_title` off (the factory's
/// rule); the engine then never spends the call.
#[tokio::test]
async fn a_named_session_is_never_titled() {
    let r = run(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("never"),
        ],
        EngineConfig {
            auto_title: false,
            ..config()
        },
        &["first"],
        None,
    )
    .await;
    assert_eq!(r.requests.len(), 1);
    assert!(r.titled.is_empty());
}
