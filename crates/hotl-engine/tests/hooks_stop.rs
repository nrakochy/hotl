//! `Stop` hook (tech-debt #10): a bounded veto at the turn's Done branch.
//! Draws from the SAME shared `turn_extensions` budget the TodoGate uses
//! (index E4) — composed with the TodoGate, a turn can never be extended
//! more than the combined cap, no matter how many gates want to fire.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hotl_engine::hooks::{InProcessHooks, StopDecision};
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Entry, EntryPayload, Item, SyntheticReason, Todo, TodoStatus};

fn logged_payloads(path: &std::path::Path) -> Vec<EntryPayload> {
    std::fs::read_to_string(path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .map(|e| e.payload)
        .collect()
}

async fn run_to_done(handle: &mut hotl_engine::SessionHandle) -> Outcome {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { outcome, .. } = ev {
            return outcome;
        }
    }
}

#[tokio::test]
async fn a_stop_hook_can_block_once_then_allow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    // Two text-only samples: the hook blocks the first, allows the second.
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("not done yet"),
        ScriptedProvider::text_reply("done"),
    ]));
    let fired = Arc::new(AtomicU32::new(0));
    let fired_clone = fired.clone();
    let hooks = InProcessHooks::new().on_stop(move |_outcome| {
        if fired_clone.fetch_add(1, Ordering::SeqCst) == 0 {
            StopDecision::Block {
                reason: "not done: tests unrun".into(),
            }
        } else {
            StopDecision::Allow
        }
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
    let outcome = run_to_done(&mut handle).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        fired.load(Ordering::SeqCst),
        2,
        "hook consulted exactly twice"
    );

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
    assert_eq!(reminders.len(), 1, "exactly one continuation, one reminder");
    let Item::User { text, .. } = reminders[0] else {
        unreachable!()
    };
    assert!(text.contains("not done: tests unrun"), "{text}");
}

#[tokio::test]
async fn an_always_block_stop_hook_composed_with_the_todo_gate_never_exceeds_the_combined_cap() {
    // Unfinished todos would make the TodoGate want to fire on every pass,
    // AND the stop hook always wants to Block — if the two gates had
    // separate budgets, worst-case extensions would be the SUM of their caps
    // (TODO_GATE_MAX=2 + something for stop). They must instead share ONE
    // counter (TURN_EXTENSION_MAX=3 total), so this can never wedge the run.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    // However many times the loop asks, always reply text-only — the
    // scripted provider errors if exhausted, which would itself fail the
    // test, so give it more samples than the combined budget could ever
    // consume (4: 3 extensions + 1 final Done).
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("still working"),
        ScriptedProvider::text_reply("still working"),
        ScriptedProvider::text_reply("still working"),
        ScriptedProvider::text_reply("done"),
    ]));
    let hooks = InProcessHooks::new().on_stop(|_outcome| StopDecision::Block {
        reason: "owner policy: always keep going".into(),
    });
    let mut handle = spawn_session(SessionDeps {
        provider: provider.clone(),
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
    handle
        .set_todos(vec![Todo {
            content: "wire the gate".into(),
            status: TodoStatus::InProgress,
            active_form: None,
        }])
        .await;

    handle.prompt("go".into()).await;
    let outcome = run_to_done(&mut handle).await;
    // The always-Block hook composed with an ever-unfinished TodoGate must
    // still let the turn end — never `TurnLimit`, never an infinite loop.
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    // Exactly 4 samples were drawn from the script (3 extended + 1 final) —
    // proof the combined budget capped it, not an unbounded run.
    assert_eq!(provider.request_count(), 4);

    let entries = logged_payloads(&log_path);
    let reminders = entries
        .iter()
        .filter(|p| {
            matches!(
                p,
                EntryPayload::Item {
                    item: Item::User {
                        synthetic: Some(SyntheticReason::SystemReminder),
                        ..
                    }
                }
            )
        })
        .count();
    assert_eq!(
        reminders, 3,
        "the combined TodoGate + Stop budget must cap at TURN_EXTENSION_MAX (3) total, \
         never the sum of each gate's own bound"
    );
}

#[tokio::test]
async fn no_hooks_means_stop_never_fires_and_todo_gate_is_unaffected() {
    // Regression guard: a session with no hooks at all must behave exactly
    // as before Task 4 landed (the pre-existing TodoGate-only test already
    // covers this shape; this is the belt-and-suspenders companion for the
    // `on_stop` call site specifically).
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
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
    let outcome = run_to_done(&mut handle).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
}
