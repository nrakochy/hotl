//! 0075 T2 — the digest's FILES section is computed from the folded tool
//! calls and enforced: the summarize request carries the harness's block, the
//! main prefix never does, and a digest that drops a path gets it back.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Item, ToolResultItem};
use serde_json::json;

type BoxStream = futures_util::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>;

/// Routes the compaction summarize to its own script.
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

fn call(id: &str, name: &str, path: &str) -> Item {
    Item::Assistant {
        blocks: vec![json!({"type": "tool_use", "id": id, "name": name, "input": {"path": path}})],
        model: None,
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

/// A finished turn that read `src/a.rs`, edited `src/b.rs`, wrote `src/c.rs`
/// and then read a haystack; the big result is what forces the fold on the
/// next sample, and it comes last so the fold's verbatim tail cannot keep
/// the three touches out of the folded span.
fn seeded() -> Vec<Item> {
    vec![
        Item::User {
            text: "first".into(),
            synthetic: None,
            images: Vec::new(),
        },
        call("t1", "read", "src/a.rs"),
        result("t1", "fn a() {}"),
        call("t2", "edit", "src/b.rs"),
        result("t2", "ok"),
        call("t3", "write", "src/c.rs"),
        result("t3", "ok"),
        call("t4", "read", "src/big.rs"),
        result("t4", &"y".repeat(2_600)),
        Item::Assistant {
            blocks: vec![json!({"type": "text", "text": "done"})],
            model: None,
        },
    ]
}

struct Session {
    handle: SessionHandle,
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    log_path: std::path::PathBuf,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(digests: Vec<&str>) -> Session {
    let main = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "after the fold",
    )]));
    let summarize = Arc::new(ScriptedProvider::new(
        digests
            .into_iter()
            .map(ScriptedProvider::text_reply)
            .collect(),
    ));
    let config = EngineConfig {
        model: "test-model".into(),
        context_window: 1_000,
        keep_results_turns: 0,
        max_turns: 10,
        ..Default::default()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let log_path = log.path().to_path_buf();
    let handle = spawn_session(SessionDeps {
        provider: Arc::new(Router {
            main: Arc::clone(&main),
            summarize: Arc::clone(&summarize),
        }),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(Rules::default()),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
        hooks: None,
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
        main,
        summarize,
        log_path,
        dir,
    }
}

async fn run(s: &mut Session) -> (Outcome, bool) {
    s.handle.prompt("second".into()).await;
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

const INTRO: &str = "Files touched in this span";

#[tokio::test]
async fn the_summarize_request_carries_the_block_and_the_main_prefix_does_not() {
    let full = "GOAL: g\nFILES:\n- src/a.rs — read\n- src/b.rs — edited (1×)\n- src/c.rs — written\n- src/big.rs — read\nNEXT: n";
    let mut s = session(vec![full, full]);
    let (outcome, compacted) = run(&mut s).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(compacted);
    let digest_calls = s.summarize.requests();
    assert!(!digest_calls.is_empty());
    for req in &digest_calls {
        let text = req
            .items
            .iter()
            .filter_map(|i| match &**i {
                Item::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(INTRO), "{text}");
        assert!(text.contains("- src/a.rs — read"), "{text}");
        assert!(text.contains("- src/b.rs — edited (1×)"), "{text}");
        assert!(text.contains("- src/c.rs — written"), "{text}");
    }
    for req in s.main.requests() {
        for item in req.items.iter() {
            if let Item::User { text, .. } = &**item {
                assert!(
                    !text.contains(INTRO),
                    "the main prefix must not carry the block: {text}"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_digest_that_drops_a_path_gets_it_back() {
    // The model's FILES names two of three paths; `src/b.rs` is missing.
    let dropped = "GOAL: g\nSTATE: s\nFILES: read src/a.rs, wrote src/c.rs\nNEXT: n";
    let mut s = session(vec![dropped, dropped]);
    let (outcome, compacted) = run(&mut s).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(compacted);
    // The digest item the model saw after the fold carries the harness's block.
    let last = s
        .main
        .requests()
        .last()
        .expect("a request after the fold")
        .clone();
    let digest = last
        .items
        .iter()
        .find_map(|i| match &**i {
            Item::User { text, .. } if text.contains("<compaction-summary>") => Some(text.clone()),
            _ => None,
        })
        .expect("the digest item");
    assert!(digest.contains("- src/b.rs — edited (1×)"), "{digest}");
    assert!(
        digest.contains("FILES:\n- src/a.rs — read"),
        "replaced, not appended: {digest}"
    );
    assert!(
        !digest.contains("read src/a.rs, wrote src/c.rs"),
        "{digest}"
    );
    // And the log's `compaction` entry holds the enforced text.
    let log = std::fs::read_to_string(&s.log_path).unwrap();
    let entry = log
        .lines()
        .find(|l| l.contains("\"compaction\""))
        .expect("a compaction entry");
    assert!(entry.contains("src/b.rs"), "{entry}");
}
