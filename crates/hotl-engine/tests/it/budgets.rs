//! 0059 T5 — session spend caps. `max_tool_calls` refuses a batch before it
//! runs; `max_cost_usd` refuses a sample before it is sent, prices nothing on
//! an uncatalogued model, and announces each threshold once. Both end the turn
//! with a typed `Outcome::Budget` rather than a bare error.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<ScriptedProvider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let handle = spawn_session(SessionDeps {
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
        initial_goal: None,
        initial_decisions: Vec::new(),
        plan_files: None,
        config,
    });
    Session { handle, dir }
}

/// Drive one turn, allowing every ask, and collect the budget notices seen.
async fn run(s: &mut Session) -> (Outcome, Vec<(u8, f64, f64)>) {
    s.handle.prompt("go".into()).await;
    let mut notices = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::BudgetNotice {
                pct,
                used_usd,
                cap_usd,
            } => notices.push((pct, used_usd, cap_usd)),
            EngineEvent::TurnDone { outcome, .. } => return (outcome, notices),
            _ => {}
        }
    }
}

/// One `read` per script entry, so each call is its own batch.
fn reads(
    dir: &std::path::Path,
    n: usize,
) -> (Vec<Vec<Result<StreamEvent, ProviderError>>>, String) {
    let file = dir.join("f.txt");
    std::fs::write(&file, "body").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    let scripts = (0..n)
        .map(|i| {
            ScriptedProvider::tool_call(&format!("r{i}"), "read", json!({"path": path.clone()}))
        })
        .collect();
    (scripts, path)
}

#[tokio::test]
async fn max_tool_calls_refuses_the_batch_that_would_cross_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut scripts, _) = reads(dir.path(), 8);
    scripts.push(ScriptedProvider::text_reply("never reached"));
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut s = session(
        Arc::clone(&provider),
        EngineConfig {
            max_turns: 30,
            max_tool_calls: 5,
            ..Default::default()
        },
    );
    let (outcome, _) = run(&mut s).await;
    assert_eq!(
        outcome,
        Outcome::Budget {
            kind: "tool_calls".into(),
            used: 5.0,
            cap: 5.0,
        }
    );
    // Five ran; the sixth batch was refused before it did.
    assert_eq!(
        provider.requests().len(),
        6,
        "five calls plus the sample that proposed the sixth"
    );
}

#[tokio::test]
async fn no_tool_call_cap_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut scripts, _) = reads(dir.path(), 8);
    scripts.push(ScriptedProvider::text_reply("done"));
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut s = session(
        provider,
        EngineConfig {
            max_turns: 30,
            ..Default::default()
        },
    );
    let (outcome, _) = run(&mut s).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done".into()
        }
    );
}

#[tokio::test]
async fn max_cost_usd_refuses_a_sample_it_cannot_afford() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "never reached",
    )]));
    let mut s = session(
        Arc::clone(&provider),
        EngineConfig {
            model: "claude-opus-4-8".into(),
            max_cost_usd: 0.05,
            ..Default::default()
        },
    );
    let (outcome, _) = run(&mut s).await;
    // 64k output tokens at $25/Mtok is $1.60 — far past a five-cent cap, and
    // the refusal happens before the request goes out.
    assert_eq!(
        outcome,
        Outcome::Budget {
            kind: "cost_usd".into(),
            used: 0.0,
            cap: 0.05,
        }
    );
    assert!(
        provider.requests().is_empty(),
        "pre-flight: nothing was sent"
    );
}

/// hotl allowlists no model names, so an uncatalogued model has no price.
/// Refusing on a guessed price would be worse than not capping.
#[tokio::test]
async fn an_uncatalogued_model_is_never_refused_on_price() {
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "ok",
    )]));
    let mut s = session(
        provider,
        EngineConfig {
            model: "some-gateway-alias".into(),
            max_cost_usd: 0.000_001,
            ..Default::default()
        },
    );
    let (outcome, notices) = run(&mut s).await;
    assert_eq!(outcome, Outcome::Done { text: "ok".into() });
    assert!(notices.is_empty(), "no price, no meter: {notices:?}");
}

/// Samples big enough to move the meter: each threshold is announced once,
/// and when the meter finally cannot afford the next sample the turn ends
/// with the typed outcome rather than a bare error.
#[tokio::test]
async fn budget_notices_fire_once_per_threshold() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("f.txt");
    std::fs::write(&file, "body").expect("fixture");
    let path = file.to_str().expect("utf8").to_string();
    // 600k output tokens at $25/Mtok is $15 a sample, against a $100 cap.
    let expensive = |i: usize| {
        let mut script =
            ScriptedProvider::tool_call(&format!("r{i}"), "read", json!({"path": path.clone()}));
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.output_tokens = 600_000;
        }
        script
    };
    let provider = Arc::new(ScriptedProvider::new((0..10).map(expensive).collect()));
    let mut s = session(
        provider,
        EngineConfig {
            model: "claude-opus-4-8".into(),
            max_tokens: 100,
            // Far above the fixture's reported tokens: this test is about the
            // meter, and a compaction here would pop a script entry.
            context_window: 1_000_000_000,
            max_turns: 20,
            max_cost_usd: 100.0,
            ..Default::default()
        },
    );
    let (outcome, notices) = run(&mut s).await;
    let pcts: Vec<u8> = notices.iter().map(|(p, ..)| *p).collect();
    assert_eq!(
        pcts,
        vec![50, 80, 100],
        "one notice per threshold: {notices:?}"
    );
    assert!(
        (notices[0].1 - 60.0).abs() < 0.01,
        "the notice reports real spend: {notices:?}"
    );
    let Outcome::Budget { kind, cap, .. } = outcome else {
        panic!("a spent budget ends the turn typed, not as an error: {outcome:?}");
    };
    assert_eq!((kind.as_str(), cap), ("cost_usd", 100.0));
}

/// The cap is session-level, not per-turn: a fresh prompt does not refill it.
#[tokio::test]
async fn a_spent_budget_refuses_the_next_turn() {
    let expensive = || {
        let mut script = ScriptedProvider::text_reply("spent");
        if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
            usage.output_tokens = 1_200_000;
        }
        script
    };
    let provider = Arc::new(ScriptedProvider::new(vec![expensive(), expensive()]));
    let mut s = session(
        provider,
        EngineConfig {
            model: "claude-opus-4-8".into(),
            max_tokens: 100,
            context_window: 1_000_000_000,
            max_cost_usd: 30.0,
            ..Default::default()
        },
    );
    let (outcome, _) = run(&mut s).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "spent".into()
        }
    );
    let (outcome, _) = run(&mut s).await;
    assert!(
        matches!(&outcome, Outcome::Budget { kind, .. } if kind == "cost_usd"),
        "{outcome:?}"
    );
}
