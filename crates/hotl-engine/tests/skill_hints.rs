//! 0068 T4 — a skill's `paths:` globs point the model at the skill the first
//! time a tool touches a matching path: one `SkillHint` item per skill per
//! session, deduped across turns through the committed projection.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::skills::SkillPathRule;
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, SyntheticReason};
use serde_json::json;

fn session(provider: Arc<ScriptedProvider>, dir: &std::path::Path) -> SessionHandle {
    let rule = SkillPathRule::new("rusty", &["src/**".into()]).unwrap();
    let config = EngineConfig {
        max_turns: 20,
        skill_paths: Arc::from(vec![rule]),
        ..Default::default()
    };
    let log = SessionLog::create(dir, &config.model, None, Masker::empty(), 0).expect("log");
    spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        initial_decisions: Vec::new(),
        plan_files: None,
        config,
    })
}

async fn run(handle: &mut SessionHandle) {
    handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if matches!(ev, EngineEvent::TurnDone { .. }) {
            break;
        }
    }
}

fn hints(provider: &ScriptedProvider) -> Vec<String> {
    let last = provider.requests().pop().expect("at least one request");
    last.items
        .iter()
        .filter_map(|i| match &**i {
            Item::User {
                text,
                synthetic: Some(SyntheticReason::SkillHint),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_touched_path_matching_a_skills_globs_injects_one_hint_per_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(dir.path().join("README.md"), "docs\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        // Outside the globs: no hint yet.
        ScriptedProvider::tool_call("r0", "read", json!({"path": "README.md"})),
        ScriptedProvider::tool_call("r1", "read", json!({"path": "src/a.rs"})),
        // A second match in the same session earns nothing more.
        ScriptedProvider::tool_call("r2", "read", json!({"path": "src/b.rs"})),
        ScriptedProvider::text_reply("done"),
        // Turn two: the hint is in the projection, still exactly one.
        ScriptedProvider::tool_call("r3", "read", json!({"path": "src/a.rs"})),
        ScriptedProvider::text_reply("done again"),
    ]));
    let mut handle = session(Arc::clone(&provider), dir.path());
    run(&mut handle).await;
    let first = hints(&provider);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        first[0].contains("skill-paths=\"rusty\"") && first[0].contains("path=\"src/a.rs\""),
        "{first:?}"
    );
    assert!(first[0].contains("skill{\"name\":\"rusty\"}"), "{first:?}");

    // The README read before the match produced no hint at all.
    let reqs = provider.requests();
    let after_readme = &reqs[1];
    assert!(
        !after_readme.items.iter().any(|i| matches!(
            &**i,
            Item::User {
                synthetic: Some(SyntheticReason::SkillHint),
                ..
            }
        )),
        "no hint before a matching path"
    );

    run(&mut handle).await;
    let second = hints(&provider);
    assert_eq!(second.len(), 1, "deduped across turns: {second:?}");
}
