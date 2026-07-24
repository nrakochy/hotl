//! Speculative compaction: once the context estimate crosses the speculation
//! threshold, the digest summarize runs concurrently with the turn's samples,
//! so hitting the compaction trigger folds without a blocking model call.
//! Under tokio's paused clock, per-request delays make overlap measurable
//! deterministically: overlapped sleeps advance once, serial sleeps add up.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ProviderError, SamplingRequest, ScriptedProvider, StreamEvent};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use hotl_types::{EntryPayload, Item};
use serde_json::json;

/// Routes summarize requests (identified by the compaction system prompt) to
/// their own scripts, delaying every stream by `delay_ms`.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    delay_ms: u64,
}

impl Provider for Router {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let inner = if req.system.contains("compress") {
            Arc::clone(&self.summarize)
        } else {
            Arc::clone(&self.main)
        };
        let delay = Duration::from_millis(self.delay_ms);
        Box::pin(
            futures_util::stream::once(async move {
                tokio::time::sleep(delay).await;
                inner.stream(req)
            })
            .flatten(),
        )
    }
}

struct Session {
    handle: SessionHandle,
    log_path: std::path::PathBuf,
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, config: EngineConfig) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let log_path = log.path().to_path_buf();
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
        config,
    });
    Session {
        handle,
        log_path,
        dir,
    }
}

async fn wait_done(s: &mut Session) -> Outcome {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(60), s.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { outcome, .. } => return outcome,
            _ => {}
        }
    }
}

/// The persisted compaction entry's digest text and degraded flag.
fn compaction_digest(log_path: &Path) -> Option<(String, bool)> {
    for line in std::fs::read_to_string(log_path).expect("read log").lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("parse entry");
        if let EntryPayload::Compaction {
            digest, degraded, ..
        } = entry.payload
        {
            let text = digest
                .iter()
                .filter_map(|i| match i {
                    Item::User { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Some((text, degraded));
        }
    }
    None
}

/// A one-tool-call sample whose Completed reports a chosen input_tokens —
/// the anchor (A12b) the engine's context estimate builds on.
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

fn cfg() -> EngineConfig {
    // Window 1000: speculation fires past 600 estimated tokens, compaction
    // past 800. Sample 1 reports 650 (speculate), sample 2 reports 850 (fold).
    EngineConfig {
        context_window: 1000,
        max_turns: 10,
        ..Default::default()
    }
}

/// Wire a crossing-the-thresholds session: two read samples whose reported
/// usage walks the estimate 650 → 850, then a continuation reply.
fn push_main_scripts(main: &ScriptedProvider, dir: &Path) {
    let file = dir.join("f.txt");
    std::fs::write(&file, "small file body").expect("fixture");
    let path = file.to_str().expect("utf8 path");
    main.push_script(tool_call_reporting(
        "t1",
        "read",
        json!({"path": path}),
        650,
    ));
    main.push_script(tool_call_reporting(
        "t2",
        "read",
        json!({"path": path}),
        850,
    ));
    main.push_script(ScriptedProvider::text_reply("done after compaction"));
}

#[tokio::test(start_paused = true)]
async fn speculative_digest_overlaps_the_turn() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("GOAL: SPEC DIGEST of the early work"),
        ScriptedProvider::text_reply("LATE DIGEST"),
    ]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
        delay_ms: 300,
    });
    let mut s = session(provider, cfg());
    push_main_scripts(&main, s.dir.path());

    let t0 = tokio::time::Instant::now();
    s.handle.prompt("start the long task".into()).await;
    let outcome = wait_done(&mut s).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done after compaction".into()
        }
    );

    // Three main samples at 300ms each; the summarize must ride inside
    // sample 2's window (a serial summarize would add a fourth 300ms step).
    let elapsed = t0.elapsed();
    assert!(
        elapsed <= Duration::from_millis(1050),
        "summarize did not overlap the turn: {elapsed:?}"
    );
    assert_eq!(summarize.request_count(), 1, "exactly one summarize call");
    let (digest, degraded) = compaction_digest(&s.log_path).expect("compaction entry");
    assert!(digest.contains("SPEC DIGEST"), "digest was: {digest}");
    assert!(!degraded);
}

#[tokio::test]
async fn failed_speculation_falls_back_to_inline_summarize() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![
        // Both speculative attempts fail…
        vec![Err(ProviderError::Transport("summarizer flaky".into()))],
        vec![Err(ProviderError::Transport(
            "summarizer still flaky".into(),
        ))],
        // …the fold-time inline path succeeds: no degraded floor.
        ScriptedProvider::text_reply("LATE DIGEST from the inline path"),
    ]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
        delay_ms: 0,
    });
    let mut s = session(provider, cfg());
    push_main_scripts(&main, s.dir.path());

    s.handle.prompt("start the long task".into()).await;
    let outcome = wait_done(&mut s).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done after compaction".into()
        }
    );
    assert_eq!(
        summarize.request_count(),
        3,
        "2 speculative attempts + 1 inline"
    );
    let (digest, degraded) = compaction_digest(&s.log_path).expect("compaction entry");
    assert!(digest.contains("LATE DIGEST"), "digest was: {digest}");
    assert!(!degraded, "a failed speculation must not floor the digest");
}

#[tokio::test]
async fn reset_mode_compaction_stays_inline() {
    // Reset-mode folds everything after the prefix; a speculative digest
    // covering only part of that span must not be used.
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "RESET DIGEST",
    )]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
        delay_ms: 0,
    });
    let mut s = session(
        provider,
        EngineConfig {
            compaction_reset: true,
            ..cfg()
        },
    );
    push_main_scripts(&main, s.dir.path());

    s.handle.prompt("start the long task".into()).await;
    let outcome = wait_done(&mut s).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done after compaction".into()
        }
    );
    assert_eq!(summarize.request_count(), 1);
    let (digest, _) = compaction_digest(&s.log_path).expect("compaction entry");
    assert!(digest.contains("RESET DIGEST"), "digest was: {digest}");
}
