//! `PreCompact` / `PostCompact` (0057 T3): a hook can keep a named tool
//! result verbatim through a fold, and sees the digest after one.
//!
//! The bound — a hung hook folds with no pins — is proven at
//! `hooks::call_pre_compact` instead of here: a paused clock is the only way
//! to skip `HOOK_CALL_TIMEOUT` without a 15-second test, and tokio's
//! auto-advance fires every other deadline in the file the moment the store's
//! real writer thread makes the runtime look idle.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hotl_engine::hooks::{CompactInfo, Hooks, InProcessHooks, PreCompactDecision};
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, ToolResultItem};

/// Routes the compaction summarize to its own script, so the main script
/// isn't consumed by housekeeping.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
}

impl Provider for Router {
    fn stream(&self, req: SamplingRequest) -> BoxStream {
        if req.system.contains("compress") {
            self.summarize.stream(req)
        } else {
            self.main.stream(req)
        }
    }
}

type BoxStream = futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>;

/// A window small enough that the seeded history folds on the first sample.
fn config() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        context_window: 1_000,
        // The fold is what this file is about; the cheap rung would reclaim
        // the same tokens first and there would be nothing left to pin.
        keep_results_turns: 0,
        max_turns: 10,
        ..Default::default()
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.into(),
        synthetic: None,
        images: Vec::new(),
    }
}

fn call(id: &str) -> Item {
    Item::Assistant {
        blocks: vec![serde_json::json!({
            "type": "tool_use", "id": id, "name": "bash", "input": {"command": "ls"}
        })],
    }
}

fn result(id: &str, content: &str) -> Item {
    Item::ToolResults {
        results: vec![ToolResultItem {
            tool_use_id: id.into(),
            content: content.into(),
            is_error: false,
        }],
    }
}

/// Two finished turns. `t1`'s result is small and worth keeping — the thing a
/// hook pins; `t2`'s is the haystack that forces the fold. Deliberately not
/// one item: a pinned result rides through *after* the digest, so a pin big
/// enough to re-trip the trigger would just fold again on the next sample.
fn seeded() -> Vec<Item> {
    vec![
        user("first"),
        call("t1"),
        result("t1", "THE-ONE-THING-WORTH-KEEPING"),
        call("t2"),
        result("t2", &"y".repeat(2_600)),
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "listed"})],
        },
        user("second"),
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "noted"})],
        },
    ]
}

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
    provider: Arc<ScriptedProvider>,
}

fn session(hooks: Arc<dyn Hooks>) -> Session {
    let main = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "after the fold",
    )]));
    // Two: the speculative digest fires at 60% and the inline fold at 80%,
    // and whichever runs must not degrade for want of a script.
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("GOAL: keep going"),
        ScriptedProvider::text_reply("GOAL: keep going"),
    ]));
    let config = config();
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        provider: Arc::new(Router {
            main: Arc::clone(&main),
            summarize,
        }),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        hooks: Some(hooks),
        initial_items: seeded(),
        initial_todos: Vec::new(),
        initial_decisions: Vec::new(),
        plan_files: None,
        initial_goal: None,
        concurrency: Default::default(),
        config,
    });
    Session {
        handle,
        dir,
        provider: main,
    }
}

/// Drive one prompt to its outcome, reporting whether a fold happened.
async fn run(s: &mut Session, prompt: &str) -> (Outcome, bool) {
    s.handle.prompt(prompt.into()).await;
    let mut compacted = false;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Compacted { .. } => compacted = true,
            EngineEvent::TurnDone { outcome, .. } => return (outcome, compacted),
            _ => {}
        }
    }
}

/// The items the model saw on the request after the fold.
fn post_fold_items(s: &Session) -> Vec<Item> {
    let requests = s.provider.requests();
    let last = requests.last().expect("a request was made");
    last.items.iter().map(|i| (**i).clone()).collect()
}

#[tokio::test]
async fn an_in_process_pre_compact_hook_keeps_a_pinned_item_verbatim() {
    let seen: Arc<Mutex<Vec<CompactInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let hooks = InProcessHooks::new().on_pre_compact(move |info| {
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(info.clone());
        PreCompactDecision {
            pins: vec!["t1".into()],
        }
    });
    let mut s = session(Arc::new(hooks));
    let (outcome, compacted) = run(&mut s, "third").await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "after the fold".into()
        }
    );
    assert!(compacted, "the seeded history must fold");

    let info = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .first()
        .cloned()
        .expect("the hook fired");
    assert_eq!(
        info.folded_ids,
        vec!["t1".to_string(), "t2".to_string()],
        "{info:?}"
    );
    assert!(info.kept_from > 0, "{info:?}");
    assert!(info.estimate_pct >= 60, "the window was full: {info:?}");

    let items = post_fold_items(&s);
    let texts: Vec<&str> = items
        .iter()
        .filter_map(|i| match i {
            Item::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("<pinned tool_use_id=\"t1\"")
                && t.contains("THE-ONE-THING-WORTH-KEEPING")),
        "the pinned result rides through the fold verbatim: {texts:?}"
    );
    // And it sits right after the digest, not in the folded span's old slot.
    let digest = texts
        .iter()
        .position(|t| t.contains("<compaction-summary>"))
        .expect("a digest was appended");
    let pinned = texts
        .iter()
        .position(|t| t.contains("<pinned"))
        .expect("a pin was appended");
    assert_eq!(pinned, digest + 1, "{texts:?}");
    // Replay agrees with the live head: the entry names what was pinned.
    let log = std::fs::read_to_string(
        std::fs::read_dir(s.dir.path())
            .expect("session dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .expect("session log"),
    )
    .expect("read log");
    assert!(log.contains("\"pinned\":[\"t1\"]"), "the entry records it");
}

#[tokio::test]
async fn post_compact_receives_the_digest() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let hooks = InProcessHooks::new().on_post_compact(move |digest| {
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(digest.to_string());
    });
    let mut s = session(Arc::new(hooks));
    let (_, compacted) = run(&mut s, "third").await;
    assert!(compacted);
    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(seen, vec!["GOAL: keep going".to_string()]);
}
