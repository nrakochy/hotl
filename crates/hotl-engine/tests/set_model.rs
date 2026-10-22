//! `/model` (0067): a switch lands on a turn boundary, writes a `ModelSet`
//! entry where the next request changed, and every assistant item records
//! the model that produced it — dispatch and selection kept apart.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, EngineConfig, EngineEvent, ModelProfile, ModelSwitch, Outcome, SessionDeps,
    SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item};

type BoxStream = futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>;

/// Routes the housekeeping summarize to its own script; the main stream can
/// be slowed so a switch lands while a turn is open.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    slow: bool,
}

impl Provider for Router {
    fn stream(&self, req: SamplingRequest) -> BoxStream {
        if req.system.contains("compress") {
            return self.summarize.stream(req);
        }
        let inner = self.main.stream(req);
        if !self.slow {
            return inner;
        }
        let mut first = true;
        Box::pin(futures_util::StreamExt::then(inner, move |ev| {
            let pause = first && matches!(ev, Ok(StreamEvent::TextDelta { .. }));
            if pause {
                first = false;
            }
            async move {
                if pause {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                ev
            }
        }))
    }
}

struct Session {
    handle: SessionHandle,
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    log_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn session_with(
    scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>,
    config: EngineConfig,
    slow: bool,
) -> Session {
    let main = Arc::new(ScriptedProvider::new(scripts));
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("GOAL: digest"),
        ScriptedProvider::text_reply("GOAL: digest"),
    ]));
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: Arc::new(Router {
            main: Arc::clone(&main),
            summarize: Arc::clone(&summarize),
            slow,
        }),
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
    Session {
        handle,
        main,
        summarize,
        log_path,
        _dir: dir,
    }
}

fn session(scripts: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Session {
    session_with(scripts, EngineConfig::default(), false)
}

fn switch(spec: &str, provider: Option<Arc<dyn Provider>>) -> ModelSwitch {
    ModelSwitch {
        spec: spec.into(),
        model: spec.split_once('/').map_or(spec, |(_, m)| m).to_string(),
        provider,
        profile: ModelProfile {
            context_window: 200_000,
            max_tokens: 8_000,
            token_profile: hotl_context::TokenProfile::CONSERVATIVE,
        },
    }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

async fn run_turn(s: &mut Session, prompt: &str) -> (Outcome, Vec<String>) {
    s.handle.prompt(prompt.into()).await;
    let mut names = Vec::new();
    loop {
        let ev = next_event(s).await;
        names.push(format!("{ev:?}").split('(').next().unwrap().to_string());
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => return (outcome, names),
            _ => {}
        }
    }
}

fn entries(log_path: &std::path::Path) -> Vec<hotl_types::Entry> {
    std::fs::read_to_string(log_path)
        .expect("read log")
        .lines()
        .map(|l| serde_json::from_str(l).expect("entry"))
        .collect()
}

fn kinds(log_path: &std::path::Path) -> Vec<String> {
    entries(log_path)
        .into_iter()
        .map(|e| format!("{:?}", hotl_store::EntryKind::from(&e.payload)).to_lowercase())
        .collect()
}

fn model_sets(log_path: &std::path::Path) -> Vec<String> {
    entries(log_path)
        .into_iter()
        .filter_map(|e| match e.payload {
            EntryPayload::ModelSet { model } => Some(model),
            _ => None,
        })
        .collect()
}

fn assistant_models(log_path: &std::path::Path) -> Vec<Option<String>> {
    entries(log_path)
        .into_iter()
        .filter_map(|e| match e.payload {
            EntryPayload::Item {
                item: Item::Assistant { model, .. },
            } => Some(model),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn set_model_between_turns_reaches_the_next_request() {
    let mut s = session(vec![
        ScriptedProvider::text_reply("one"),
        ScriptedProvider::text_reply("two"),
    ]);
    run_turn(&mut s, "a").await;
    assert_eq!(
        s.handle.set_model(switch("anthropic/other", None)).await,
        Ok(true)
    );
    let ev = next_event(&mut s).await;
    assert!(
        matches!(&ev, EngineEvent::ModelChanged { model, previous, .. } if model == "anthropic/other" && previous == &EngineConfig::default().model),
        "{ev:?}"
    );
    run_turn(&mut s, "b").await;
    let reqs = s.main.requests();
    assert_eq!(reqs[0].model, EngineConfig::default().model);
    assert_eq!(reqs[1].model, "other");
}

#[tokio::test]
async fn set_model_during_a_turn_waits_for_the_boundary() {
    let mut s = session_with(
        vec![
            ScriptedProvider::tool_call("t1", "glob", serde_json::json!({"pattern": "*.nothing"})),
            ScriptedProvider::text_reply("done"),
            ScriptedProvider::text_reply("next"),
        ],
        EngineConfig::default(),
        false,
    );
    s.handle.prompt("go".into()).await;
    // After the first sample's tool call is sealed, the switch arrives.
    loop {
        if matches!(next_event(&mut s).await, EngineEvent::ToolDone { .. }) {
            break;
        }
    }
    assert_eq!(
        s.handle.set_model(switch("anthropic/other", None)).await,
        Ok(false)
    );
    // The switch lands at the boundary: after the turn's last sample, right
    // before `TurnDone` — never between sample 1 and sample 2.
    let mut order = Vec::new();
    loop {
        let ev = next_event(&mut s).await;
        order.push(format!("{ev:?}").split('(').next().unwrap().to_string());
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }
    let change = order
        .iter()
        .position(|n| n == "ModelChanged")
        .expect("applied at the boundary");
    let last_text = order.iter().rposition(|n| n == "TextDelta").unwrap();
    assert!(last_text < change, "{order:?}");
    assert_eq!(order.iter().filter(|n| *n == "ModelChanged").count(), 1);
    run_turn(&mut s, "b").await;
    let reqs = s.main.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(
        reqs[1].model,
        EngineConfig::default().model,
        "sample 2 still on the old model"
    );
    assert_eq!(reqs[2].model, "other");
    // The entry sits after the first turn's last item.
    let k = kinds(&s.log_path);
    let set_at = k.iter().position(|k| k == "modelset").unwrap();
    let last_item_of_turn_one = k[..set_at].iter().rposition(|k| k == "item").unwrap();
    assert!(last_item_of_turn_one < set_at);
    let models = assistant_models(&s.log_path);
    assert_eq!(models.len(), 3);
    assert_eq!(
        models[1].as_deref(),
        Some(EngineConfig::default().model.as_str())
    );
    assert_eq!(models[2].as_deref(), Some("other"));
}

#[tokio::test]
async fn a_second_switch_while_parked_replaces_the_first() {
    let mut s = session_with(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("two"),
        ],
        EngineConfig::default(),
        true,
    );
    s.handle.prompt("go".into()).await;
    loop {
        if matches!(next_event(&mut s).await, EngineEvent::TextDelta(_)) {
            break;
        }
    }
    assert_eq!(
        s.handle.set_model(switch("anthropic/first", None)).await,
        Ok(false)
    );
    assert_eq!(
        s.handle.set_model(switch("anthropic/second", None)).await,
        Ok(false)
    );
    let mut changed = Vec::new();
    loop {
        match next_event(&mut s).await {
            EngineEvent::ModelChanged { model, .. } => changed.push(model),
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert_eq!(changed, vec!["anthropic/second".to_string()]);
    assert_eq!(
        model_sets(&s.log_path),
        vec!["anthropic/second".to_string()]
    );
}

#[tokio::test]
async fn set_model_appends_a_model_set_and_nothing_else() {
    let mut s = session(vec![ScriptedProvider::text_reply("one")]);
    let before = kinds(&s.log_path);
    assert_eq!(
        s.handle.set_model(switch("anthropic/other", None)).await,
        Ok(true)
    );
    next_event(&mut s).await;
    let mut expected = before;
    expected.push("modelset".into());
    assert_eq!(kinds(&s.log_path), expected);
}

#[tokio::test]
async fn a_switch_to_the_active_model_appends_nothing() {
    let mut s = session(vec![ScriptedProvider::text_reply("one")]);
    let before = kinds(&s.log_path);
    let current = EngineConfig::default().model;
    assert_eq!(s.handle.set_model(switch(&current, None)).await, Ok(true));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(kinds(&s.log_path), before);
    assert!(s.handle.events.try_recv().is_err(), "no ModelChanged");
}

#[tokio::test]
async fn assistant_items_record_the_model_that_produced_them() {
    let mut s = session(vec![
        ScriptedProvider::text_reply("one"),
        ScriptedProvider::text_reply("two"),
    ]);
    run_turn(&mut s, "a").await;
    s.handle
        .set_model(switch("anthropic/other", None))
        .await
        .unwrap();
    next_event(&mut s).await;
    run_turn(&mut s, "b").await;
    assert_eq!(
        assistant_models(&s.log_path),
        vec![
            Some(EngineConfig::default().model),
            Some("other".to_string())
        ]
    );
}

#[tokio::test]
async fn a_fallback_sample_is_stamped_with_the_fallback_model() {
    let mut s = session_with(
        vec![
            vec![Err(ProviderError::Transport("connection reset".into()))],
            ScriptedProvider::text_reply("served by fallback"),
        ],
        EngineConfig {
            fallback_models: vec!["backup-model".into()],
            ..EngineConfig::default()
        },
        false,
    );
    let (outcome, names) = run_turn(&mut s, "hi").await;
    assert!(matches!(outcome, Outcome::Done { .. }));
    assert!(names.iter().any(|n| n == "FallbackModel"));
    assert_eq!(
        assistant_models(&s.log_path),
        vec![Some("backup-model".to_string())]
    );
    assert!(
        model_sets(&s.log_path).is_empty(),
        "dispatch is not selection"
    );
}

#[tokio::test]
async fn a_cross_dialect_switch_swaps_the_provider() {
    let mut s = session(vec![ScriptedProvider::text_reply("one")]);
    run_turn(&mut s, "a").await;
    let other = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "from the other dialect",
    )]));
    let as_dyn: Arc<dyn Provider> = other.clone();
    s.handle
        .set_model(switch("openai/gpt-5", Some(as_dyn)))
        .await
        .unwrap();
    next_event(&mut s).await;
    let (outcome, _) = run_turn(&mut s, "b").await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "from the other dialect".into()
        }
    );
    assert_eq!(s.main.request_count(), 1);
    assert_eq!(other.request_count(), 1);
    assert_eq!(other.requests()[0].model, "gpt-5");
}

#[tokio::test]
async fn a_cross_dialect_switch_drops_the_startup_fallback_chain() {
    let mut s = session_with(
        vec![ScriptedProvider::text_reply("one")],
        EngineConfig {
            fallback_models: vec!["backup-model".into()],
            ..EngineConfig::default()
        },
        false,
    );
    run_turn(&mut s, "a").await;
    let other = Arc::new(ScriptedProvider::new(vec![vec![Err(
        ProviderError::Transport("down".into()),
    )]]));
    let as_dyn: Arc<dyn Provider> = other.clone();
    s.handle
        .set_model(switch("openai/gpt-5", Some(as_dyn)))
        .await
        .unwrap();
    next_event(&mut s).await;
    let (outcome, names) = run_turn(&mut s, "b").await;
    assert!(matches!(outcome, Outcome::Error { .. }), "{outcome:?}");
    assert!(
        !names.iter().any(|n| n == "FallbackModel"),
        "the startup chain belongs to the startup dialect: {names:?}"
    );
    assert_eq!(other.request_count(), 1);
}

#[tokio::test]
async fn housekeeping_follows_the_active_dialect() {
    // A window small enough that the second turn folds.
    let config = EngineConfig {
        context_window: 1_000,
        keep_results_turns: 0,
        utility_model: Some("tiny-helper".into()),
        ..EngineConfig::default()
    };
    let mut s = session_with(
        vec![ScriptedProvider::text_reply(&"y".repeat(2_600))],
        config,
        false,
    );
    run_turn(&mut s, "a").await;
    // Same dialect: the digest runs on the configured helper.
    s.handle.fold(None).await;
    loop {
        if matches!(
            next_event(&mut s).await,
            EngineEvent::Compacted { .. } | EngineEvent::FoldSkipped { .. }
        ) {
            break;
        }
    }
    assert_eq!(s.summarize.requests().last().unwrap().model, "tiny-helper");
    // Cross-dialect: the helper's name belongs to the old provider; the
    // digest runs on the active model.
    let other = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        &"z".repeat(2_600),
    )]));
    let as_dyn: Arc<dyn Provider> = other.clone();
    s.handle
        .set_model(switch("openai/gpt-5", Some(as_dyn)))
        .await
        .unwrap();
    next_event(&mut s).await;
    run_turn(&mut s, "b").await;
    s.handle.fold(None).await;
    loop {
        if matches!(
            next_event(&mut s).await,
            EngineEvent::Compacted { .. } | EngineEvent::FoldSkipped { .. }
        ) {
            break;
        }
    }
    // The summarize request went to the *active* provider (the router no
    // longer sees it), on the session model.
    let other_reqs = other.requests();
    let digest = other_reqs
        .iter()
        .find(|r| r.system.contains("compress"))
        .expect("the digest ran on the active provider");
    assert_eq!(digest.model, "gpt-5");
}

#[tokio::test]
async fn a_switch_past_the_new_window_is_refused() {
    let big = "x".repeat(700_000);
    let mut s = session_with(
        vec![ScriptedProvider::text_reply("one")],
        EngineConfig {
            context_window: 1_000_000,
            ..EngineConfig::default()
        },
        false,
    );
    s.handle.prompt(big).await;
    loop {
        if matches!(next_event(&mut s).await, EngineEvent::TurnDone { .. }) {
            break;
        }
    }
    let before = kinds(&s.log_path);
    let mut small = switch("anthropic/small", None);
    small.profile.context_window = 200_000;
    let err = s.handle.set_model(small).await.unwrap_err();
    assert!(
        err.contains("200000") && err.contains("anthropic/small"),
        "{err}"
    );
    assert_eq!(kinds(&s.log_path), before);
    assert!(s.handle.events.try_recv().is_err());
}

#[tokio::test]
async fn a_parked_switch_applies_on_cancel() {
    let mut s = session_with(
        vec![ScriptedProvider::hang_after(
            ScriptedProvider::partial_text("working…"),
        )],
        EngineConfig::default(),
        false,
    );
    s.handle.prompt("go".into()).await;
    loop {
        if matches!(next_event(&mut s).await, EngineEvent::TextDelta(_)) {
            break;
        }
    }
    assert_eq!(
        s.handle.set_model(switch("anthropic/other", None)).await,
        Ok(false)
    );
    s.handle.interrupt();
    let mut order = Vec::new();
    loop {
        let ev = next_event(&mut s).await;
        let name = format!("{ev:?}").split('(').next().unwrap().to_string();
        order.push(name.clone());
        if name == "TurnDone" {
            break;
        }
    }
    assert!(order.contains(&"ModelChanged".to_string()), "{order:?}");
    assert_eq!(model_sets(&s.log_path), vec!["anthropic/other".to_string()]);
}

/// 0067 decision 3: hotl never strips thinking on a switch. A signed block
/// stays in the log and the projection across A → B → A, and A gets its own
/// block back unchanged on the third request.
#[tokio::test]
async fn signed_thinking_survives_a_round_trip_in_the_log() {
    let signed =
        serde_json::json!({"type": "thinking", "thinking": "private", "signature": "sig=="});
    let first = vec![
        Ok(StreamEvent::Started { request_id: None }),
        Ok(StreamEvent::Completed {
            stop: hotl_types::StopReason::EndTurn,
            usage: hotl_types::TokenUsage::default(),
            blocks: vec![
                signed.clone(),
                serde_json::json!({"type": "text", "text": "one"}),
            ],
        }),
    ];
    let mut s = session(vec![first, ScriptedProvider::text_reply("three")]);
    run_turn(&mut s, "a").await;
    let other = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "two",
    )]));
    let as_dyn: Arc<dyn Provider> = other.clone();
    s.handle
        .set_model(switch("openai/gpt-5", Some(as_dyn)))
        .await
        .unwrap();
    next_event(&mut s).await;
    run_turn(&mut s, "b").await;
    let back: Arc<dyn Provider> = Arc::new(Router {
        main: Arc::clone(&s.main),
        summarize: Arc::clone(&s.summarize),
        slow: false,
    });
    s.handle
        .set_model(switch(&EngineConfig::default().model, Some(back)))
        .await
        .unwrap();
    next_event(&mut s).await;
    run_turn(&mut s, "c").await;
    // The log still holds the signed block.
    let models = entries(&s.log_path)
        .into_iter()
        .filter_map(|e| match e.payload {
            EntryPayload::Item {
                item: Item::Assistant { blocks, .. },
            } => Some(blocks),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(models[0].contains(&signed), "{:?}", models[0]);
    // The other dialect's request carried the item (dropping is the
    // serializer's job, at render), and the third request carries it back
    // unchanged.
    assert!(other.requests()[0]
        .items
        .iter()
        .any(|i| matches!(&**i, Item::Assistant { blocks, .. } if blocks.contains(&signed))));
    let third = s.main.requests().pop().unwrap();
    assert!(third
        .items
        .iter()
        .any(|i| matches!(&**i, Item::Assistant { blocks, .. } if blocks.contains(&signed))));
}
