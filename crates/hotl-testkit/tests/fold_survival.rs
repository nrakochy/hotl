//! What a fold must never lose (0057 T4): the user's own constraints, the
//! decisions, the open validation results and the modified paths.
//!
//! A scripted provider cannot prove what a *model* copies, so the claim under
//! test is the one the harness owns: across three forced folds, every
//! summarize call is shown those things verbatim, and the system prompt puts
//! them on a COPY VERBATIM list. The scripted digest plays a compliant model,
//! which is what carries the constraint from one fold into the next.
//!
//! It also pins `Compaction::source_range` — the log coordinate the digest's
//! trailer promises `recall` can fetch the folded span back from.

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use hotl_engine::{spawn_session, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{Entry, EntryPayload, Item, ToolResultItem};

/// The user's own words. Every digest must be shown them, quoted.
const CONSTRAINT: &str = "MUST: never touch src/legacy.rs";
/// A path the session modified.
const TOUCHED: &str = "crates/foo/src/bar.rs";
/// A validation result that is still open.
const OPEN: &str = "FAILED: 3 tests in crates/foo";
/// A decision, in the shape the digest's DECISIONS section carries.
const DECISION: &str = "chose a ULID over a content hash";

/// A compliant model's digest: it copies the constraint, the decision, the
/// open result and the path, so the next fold is shown them too.
fn digest_reply() -> String {
    format!(
        "GOAL: land the change\\n\
         CONSTRAINTS: \"{CONSTRAINT}\"\\n\
         STATE: {}\\n\
         DECISIONS: {DECISION}\\n\
         FILES: {TOUCHED} modified\\n\
         NEXT: {OPEN}",
        "the work is half done. ".repeat(50)
    )
}

struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
}

impl Provider for Router {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        if req.system.contains("COPY VERBATIM") {
            self.summarize.stream(req)
        } else {
            self.main.stream(req)
        }
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.into(),
        synthetic: None,
        images: Vec::new(),
    }
}

/// One finished turn: the user's constraint, a tool call, and a result that
/// names the touched path and the open validation result. Big enough that the
/// next prompt trips the 80% fold.
fn seeded() -> Vec<Item> {
    vec![
        user(&format!("{CONSTRAINT}. Land the change.")),
        Item::Assistant {
            blocks: vec![serde_json::json!({
                "type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "cargo test"}
            })],
        },
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: "t1".into(),
                content: format!("modified {TOUCHED}\\n{OPEN}\\n{}", "y".repeat(2_100)),
                is_error: false,
            }],
        },
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": DECISION})],
        },
    ]
}

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
    summarize: Arc<ScriptedProvider>,
}

fn session() -> Session {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(Vec::new()));
    // Generous: the speculative digest and the inline fold each draw one.
    for _ in 0..12 {
        main.push_script(ScriptedProvider::text_reply("carrying on"));
        summarize.push_script(ScriptedProvider::text_reply(&digest_reply()));
    }
    let config = EngineConfig {
        model: "test-model".into(),
        context_window: 2_000,
        // The fold is the subject; the cheap rung would reclaim the same
        // tokens first and there would be nothing left to fold.
        keep_results_turns: 0,
        max_turns: 20,
        ..Default::default()
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        provider: Arc::new(Router {
            main,
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
        initial_goal: None,
        config,
    });
    Session {
        handle,
        dir,
        summarize,
    }
}

async fn run(s: &mut Session, prompt: &str) -> Outcome {
    s.handle.prompt(prompt.into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        if let EngineEvent::TurnDone { outcome, .. } = ev {
            return outcome;
        }
    }
}

fn entries(s: &Session) -> Vec<Entry> {
    let path = std::fs::read_dir(s.dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    std::fs::read_to_string(path)
        .expect("read log")
        .lines()
        .map(|l| serde_json::from_str(l).expect("parse entry"))
        .collect()
}

#[tokio::test]
async fn constraints_and_decisions_reach_every_digest_and_the_span_is_named() {
    let mut s = session();
    // Long prompts: each one re-trips the 80% trigger after the last fold.
    let filler = "please keep going with the work described above. ".repeat(50);
    for n in 0..3 {
        let outcome = run(&mut s, &format!("step {n}. {filler}")).await;
        assert!(
            matches!(outcome, Outcome::Done { .. }),
            "step {n}: {outcome:?}"
        );
    }

    // Every summarize call was shown the four things the system prompt puts
    // on the COPY VERBATIM list.
    let prompts: Vec<String> = s
        .summarize
        .requests()
        .iter()
        .flat_map(|r| r.items.iter().cloned().collect::<Vec<_>>())
        .filter_map(|i| match &*i {
            Item::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        prompts.len() >= 3,
        "three folds summarize: {}",
        prompts.len()
    );
    for (n, prompt) in prompts.iter().enumerate() {
        assert!(prompt.contains(CONSTRAINT), "fold {n} lost the constraint");
        assert!(prompt.contains(TOUCHED), "fold {n} lost the modified path");
        assert!(prompt.contains(OPEN), "fold {n} lost the open result");
        assert!(prompt.contains(DECISION), "fold {n} lost the decision");
    }
    // And the system prompt actually asks for them verbatim.
    let system = &s.summarize.requests()[0].system;
    for needle in ["COPY VERBATIM", "CONSTRAINTS:", "DECISIONS:", "MUST"] {
        assert!(system.contains(needle), "{needle} missing from {system}");
    }

    // Each fold names the log span its digest was computed from.
    let folds: Vec<(bool, Option<(String, String)>)> = entries(&s)
        .into_iter()
        .filter_map(|e| match e.payload {
            EntryPayload::Compaction {
                degraded,
                source_range,
                ..
            } => Some((degraded, source_range)),
            _ => None,
        })
        .collect();
    assert!(folds.len() >= 3, "three folds landed: {}", folds.len());
    for (n, (degraded, range)) in folds.iter().enumerate() {
        assert!(!degraded, "fold {n} degraded — the script ran dry");
        let (first, last) = range.clone().unwrap_or_else(|| panic!("fold {n} range"));
        assert!(!first.is_empty() && !last.is_empty(), "fold {n}");
    }
    // Successive folds cover successive spans: each starts where the last
    // one's compaction entry sits.
    for pair in folds.windows(2) {
        let a = pair[0].1.clone().expect("range");
        let b = pair[1].1.clone().expect("range");
        assert!(b.0 > a.0, "spans must advance: {a:?} then {b:?}");
    }

    // The digest tells the model where the folded span went.
    let digests: Vec<String> = entries(&s)
        .into_iter()
        .filter_map(|e| match e.payload {
            EntryPayload::Compaction { digest, .. } => Some(digest),
            _ => None,
        })
        .flatten()
        .filter_map(|i| match i {
            Item::User { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(digests
        .iter()
        .all(|d| d.contains("retrievable with recall (backend session-log)")));
}
