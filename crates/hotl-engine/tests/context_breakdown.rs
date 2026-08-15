//! `SessionCmd::ContextBreakdown` (plan 0028) — the one command that reads
//! and returns. It appends nothing, publishes nothing and awaits nothing,
//! which is exactly what makes `/context` safe to run mid-turn.

use std::sync::Arc;

use hotl_engine::{spawn_session, spawn_session_with_channels, EngineConfig, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{ContextBreakdown, ContextKind, Item, SyntheticReason};

/// A stand-in for the `skill` and `spawn` tools. `Registry::builtin()` carries
/// neither — the CLI registers them, because one needs a skills roster on disk
/// and the other a child-session factory — but the name split is exactly what
/// the tool rows are about, so the test supplies its own.
struct Roster(&'static str);

impl hotl_tools::Tool for Roster {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &str {
        "available entries:\n- one\n- two\n- three\n"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}})
    }
    fn permission(&self, _input: &serde_json::Value) -> hotl_tools::Permission {
        hotl_tools::Permission::None
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
        Box::pin(async { hotl_tools::ToolOutcome::ok("") })
    }
}

/// `builtin()` plus the two roster-bearing tools the CLI adds.
fn registry_with_rosters() -> Registry {
    let mut registry = Registry::builtin();
    registry.register(Box::new(Roster("skill")));
    registry.register(Box::new(Roster("spawn")));
    registry
}

fn deps(dir: &std::path::Path, log: SessionLog, config: EngineConfig) -> SessionDeps {
    SessionDeps {
        provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )])),
        registry: Arc::new(registry_with_rosters()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "you are hotl".into(),
        cwd: dir.to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: vec![
            Item::User {
                text: "hello there".into(),
                synthetic: None,
                images: Vec::new(),
            },
            Item::User {
                text: "<project-instructions>rules</project-instructions>".into(),
                synthetic: Some(SyntheticReason::ProjectInstructions),
                images: Vec::new(),
            },
        ],
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    }
}

/// The same three strings the engine bills per tool definition.
fn defs_estimate(registry: &Registry) -> u64 {
    registry
        .defs()
        .iter()
        .map(|d| {
            hotl_context::tokens::estimate_text(&d.name)
                + hotl_context::tokens::estimate_text(&d.description)
                + hotl_context::tokens::estimate_text(&d.input_schema.to_string())
        })
        .sum()
}

fn row(b: &ContextBreakdown, kind: ContextKind) -> u64 {
    b.rows
        .iter()
        .find(|r| r.kind == kind)
        .expect("every row is emitted")
        .tokens
}

#[tokio::test]
async fn a_context_breakdown_reads_without_appending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(deps(dir.path(), log, config.clone()));

    let before = std::fs::read_to_string(&log_path).expect("log");
    let epoch_before = handle.head().borrow().epoch();

    let b = handle.context_breakdown().await.expect("breakdown");
    assert_eq!(b.window, config.context_window);
    assert!(row(&b, ContextKind::Messages) > 0);
    assert!(row(&b, ContextKind::ProjectInstructions) > 0);

    assert_eq!(
        before,
        std::fs::read_to_string(&log_path).expect("log"),
        "a breakdown must not reach the log"
    );
    assert_eq!(
        epoch_before,
        handle.head().borrow().epoch(),
        "a breakdown must not advance the projection"
    );
}

#[tokio::test]
async fn the_breakdown_names_the_skill_and_spawn_tools_separately() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let handle = spawn_session(deps(dir.path(), log, config));

    let b = handle.context_breakdown().await.expect("breakdown");
    let schemas = row(&b, ContextKind::ToolSchemas);
    let skills = row(&b, ContextKind::SkillsRoster);
    let agents = row(&b, ContextKind::AgentsRoster);
    assert!(
        skills > 0,
        "the skill tool carries its roster in the schema"
    );
    assert!(agents > 0, "so does spawn");
    assert!(schemas > 0);

    // The two rosters are lifted OUT of the schema total, not copied out of
    // it: the schema row matches a registry without them, and the three rows
    // together match one with them.
    assert_eq!(schemas, defs_estimate(&Registry::builtin()));
    assert_eq!(
        schemas + skills + agents,
        defs_estimate(&registry_with_rosters())
    );
}

#[tokio::test]
async fn a_dropped_reply_channel_does_not_wedge_the_actor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
    let (event_tx, event_rx) = hotl_engine::event_channel();
    let handle = spawn_session_with_channels(
        deps(dir.path(), log, config),
        cmd_tx.clone(),
        cmd_rx,
        event_tx,
        event_rx,
        hotl_engine::hooks::NotificationDrain::new(),
    );

    // A client that hung up before the actor got to its command.
    let (reply, rx) = tokio::sync::oneshot::channel();
    drop(rx);
    cmd_tx
        .send(hotl_engine::SessionCmd::ContextBreakdown { reply })
        .await
        .expect("send");

    // The next command still gets an answer.
    let b = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        handle.context_breakdown(),
    )
    .await
    .expect("the actor is still servicing commands")
    .expect("breakdown");
    assert!(!b.rows.is_empty());
}
