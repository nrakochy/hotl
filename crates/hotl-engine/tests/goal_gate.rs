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
    spawn_session, AskReply, EngineConfig, EngineEvent, GoalVerdictKind, Outcome, SessionDeps,
    SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Permission, Registry, Tool, ToolOutcome};
use hotl_types::{Item, SyntheticReason};

fn session(
    provider: Arc<dyn Provider>,
    dir: &std::path::Path,
) -> (SessionHandle, std::path::PathBuf) {
    session_with(provider, dir, Registry::builtin())
}

fn session_with(
    provider: Arc<dyn Provider>,
    dir: &std::path::Path,
    registry: Registry,
) -> (SessionHandle, std::path::PathBuf) {
    let config = EngineConfig::default();
    let log = SessionLog::create(dir, &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
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
    });
    (handle, log_path)
}

/// Drain events until the first `TurnDone`, returning everything seen
/// (the `TurnDone` included, as the last element). A permission ask is
/// allowed and dropped rather than collected — its reply channel cannot be
/// put back in the vec, and no test here asserts on the ask itself.
async fn events_until_turn_done(handle: &mut SessionHandle) -> Vec<EngineEvent> {
    let mut seen = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::Ask { reply, .. } = ev {
            let _ = reply.send(AskReply::Allow);
            continue;
        }
        let done = matches!(ev, EngineEvent::TurnDone { .. });
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

/// One idle goal turn: the model talks, the evaluator says not yet.
fn idle_turn() -> Vec<Vec<Result<StreamEvent, ProviderError>>> {
    vec![
        ScriptedProvider::text_reply("still thinking about it"),
        ScriptedProvider::text_reply("VERDICT: not_yet\nREASON: nothing has run"),
    ]
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
    // with NO TurnDone — the single final one carries the cumulative spend of
    // both turns AND both evaluations (four samples at 10 in / 5 out each).
    // The evaluator is not free, so it is not invisible (0051 decision 6).
    let Some(EngineEvent::TurnDone { outcome, usage, .. }) = seen.last() else {
        unreachable!()
    };
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        usage.input_tokens, 40,
        "cumulative, evaluator included: {usage:?}"
    );
    assert_eq!(
        usage.output_tokens, 20,
        "cumulative, evaluator included: {usage:?}"
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
    let hotl_types::Item::User { text, .. } = &*eval_req.items[0] else {
        unreachable!()
    };
    assert!(
        text.contains("<goal-condition>finish the work</goal-condition>"),
        "{text}"
    );
    assert!(text.contains("Progress: turn 1 · "), "{text}");
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
    // The condition is data, and the worker is told where it stands (0051 T4).
    assert!(
        text.contains("<goal-condition>finish the work</goal-condition>"),
        "{text}"
    );
    assert!(text.contains("Progress: turn 1 · "), "{text}");
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

/// 0051 G1: a goal loop that never runs a tool is not making progress. Eight
/// consecutive idle not-yet verdicts pause it — one `TurnDone`, and the goal
/// stays set so the next prompt can re-arm it.
#[tokio::test]
async fn eight_idle_not_yet_turns_pause_the_loop_and_keep_the_goal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script: Vec<_> = (0..8).flat_map(|_| idle_turn()).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let (mut handle, log_path) = session(provider.clone(), dir.path());

    handle.set_goal(Some("the suite is green".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let mut expected: Vec<_> = (1..=8).map(|n| (GoalVerdictKind::NotYet, n)).collect();
    expected.push((GoalVerdictKind::Stalled, 8));
    assert_eq!(verdicts(&seen), expected);
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, EngineEvent::TurnDone { .. }))
            .count(),
        1,
        "the pause ends the logical turn exactly once"
    );
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, EngineEvent::GoalChanged { condition: None })),
        "a stall is a pause, not a tombstone"
    );
    assert_eq!(
        hotl_store::replay(&log_path)
            .expect("replay")
            .goal
            .as_deref(),
        Some("the suite is green")
    );
    // Eight turn legs and eight evaluations, and no ninth leg.
    assert_eq!(provider.request_count(), 16);
}

/// The reset is the proof the brake counts *idle* turns, not turns: one
/// executed tool at turn 8 buys eight more, so the pause lands at turn 16.
#[tokio::test]
async fn a_tool_call_resets_the_idle_count() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut script: Vec<_> = (0..7).flat_map(|_| idle_turn()).collect();
    // Turn 8 runs a tool, so it is not idle however little it says.
    script.push(ScriptedProvider::tool_call(
        "t1",
        "bash",
        serde_json::json!({"command": "echo ok"}),
    ));
    script.push(ScriptedProvider::text_reply("ran it"));
    script.push(ScriptedProvider::text_reply(
        "VERDICT: not_yet\nREASON: one step done",
    ));
    script.extend((0..8).flat_map(|_| idle_turn()));
    let provider = Arc::new(ScriptedProvider::new(script));
    let (mut handle, log_path) = session(provider.clone(), dir.path());

    handle.set_goal(Some("the suite is green".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let mut expected: Vec<_> = (1..=16).map(|n| (GoalVerdictKind::NotYet, n)).collect();
    expected.push((GoalVerdictKind::Stalled, 16));
    assert_eq!(verdicts(&seen), expected);
    assert_eq!(
        hotl_store::replay(&log_path)
            .expect("replay")
            .goal
            .as_deref(),
        Some("the suite is green")
    );
    // 15 talk-only legs + 15 evaluations + the tool turn's extra sample.
    assert_eq!(provider.request_count(), 33);
}

/// 0051 G6: an unrecoverable provider failure tombstones the goal, so a
/// resume never re-arms the loop against a dead credential.
#[tokio::test]
async fn an_unrecoverable_error_tombstones_the_goal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("working"),
        ScriptedProvider::text_reply("VERDICT: not_yet\nREASON: nothing verified yet"),
        vec![Err(ProviderError::Auth("revoked".into()))],
    ]));
    let (mut handle, log_path) = session(provider, dir.path());

    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let Some(EngineEvent::TurnDone { outcome, .. }) = seen.last() else {
        unreachable!()
    };
    assert!(matches!(outcome, Outcome::Error { .. }), "{outcome:?}");
    assert_eq!(
        verdicts(&seen),
        vec![(GoalVerdictKind::NotYet, 1), (GoalVerdictKind::Errored, 1)]
    );
    assert!(seen
        .iter()
        .any(|e| matches!(e, EngineEvent::GoalChanged { condition: None })));
    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert_eq!(replayed.goal, None);
    // The tombstone names why, additively — `"error"` beside achieved and
    // impossible. Read from the log itself: `replay` folds `GoalSet` down to
    // the condition and drops the outcome word.
    let raw = std::fs::read_to_string(&log_path).expect("log");
    assert!(
        raw.contains(r#""kind":"goal_set""#) && raw.contains(r#""outcome":"error""#),
        "the goal tombstone must record why it ended: {raw}"
    );
}

/// The other half of the rule: a transient failure is not the owner's to fix,
/// so the goal stays armed and a later prompt picks it back up.
#[tokio::test]
async fn a_transient_error_leaves_the_goal_armed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("working"),
        ScriptedProvider::text_reply("VERDICT: not_yet\nREASON: nothing verified yet"),
        vec![Err(ProviderError::Http {
            status: 429,
            message: "slow down".into(),
            retry_after: None,
        })],
    ]));
    let (mut handle, log_path) = session(provider, dir.path());

    handle.set_goal(Some("finish the work".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let Some(EngineEvent::TurnDone { outcome, .. }) = seen.last() else {
        unreachable!()
    };
    assert!(matches!(outcome, Outcome::Error { .. }), "{outcome:?}");
    assert_eq!(verdicts(&seen), vec![(GoalVerdictKind::NotYet, 1)]);
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, EngineEvent::GoalChanged { condition: None })),
        "a rate limit must not clear the goal"
    );
    assert_eq!(
        hotl_store::replay(&log_path)
            .expect("replay")
            .goal
            .as_deref(),
        Some("finish the work")
    );
}

/// The pause hands control back without giving up: the next user prompt
/// re-enters the gate with a fresh idle budget.
#[tokio::test]
async fn a_stalled_goal_re_arms_on_the_next_prompt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut script: Vec<_> = (0..8).flat_map(|_| idle_turn()).collect();
    script.push(ScriptedProvider::text_reply("done now"));
    script.push(ScriptedProvider::text_reply(
        "VERDICT: met\nREASON: the suite is green",
    ));
    let provider = Arc::new(ScriptedProvider::new(script));
    let (mut handle, log_path) = session(provider, dir.path());

    handle.set_goal(Some("the suite is green".into())).await;
    handle.prompt("go".into()).await;
    let stalled = events_until_turn_done(&mut handle).await;
    assert_eq!(
        verdicts(&stalled).last(),
        Some(&(GoalVerdictKind::Stalled, 8))
    );

    handle.prompt("again".into()).await;
    let resolved = events_until_turn_done(&mut handle).await;
    assert_eq!(verdicts(&resolved), vec![(GoalVerdictKind::Met, 9)]);
    assert_eq!(hotl_store::replay(&log_path).expect("replay").goal, None);
}

/// A tool that blocks on a nested session, as `spawn` and `workflow` do —
/// so 0055 exempts it from the Layer-B subprocess permit entirely.
struct NestingProbe;

impl Tool for NestingProbe {
    fn name(&self) -> &'static str {
        "nest"
    }
    fn description(&self) -> &str {
        "test nesting probe"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn awaits_child_session(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { ToolOutcome::ok("spawned") })
    }
}

/// A goal making progress through sub-agents must not read as idle. 0055
/// gave `spawn`/`workflow` a subprocess-permit exemption, so a nesting call
/// draws no permit at all; the stall brake counts what `finish_call` saw
/// execute, which is downstream of the permit and independent of it. If the
/// two were ever coupled, this call would count zero and the pause would
/// land at turn 8 instead of turn 16.
#[tokio::test]
async fn a_permit_exempt_nesting_tool_still_counts_as_progress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut script: Vec<_> = (0..7).flat_map(|_| idle_turn()).collect();
    script.push(ScriptedProvider::tool_call(
        "n1",
        "nest",
        serde_json::json!({}),
    ));
    script.push(ScriptedProvider::text_reply("the child did the work"));
    script.push(ScriptedProvider::text_reply(
        "VERDICT: not_yet\nREASON: the child reported back",
    ));
    script.extend((0..8).flat_map(|_| idle_turn()));
    let provider = Arc::new(ScriptedProvider::new(script));
    let mut registry = Registry::builtin();
    registry.register(Box::new(NestingProbe));
    let (mut handle, log_path) = session_with(provider.clone(), dir.path(), registry);

    handle.set_goal(Some("the suite is green".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let mut expected: Vec<_> = (1..=16).map(|n| (GoalVerdictKind::NotYet, n)).collect();
    expected.push((GoalVerdictKind::Stalled, 16));
    assert_eq!(
        verdicts(&seen),
        expected,
        "a permit-exempt tool that ran is still a tool that ran"
    );
    assert_eq!(
        hotl_store::replay(&log_path)
            .expect("replay")
            .goal
            .as_deref(),
        Some("the suite is green")
    );
    assert_eq!(provider.request_count(), 33);
}

// ── 0056 T4/T5: evidence, the rubric's post-filter, and machine leaves ─────

/// A step the model has already marked `completed` — the case the whole
/// post-filter exists for. It is also what keeps the TodoGate out of these
/// tests: an open todo makes the turn re-sample, which is a different
/// mechanism entirely.
fn node(id: &str, cmd: &str) -> hotl_types::Todo {
    hotl_types::Todo {
        content: format!("step {id}"),
        status: hotl_types::TodoStatus::Completed,
        id: Some(id.into()),
        validate_cmd: Some(cmd.into()),
        ..Default::default()
    }
}

/// A tool that reports an exit status the way `bash` does, without a shell.
/// `bash` itself resolves against the process-global fsguard root and needs
/// the sandbox floor, neither of which this test is about.
struct FakeBash(i32);

impl Tool for FakeBash {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &str {
        "run a command"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},
                           "required":["command"]})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            ToolOutcome::ok("ran it, printed 180ms").with_facts(hotl_tools::OutcomeFacts {
                exit: Some(self.0),
                matched: None,
            })
        })
    }
}

/// An empty roster plus the fake. Emptied rather than filtered by name:
/// `Registry::builtin` ships a real `bash`, and `register` refuses a
/// duplicate name.
fn ran(exit: i32) -> Registry {
    let mut out = Registry::builtin().filtered(|_| false);
    out.register(Box::new(FakeBash(exit)));
    out
}

/// The gate's whole point (0056 T4): a scripted `met` while a node's
/// `validate_cmd` is red becomes `not_yet`, with the command named.
#[tokio::test]
async fn a_met_verdict_is_refused_while_a_validate_cmd_is_red() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", serde_json::json!({"command": "cargo test"})),
        ScriptedProvider::text_reply("all good, tests pass"),
        ScriptedProvider::text_reply("VERDICT: met\nREASON: the transcript says so"),
    ]));
    let (mut handle, _) = session_with(provider, dir.path(), ran(1));
    handle
        .set_plan_nodes(vec![node("n1", "cargo test")], Vec::new())
        .await;
    handle.set_goal(Some("the work is finished".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let v: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            EngineEvent::GoalVerdict {
                verdict,
                reason,
                evidence,
                ..
            } => Some((*verdict, reason.clone(), evidence.clone())),
            _ => None,
        })
        .collect();
    let (kind, reason, evidence) = v.last().expect("a verdict");
    assert_eq!(
        *kind,
        GoalVerdictKind::NotYet,
        "a red validation must refuse a met: {reason}"
    );
    assert!(reason.contains("cargo test"), "{reason}");
    assert_eq!(evidence, &["n1 `cargo test`: red (exit 1)".to_string()]);
}

/// The control: the same script with the command green passes `met` through.
#[tokio::test]
async fn a_met_verdict_stands_when_every_validation_is_green() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "bash", serde_json::json!({"command": "cargo test"})),
        ScriptedProvider::text_reply("done"),
        ScriptedProvider::text_reply("VERDICT: met\nREASON: cargo test exited 0"),
    ]));
    let (mut handle, _) = session_with(provider, dir.path(), ran(0));
    handle
        .set_plan_nodes(vec![node("n1", "cargo test")], Vec::new())
        .await;
    handle.set_goal(Some("the work is finished".into())).await;
    handle.prompt("go".into()).await;

    let seen = events_until_turn_done(&mut handle).await;
    let (kind, evidence) = seen
        .iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::GoalVerdict {
                verdict, evidence, ..
            } => Some((*verdict, evidence.clone())),
            _ => None,
        })
        .expect("a verdict");
    assert_eq!(kind, GoalVerdictKind::Met);
    assert_eq!(evidence.len(), 1);
    assert!(evidence[0].contains("green"), "{evidence:?}");
}

/// The evaluator reads the harness's own observations, not just the
/// transcript's account of them.
#[tokio::test]
async fn the_evaluator_prompt_carries_the_evidence_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("thinking"),
        ScriptedProvider::text_reply("VERDICT: not_yet\nREASON: nothing ran"),
    ]));
    let (mut handle, _) = session_with(provider.clone(), dir.path(), ran(0));
    handle
        .set_plan_nodes(vec![node("n1", "cargo test -p fx")], Vec::new())
        .await;
    handle.set_goal(Some("the suite is green".into())).await;
    handle.prompt("go".into()).await;
    let _ = events_until_turn_done(&mut handle).await;

    let eval = &provider.requests()[1];
    let text = match eval.items[0].as_ref() {
        Item::User { text, .. } => text.clone(),
        other => panic!("the evaluator prompt is a user item: {other:?}"),
    };
    assert!(text.contains("EVIDENCE"), "{text}");
    assert!(text.contains("n1 `cargo test -p fx`: not run"), "{text}");
    assert!(
        eval.system
            .contains("A claim without a command result is not evidence"),
        "the rubric must reach the evaluator"
    );
}

/// T5: a false machine leaf settles the goal with no evaluator call at all —
/// the scripted provider records exactly one request, the turn's own.
#[tokio::test]
async fn a_false_machine_leaf_calls_no_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("working"),
        ScriptedProvider::text_reply("working still"),
    ]));
    let (mut handle, _) = session_with(provider.clone(), dir.path(), ran(0));
    handle
        .set_goal(Some("file_exists(\"never-written.json\")".into()))
        .await;
    handle.prompt("go".into()).await;
    let seen = events_until_turn_done(&mut handle).await;

    assert_eq!(
        verdicts(&seen).first().map(|(k, _)| *k),
        Some(GoalVerdictKind::NotYet)
    );
    assert!(
        provider
            .requests()
            .iter()
            .all(|r| !r.system.contains("You judge whether an agent session")),
        "a false machine leaf must settle the goal with no evaluator call"
    );
}

/// …and a machine leaf that is true, with no prose left, says met without a
/// model either.
#[tokio::test]
async fn a_true_machine_leaf_meets_the_goal_without_a_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("out.json"), "{}").expect("write");
    let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "wrote it",
    )]));
    let (mut handle, _) = session_with(provider.clone(), dir.path(), ran(0));
    handle
        .set_goal(Some("file_exists(\"out.json\")".into()))
        .await;
    handle.prompt("go".into()).await;
    let seen = events_until_turn_done(&mut handle).await;

    assert_eq!(
        verdicts(&seen).first().map(|(k, _)| *k),
        Some(GoalVerdictKind::Met)
    );
    assert!(
        provider
            .requests()
            .iter()
            .all(|r| !r.system.contains("You judge whether an agent session")),
        "a true machine leaf must meet the goal with no evaluator call"
    );
}
