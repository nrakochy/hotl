//! T2-2: per-turn safety counters must survive a compaction respawn. A runaway
//! loop is precisely the thing that grows context fastest, so it compacts — and
//! a fold that resets `spent` means `max_turns` cannot bound it.
//! `max_turns_caps_runaway` (hotl-testkit) never compacts, which is why this
//! went unnoticed.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

/// Routes summarize requests (identified by the compaction system prompt) to
/// their own script; everything else goes to the scripted main provider.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
}

impl Provider for Router {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        if req.system.contains("compress") {
            self.summarize.stream(req)
        } else {
            self.main.stream(req)
        }
    }
}

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });
    Session { handle, dir }
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// A one-tool-call sample whose Completed reports a chosen input_tokens — the
/// anchor (A12b) the engine's context estimate builds on.
fn tool_call_reporting(
    id: &str,
    name: &str,
    input: serde_json::Value,
    input_tokens: u64,
) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut script = ScriptedProvider::tool_call(id, name, input);
    if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
        usage.input_tokens = input_tokens;
    }
    script
}

/// Drive the session to its terminal outcome, counting folds along the way.
async fn run_to_done(s: &mut Session) -> (Outcome, usize) {
    let mut folds = 0;
    loop {
        match next_event(s).await {
            EngineEvent::Compacted { .. } => folds += 1,
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => return (outcome, folds),
            _ => {}
        }
    }
}

#[tokio::test]
async fn max_turns_is_enforced_across_a_compaction() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("DIGEST"),
        ScriptedProvider::text_reply("DIGEST 2"),
    ]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize,
    });
    // 4 steps: sample 1, sample 2, the aborted step that requests the fold, and
    // one post-compaction sample. The 5th must never happen.
    let mut s = session(
        provider,
        EngineConfig {
            context_window: 1000,
            max_turns: 4,
            ..Default::default()
        },
    );
    let file = s.dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    // Plenty of scripts: an un-carried `spent` would happily consume them.
    for i in 0..10 {
        main.push_script(tool_call_reporting(
            &format!("t{i}"),
            "read",
            json!({ "path": path }),
            if i == 0 { 650 } else { 850 },
        ));
    }

    s.handle.prompt("run away".into()).await;
    let (outcome, folds) = run_to_done(&mut s).await;

    assert!(folds >= 1, "the fixture must actually compact");
    assert_eq!(outcome, Outcome::TurnLimit);
    assert!(
        main.request_count() <= 4,
        "max_turns must bound the whole logical turn, folds included; got {} samples",
        main.request_count()
    );
}

/// T2-3: `compact_streak` counts folds *without an intervening completed
/// sample*. Three folds with real work between them is a long turn, not a
/// fold-the-digest spiral, and must not be reported as an error.
#[tokio::test]
async fn three_folds_with_progress_do_not_exhaust_the_streak() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(
        (0..6)
            .map(|i| ScriptedProvider::text_reply(&format!("DIGEST {i}")))
            .collect(),
    ));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize,
    });
    let mut s = session(
        provider,
        EngineConfig {
            context_window: 1000,
            max_turns: 20,
            ..Default::default()
        },
    );
    let file = s.dir.path().join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8 path").to_string();
    let call = |i: usize, tokens: u64| {
        tool_call_reporting(&format!("t{i}"), "read", json!({ "path": path }), tokens)
    };
    // 650 arms speculation; each 850 puts the NEXT request over the trigger.
    // After a fold the anchor is stale, so the continuation's first estimate is
    // a full one (small) and it samples before folding again.
    main.push_script(call(0, 650));
    main.push_script(call(1, 850)); // fold 1
    main.push_script(call(2, 850)); // fold 2
    main.push_script(call(3, 850)); // fold 3
    main.push_script(ScriptedProvider::text_reply("done after three folds"));

    s.handle.prompt("a genuinely long turn".into()).await;
    let (outcome, folds) = run_to_done(&mut s).await;

    assert_eq!(folds, 3, "the fixture must fold three times");
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done after three folds".into()
        }
    );
}
