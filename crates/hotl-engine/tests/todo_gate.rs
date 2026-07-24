//! The bounded TodoGate (01 §agent-loop, verbatim shape): a text-only reply
//! with unfinished todos still open gets intercepted and nudged, but only up
//! to `TODO_GATE_MAX` (2) times per prompt — after that the turn ends
//! normally. This is the anti-wedge guarantee: the gate must never be able
//! to keep an unattended run spinning forever.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, SyntheticReason, Todo, TodoStatus};

#[tokio::test]
async fn the_gate_fires_at_most_twice_then_lets_the_turn_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    // Three text-only samples: the gate should intercept the first two
    // (unfinished todo still open) and let the third end the turn.
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("still working"),
        ScriptedProvider::text_reply("still working"),
        ScriptedProvider::text_reply("done"),
    ]));
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

    let outcome = loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { outcome, .. } = ev {
            break outcome;
        }
    };
    // The gate let the third (final) sample end the turn as `Done`, not
    // `TurnLimit` or anything else — the cap didn't wedge the run.
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");

    // Exactly three samples were made: two nudged-and-continued, one final.
    // (Nothing to assert directly on request count here without the
    // provider handle — the reminder count below is the real proof.)

    // The projection carries exactly two SystemReminder nudges (the cap),
    // never a third — and never the raw `<todos>`-tagged item itself (that
    // only ever rides the ephemeral snapshot, per the previous task).
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
    assert_eq!(reminders.len(), 2, "expected exactly TODO_GATE_MAX nudges");
    for r in reminders {
        let Item::User { text, .. } = r else {
            unreachable!()
        };
        assert!(text.contains("todo"), "nudge text: {text}");
    }
    assert!(
        !replayed.items.iter().any(|i| matches!(
            i,
            Item::User {
                synthetic: Some(SyntheticReason::Todos),
                ..
            }
        )),
        "the raw todo reminder must never land in the durable projection"
    );
}
