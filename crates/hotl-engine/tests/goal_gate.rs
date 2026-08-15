//! The goal gate (0034 `/goal`): after a Done turn with an active goal and an
//! empty queue, the evaluator judges the condition — *not yet* re-enters via
//! `start_turn` with NO intermediate `TurnDone` (the one suppression that
//! keeps every surface in "turn running"), *met*/*impossible* tombstone the
//! goal and end the turn, and anything unparseable fails open. The scripted
//! provider serves the evaluator too: `fast_model` is unset, so it falls back
//! to the session model and pops the same script queue.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use hotl_engine::{
    spawn_session, EngineConfig, EngineEvent, GoalVerdictKind, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, SyntheticReason};

fn session(
    provider: Arc<dyn Provider>,
    dir: &std::path::Path,
) -> (SessionHandle, std::path::PathBuf) {
    let config = EngineConfig::default();
    let log = SessionLog::create(dir, &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });
    (handle, log_path)
}

/// Drain events until the first `TurnDone`, returning everything seen
/// (the `TurnDone` included, as the last element).
async fn events_until_turn_done(handle: &mut SessionHandle) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        let done = matches!(ev, EngineEvent::TurnDone { .. });
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

fn verdicts(events: &[EngineEvent]) -> Vec<(GoalVerdictKind, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::GoalVerdict { verdict, turns, .. } => Some((*verdict, *turns)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn not_yet_then_met_runs_two_turns_under_one_turn_done() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("working"),
        ScriptedProvider::text_reply("VERDICT: not_yet\nREASON: nothing verified yet"),
        ScriptedProvider::text_reply("finished"),
        ScriptedProvider::text_reply("VERDICT: met\nREASON: the work is done"),
    ]));
    let (mut handle, log_path) = session(provider.clone(), dir.path());

    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    // The one suppression the whole design rests on: the not-yet turn ended
    // with NO TurnDone — the single final one carries the cumulative spend
    // of both turns (two text_reply samples at 10 in / 5 out each).
    let Some(EngineEvent::TurnDone { outcome, usage }) = seen.last() else {
        unreachable!()
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        usage.input_tokens, 20,
        "cumulative, not last-leg: {usage:?}"
    );
    assert_eq!(
        usage.output_tokens, 10,
        "cumulative, not last-leg: {usage:?}"
    );
    assert_eq!(
        verdicts(&seen),
        vec![(GoalVerdictKind::NotYet, 1), (GoalVerdictKind::Met, 2)]
    );
    // Resolution cleared the goal for every surface.
    assert!(
        seen.iter()
            .any(|e| matches!(e, EngineEvent::GoalChanged { condition: None })),
        "the met verdict must broadcast the clear"
    );
    // Four samples total: two turn legs, two evaluations.
    assert_eq!(provider.request_count(), 4);
    // The evaluator's call is the fast-model shape: the goal system prompt,
    // no tools, no thinking.
    let eval_req = &provider.requests()[1];
    assert!(eval_req.system.contains("VERDICT"), "{}", eval_req.system);
    assert!(eval_req.tools.is_empty());
    assert!(!eval_req.thinking);

    // The continuation's opening item is the tagged guidance, wrapped here
    // (not by start_turn), restating reason and condition.
    let replayed = hotl_store::replay(&log_path).expect("replay");
    let guidance: Vec<&Item> = replayed
        .items
        .iter()
        .filter(|i| {
            matches!(
                i,
                Item::User {
                    synthetic: Some(SyntheticReason::GoalGuidance),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(guidance.len(), 1);
    let Item::User { text, .. } = guidance[0] else {
        unreachable!()
    };
    assert!(text.contains("<system-reminder>"), "{text}");
    assert!(text.contains("nothing verified yet"), "{text}");
    assert!(text.contains("finish the work"), "{text}");
    // The tombstone: an achieved goal must never be restored by resume.
    assert_eq!(replayed.goal, None);
}

#[tokio::test]
async fn impossible_resolves_the_goal_and_ends_the_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("tried"),
        ScriptedProvider::text_reply("VERDICT: impossible\nREASON: the target was deleted"),
    ]));
    let (mut handle, log_path) = session(provider, dir.path());

    handle.set_goal(Some("restore the target".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    assert_eq!(verdicts(&seen), vec![(GoalVerdictKind::Impossible, 1)]);
    assert!(seen
        .iter()
        .any(|e| matches!(e, EngineEvent::GoalChanged { condition: None })));
    assert_eq!(hotl_store::replay(&log_path).expect("replay").goal, None);
}

#[tokio::test]
async fn a_garbage_verdict_fails_open_and_keeps_the_goal() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Both evaluator attempts return unparseable text; the gate must end the
    // turn normally with the goal intact — never trap the user in a loop.
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("working"),
        ScriptedProvider::text_reply("hmm, hard to say"),
        ScriptedProvider::text_reply("still can't tell"),
    ]));
    let (mut handle, log_path) = session(provider, dir.path());

    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    assert_eq!(verdicts(&seen), vec![(GoalVerdictKind::EvalFailed, 1)]);
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, EngineEvent::GoalChanged { condition: None })),
        "failing open must not clear the goal"
    );
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert_eq!(replayed.goal.as_deref(), Some("finish the work"));
    assert!(
        !replayed.items.iter().any(|i| matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::GoalGuidance),
                ..
            }
        )),
        "no continuation was started, so no guidance item may exist"
    );
}

#[tokio::test]
async fn a_queued_prompt_outranks_the_continuation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("first"),
        ScriptedProvider::text_reply("second"),
        ScriptedProvider::text_reply("VERDICT: met\nREASON: both prompts answered"),
    ]));
    let (mut handle, log_path) = session(provider.clone(), dir.path());

    handle.set_goal(Some("answer everything".into())).await;
    // Both prompts enter the mailbox before the actor runs (current-thread
    // runtime): the second is queued while the first turn is live, so the
    // gate must NOT fire after turn one — the queue outranks it.
    handle.prompt("one".into()).await;
    handle.prompt("two".into()).await;

    let first = events_until_turn_done(&mut handle).await;
    assert!(
        verdicts(&first).is_empty(),
        "no evaluation may run while a prompt is queued"
    );
    let second = events_until_turn_done(&mut handle).await;
    assert_eq!(verdicts(&second), vec![(GoalVerdictKind::Met, 1)]);
    // Three samples: two prompt turns, ONE evaluation (after the queue
    // drained), and the goal resolved.
    assert_eq!(provider.request_count(), 3);
    assert_eq!(hotl_store::replay(&log_path).expect("replay").goal, None);
}

/// First `stream()` answers a scripted text turn; every later one signals
/// the test and hangs forever — a provider-shaped stand-in for a wedged
/// evaluator call.
struct HangingEvaluator {
    turn: ScriptedProvider,
    calls: Mutex<u32>,
    eval_started: tokio::sync::mpsc::UnboundedSender<()>,
}

impl Provider for HangingEvaluator {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let mut calls = self.calls.lock().expect("calls mutex");
        *calls += 1;
        if *calls == 1 {
            self.turn.stream(req)
        } else {
            let _ = self.eval_started.send(());
            futures_util::stream::pending().boxed()
        }
    }
}

#[tokio::test]
async fn an_interrupt_during_the_evaluation_fails_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (eval_started_tx, mut eval_started_rx) = tokio::sync::mpsc::unbounded_channel();
    let provider = Arc::new(HangingEvaluator {
        turn: ScriptedProvider::new(vec![ScriptedProvider::text_reply("working")]),
        calls: Mutex::new(0),
        eval_started: eval_started_tx,
    });
    let (mut handle, log_path) = session(provider, dir.path());

    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;

    // The evaluator is in flight and will never answer: Esc must return
    // control immediately (the eval races the turn's cancel token).
    tokio::time::timeout(Duration::from_secs(30), eval_started_rx.recv())
        .await
        .expect("the evaluation never started")
        .expect("signal channel closed");
    handle.interrupt();

    let seen = events_until_turn_done(&mut handle).await;
    // The turn itself had already resolved Done; the cancelled eval fails
    // open — EvalFailed, goal kept, control returned.
    let Some(EngineEvent::TurnDone { outcome, .. }) = seen.last() else {
        unreachable!()
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert_eq!(verdicts(&seen), vec![(GoalVerdictKind::EvalFailed, 1)]);
    assert_eq!(
        hotl_store::replay(&log_path)
            .expect("replay")
            .goal
            .as_deref(),
        Some("finish the work")
    );
}

#[tokio::test]
async fn clearing_an_active_goal_appends_the_tombstone_and_a_bare_clear_is_a_noop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let (mut handle, log_path) = session(provider, dir.path());

    // Clearing when nothing is active: silent no-op — no entry, no event.
    handle.set_goal(None).await;
    handle.set_goal(Some("finish".into())).await;
    let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed");
    assert!(
        matches!(&ev, EngineEvent::GoalChanged { condition: Some(c) } if c == "finish"),
        "{ev:?}"
    );
    handle.set_goal(None).await;
    let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed");
    assert!(
        matches!(&ev, EngineEvent::GoalChanged { condition: None }),
        "{ev:?}"
    );

    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert_eq!(replayed.goal, None, "the cleared goal must not survive");
}
