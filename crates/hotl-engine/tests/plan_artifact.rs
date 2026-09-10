//! The plan artifact (0056 T2): a `todo_write` renders `current.{json,md}`
//! under the project's plan dir, decisions append across calls, and the
//! optional repo mirror carries the markdown only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hotl_engine::plan_state::read_artifact;
use hotl_engine::{
    spawn_session, EngineConfig, EngineEvent, PlanFiles, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Decision, Todo, TodoStatus};

fn todo(content: &str, status: TodoStatus) -> Todo {
    Todo {
        content: content.into(),
        status,
        ..Default::default()
    }
}

fn session(dir: &Path, plan_files: Option<PlanFiles>) -> SessionHandle {
    let config = EngineConfig::default();
    let log = SessionLog::create(dir, &config.model, None, Masker::empty(), 0).expect("log");
    spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )])),
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
        initial_decisions: Vec::new(),
        plan_files,
        initial_goal: None,
        config,
    })
}

/// Drain until the `TodosChanged` that confirms the write landed.
async fn await_todos(handle: &mut SessionHandle) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TodosChanged { .. } = ev {
            return;
        }
    }
}

fn decision(what: &str, why: &str) -> Decision {
    Decision {
        // Zero on the wire: the actor stamps the real time, so the model
        // cannot backdate one. The assertions below prove it did.
        when_ms: 0,
        what: what.into(),
        why: why.into(),
    }
}

#[tokio::test]
async fn todo_write_renders_the_artifact_and_decisions_append() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let plan_dir = data.path().join("plans").join("proj1");
    let repo_mirror = dir.path().join("docs").join("hotl-plan.md");
    let mut handle = session(
        dir.path(),
        Some(PlanFiles {
            project: "proj1".into(),
            dir: plan_dir.clone(),
            repo_mirror: Some(repo_mirror.clone()),
        }),
    );

    handle
        .set_plan_nodes(
            vec![
                todo("build", TodoStatus::Completed),
                todo("test", TodoStatus::InProgress),
            ],
            vec![decision("pinned serde", "the derive broke")],
        )
        .await;
    await_todos(&mut handle).await;

    let a = read_artifact(&plan_dir).expect("artifact");
    assert_eq!(a.version, 1);
    assert_eq!(a.project, "proj1");
    assert!(!a.session.is_empty(), "the artifact names its session");
    assert_eq!(a.nodes.len(), 2);
    assert_eq!(a.decisions.len(), 1);
    assert!(
        a.decisions[0].when_ms > 0,
        "the actor stamps when_ms, not the model"
    );
    let md = std::fs::read_to_string(plan_dir.join("current.md")).expect("md");
    assert!(md.contains("[x] build"), "{md}");
    assert!(md.contains("[~] test"), "{md}");
    assert!(md.contains("pinned serde"), "{md}");
    // The repo mirror is the markdown only — the JSON is harness state.
    assert_eq!(
        std::fs::read_to_string(&repo_mirror).expect("mirror"),
        md,
        "the repo mirror is current.md verbatim"
    );
    assert!(!repo_mirror.with_file_name("current.json").exists());

    // A second write replaces the nodes and APPENDS the decision.
    handle
        .set_plan_nodes(
            vec![todo("test", TodoStatus::Completed)],
            vec![decision("dropped the cache", "it hid the bug")],
        )
        .await;
    await_todos(&mut handle).await;
    let a = read_artifact(&plan_dir).expect("artifact");
    assert_eq!(a.nodes.len(), 1, "nodes are a full-state replace");
    let whats: Vec<&str> = a.decisions.iter().map(|d| d.what.as_str()).collect();
    assert_eq!(whats, ["pinned serde", "dropped the cache"]);
}

/// The nodes stay the truth even when there is nowhere to file them: a
/// session with no plan dir must still work, and write nothing.
#[tokio::test]
async fn without_plan_files_nothing_is_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut handle = session(dir.path(), None);
    handle
        .set_plan_nodes(vec![todo("a", TodoStatus::Pending)], Vec::new())
        .await;
    await_todos(&mut handle).await;
    let stray: Vec<_> = walkdir(dir.path())
        .into_iter()
        .filter(|p| p.ends_with("current.json") || p.ends_with("current.md"))
        .collect();
    assert!(
        stray.is_empty(),
        "wrote an artifact with no plan dir: {stray:?}"
    );
}

/// The artifact survives the process: replay seeds the next session's list
/// and decisions from the durable `Todos` entry, and re-rendering it is
/// idempotent rather than a fresh empty file.
#[tokio::test]
async fn a_resumed_session_re_renders_the_same_plan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let plan_dir = data.path().join("plans").join("proj2");
    let files = || {
        Some(PlanFiles {
            project: "proj2".into(),
            dir: plan_dir.clone(),
            repo_mirror: None,
        })
    };
    let mut handle = session(dir.path(), files());
    handle
        .set_plan_nodes(
            vec![todo("a", TodoStatus::Pending)],
            vec![decision("chose fnv", "no dep")],
        )
        .await;
    await_todos(&mut handle).await;
    let log_path = std::fs::read_dir(dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    drop(handle);

    let replayed = hotl_store::replay(&log_path).expect("replay");
    assert_eq!(replayed.todos.len(), 1);
    assert_eq!(replayed.decisions.len(), 1, "the decisions log replays");
    assert_eq!(replayed.decisions[0].what, "chose fnv");

    // A fresh session seeded from that replay rewrites the same content.
    let dir2 = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log2 =
        SessionLog::create(dir2.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let mut handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "ok",
        )])),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log: log2,
        system: "sys".into(),
        cwd: dir2.path().to_path_buf(),
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: replayed.todos.clone(),
        initial_decisions: replayed.decisions.clone(),
        plan_files: files(),
        initial_goal: None,
        config,
    });
    // Marking the one node done rewrites the artifact; the seeded decision
    // is still there, which is the property a re-seed could silently lose.
    handle
        .set_plan_nodes(vec![todo("a", TodoStatus::Completed)], Vec::new())
        .await;
    await_todos(&mut handle).await;
    let a = read_artifact(&plan_dir).expect("artifact");
    assert_eq!(a.nodes[0].status, TodoStatus::Completed);
    assert_eq!(a.decisions.len(), 1, "a resume must not drop the log");
}

fn walkdir(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// 0057's digest puts "every line of the plan's DECISIONS" on its COPY
/// VERBATIM list, and the model cannot copy what it was not shown. The seam
/// 0057 left (`summarize_prompt`'s `plan_md`) reads this plan's state, so a
/// fold carries the plan and its decisions into the summarize call.
#[tokio::test]
async fn a_fold_shows_the_summarizer_the_plan_and_its_decisions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig {
        model: "test-model".into(),
        // Small enough that the second prompt folds; the cheap rung is off so
        // the fold is what actually happens.
        context_window: 2_000,
        keep_results_turns: 0,
        ..EngineConfig::default()
    };
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("one"),
        ScriptedProvider::text_reply("DIGEST"),
        ScriptedProvider::text_reply("two"),
    ]));
    let mut handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider: provider.clone(),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
        // A projection already past the window, so the very next prompt
        // folds rather than needing a dozen turns to grow into one.
        initial_items: vec![
            hotl_types::Item::User {
                text: "background ".repeat(4_000),
                synthetic: None,
                images: Vec::new(),
            },
            hotl_types::Item::Assistant {
                blocks: vec![serde_json::json!({"type": "text", "text": "noted"})],
            },
        ],
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        config,
    });
    handle
        .set_plan_nodes(
            vec![todo("wire the gate", TodoStatus::InProgress)],
            vec![decision("kept the ULID", "a hash loses ordering")],
        )
        .await;
    await_todos(&mut handle).await;
    handle.prompt("first".into()).await;
    drain_turn(&mut handle).await;

    let summarize = provider
        .requests()
        .into_iter()
        .find(|r| {
            r.system
                .contains("You compress an agent-session transcript")
        })
        .expect("the fold ran a summarize call");
    let text = match summarize.items[0].as_ref() {
        hotl_types::Item::User { text, .. } => text.clone(),
        other => panic!("the summarize prompt is a user item: {other:?}"),
    };
    assert!(text.contains("The session's plan:"), "{text}");
    assert!(text.contains("wire the gate"), "{text}");
    assert!(
        text.contains("kept the ULID — a hash loses ordering"),
        "the decisions log must reach the digest verbatim: {text}"
    );
}

/// Drain to the next `TurnDone`, answering nothing.
async fn drain_turn(handle: &mut SessionHandle) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { .. } = ev {
            return;
        }
    }
}
