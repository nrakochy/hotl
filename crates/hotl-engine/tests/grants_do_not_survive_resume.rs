//! 0055 T3, the agent-loop floor: a session-scoped grant dies with the
//! session. `AskReply::AllowWithSecretReads` (plan 0022) lifts the credential
//! read-deny for exactly one `Tool::run` future, via a task-local — it is
//! never written to the log, so nothing can replay it. That is the whole
//! reason it is out of band, and it is stated here rather than left to the
//! shape of the code: a grant that survived a resume would be a standing
//! permission the human granted once, for one command, years of turns ago.
//!
//! Both continuations are covered, because they are the two ways a projection
//! outlives its session: replaying the same log, and forking a child from it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, ParentRef, SessionLog};
use hotl_tools::{rules::Rules, Permission, Registry, Tool, ToolOutcome};
use hotl_types::Item;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Reports whether the credential read-deny was lifted for its own call —
/// the observable half of the grant, read the same way `sandbox` reads it.
struct SecretProbe {
    granted: Arc<AtomicBool>,
}

impl Tool for SecretProbe {
    fn name(&self) -> &'static str {
        "peek"
    }
    fn description(&self) -> &str {
        "reports whether the secret-read grant is in scope"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &Value) -> Permission {
        Permission::Ask {
            summary: "peek".into(),
        }
    }
    fn run<'a>(
        &'a self,
        _input: Value,
        _cancel: CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let lifted = hotl_tools::sandbox::SECRET_READS
                .try_with(|g| *g)
                .unwrap_or(false);
            self.granted.store(lifted, Ordering::SeqCst);
            ToolOutcome::ok(format!("lifted={lifted}"))
        })
    }
}

struct Ran {
    /// Whether the grant was in scope inside the tool call.
    granted: bool,
    /// Whether a human was asked at all — a remembered grant would show up
    /// here first, as a call that never prompted.
    asked: bool,
    log_path: std::path::PathBuf,
    items: Vec<Item>,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

/// One scripted `peek` call in a fresh session seeded with `initial_items`,
/// answered with `answer`.
async fn run_once(initial_items: Vec<Item>, parent: Option<ParentRef>, answer: AskReply) -> Ran {
    let granted = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::builtin();
    registry.register(Box::new(SecretProbe {
        granted: granted.clone(),
    }));

    let config = EngineConfig::default();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, parent, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "peek", json!({})),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut handle: SessionHandle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(registry),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: true,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        initial_items,
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
        concurrency: Default::default(),
    });
    handle.prompt("go".into()).await;

    let mut asked = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                asked = true;
                let _ = reply.send(answer.clone());
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    drop(handle);
    let items = hotl_store::replay(&log_path).expect("replay").items;
    Ran {
        granted: granted.load(Ordering::SeqCst),
        asked,
        log_path,
        items,
        dir,
    }
}

#[tokio::test]
async fn grants_do_not_survive_resume() {
    let first = run_once(Vec::new(), None, AskReply::AllowWithSecretReads).await;
    // Non-vacuous: the probe really can see the grant, so a `false` below is
    // the grant being absent and not the probe being broken.
    assert!(first.asked, "the first call must prompt");
    assert!(
        first.granted,
        "the grant must reach the tool it was given for"
    );
    // …and nothing about it reached the log, which is why nothing can replay
    // it. Asserted on the bytes: the grant has no `EntryPayload` of its own,
    // so there is no typed absence to state instead.
    let bytes = std::fs::read_to_string(&first.log_path).expect("read log");
    for trace in ["SecretReads", "secret_reads", "secret-reads"] {
        assert!(!bytes.contains(trace), "the log carries {trace}: {bytes}");
    }
    assert!(
        !first.items.is_empty(),
        "the fixture must produce a projection to resume from"
    );

    // Resume: the same projection, a new actor. The human is asked again, and
    // a plain allow is a plain allow — no lift carried forward.
    let resumed = run_once(first.items.clone(), None, AskReply::Allow).await;
    assert!(resumed.asked, "a resumed session must ask again");
    assert!(!resumed.granted, "the secret-read grant survived a resume");

    // Fork: a child seeded from the same projection, with real lineage.
    let parent_id = hotl_store::replay(&first.log_path)
        .expect("replay")
        .header
        .session_id;
    let forked = run_once(
        first.items,
        Some(ParentRef {
            session_id: parent_id,
            tip_entry_id: None,
        }),
        AskReply::Allow,
    )
    .await;
    assert!(forked.asked, "a forked session must ask again");
    assert!(!forked.granted, "the secret-read grant survived a fork");
}
