//! Task 8 review, IMPORTANT 1: a `StaleEpoch` reject must not be treated
//! like a real commit. Drives the actor's real `run()` command loop (not a
//! hand-called `commit_prepared`) through a reject → re-mask → retry round
//! trip, using the `#[doc(hidden)] SessionCmd::BumpRulesEpoch` test hook,
//! and asserts the held steer lands AFTER the retried assistant item —
//! never before it (the transcript-inversion bug 72a6f1b exists to prevent).

use std::sync::Arc;

use hotl_engine::{
    event_channel, session_channel, spawn_session_with_channels, EngineConfig, PreparedEntry,
    ProposeReply, SessionCmd, SessionDeps,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item, SyntheticReason};
use tokio::sync::oneshot;

fn deps(dir: &std::path::Path, log: SessionLog, config: EngineConfig) -> SessionDeps {
    SessionDeps {
        provider: Arc::new(ScriptedProvider::new(vec![])),
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
        config,
    }
}

fn assistant_payload() -> EntryPayload {
    EntryPayload::Item {
        item: Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "hi"})],
        },
    }
}

#[tokio::test]
async fn a_stale_epoch_reject_holds_the_steer_until_the_retried_commit_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();

    let (cmd_tx, cmd_rx) = session_channel();
    let (event_tx, event_rx) = event_channel();
    let handle = spawn_session_with_channels(
        deps(dir.path(), log, config),
        cmd_tx.clone(),
        cmd_rx,
        event_tx,
        event_rx,
        hotl_engine::hooks::NotificationDrain::new(),
    );

    // Sample boundary: grants a snapshot, which starts the `sampling` hold a
    // real `Turn` would be inside for the duration of its request build —
    // exactly the window a steer must not jump ahead of.
    let (tx, rx) = oneshot::channel();
    cmd_tx
        .send(SessionCmd::Snapshot { reply: tx })
        .await
        .expect("send snapshot");
    rx.await.expect("snapshot reply");

    // A steer arrives mid-sample: held, not appended (sampling=true).
    handle.steer("hold me".into()).await;

    // The masking rules advance (test-only hook) — an entry already stamped
    // with the old epoch is now stale, exactly what a real re-mask race
    // would produce.
    cmd_tx
        .send(SessionCmd::BumpRulesEpoch)
        .await
        .expect("send bump");

    // First attempt, built at the now-stale epoch 0: rejected.
    let stale = hotl_store::prepare_payload(&assistant_payload(), &Masker::empty(), 0)
        .expect("prepare stale");
    let stale_entry = PreparedEntry::new(
        stale,
        Some(Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "hi"})],
        }),
    );
    let (tx, rx) = oneshot::channel();
    cmd_tx
        .send(SessionCmd::ProposePrepared {
            entries: vec![stale_entry],
            reply: tx,
        })
        .await
        .expect("send stale propose");
    assert_eq!(
        rx.await.expect("stale reply"),
        ProposeReply::StaleEpoch,
        "an entry stamped with the old epoch must be rejected"
    );

    // Nothing landed: the log is still just the header, and the held steer
    // is still held — this is the bug under test. Before the fix, the
    // actor's `ProposePrepared` handler flipped `sampling` false and
    // released the steer on this very reply, even though nothing committed.
    let after_reject = std::fs::read_to_string(&log_path).expect("read log");
    assert_eq!(
        after_reject.lines().count(),
        1,
        "only the header must be on disk after a stale reject: {after_reject}"
    );

    // Retry, built at the now-current epoch 1: commits.
    let fresh = hotl_store::prepare_payload(&assistant_payload(), &Masker::empty(), 1)
        .expect("prepare fresh");
    let fresh_entry = PreparedEntry::new(
        fresh,
        Some(Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "hi"})],
        }),
    );
    let (tx, rx) = oneshot::channel();
    cmd_tx
        .send(SessionCmd::ProposePrepared {
            entries: vec![fresh_entry],
            reply: tx,
        })
        .await
        .expect("send fresh propose");
    assert_eq!(
        rx.await.expect("fresh reply"),
        ProposeReply::Committed,
        "the retried entry, stamped with the current epoch, must commit"
    );

    // The held steer must now have been released — but strictly AFTER the
    // assistant item, never before it (the transcript-inversion bug).
    drop(cmd_tx); // let the actor's command loop end cleanly
    handle.finish(std::time::Duration::from_millis(500)).await;

    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert_eq!(replayed.warnings, Vec::<String>::new(), "clean log");
    assert_eq!(
        replayed.items.len(),
        2,
        "assistant item then the released steer: {:?}",
        replayed.items
    );
    match &replayed.items[0] {
        Item::Assistant { .. } => {}
        other => panic!("item 0 must be the retried assistant item, got {other:?}"),
    }
    match &replayed.items[1] {
        Item::User {
            synthetic: Some(SyntheticReason::Steer),
            text,
            ..
        } => assert_eq!(text, "hold me"),
        other => panic!("item 1 must be the released steer, got {other:?}"),
    }
}
