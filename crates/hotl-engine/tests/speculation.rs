//! Speculative compaction: once the context estimate crosses the speculation
//! threshold, the digest summarize runs concurrently with the turn's samples,
//! so hitting the compaction trigger folds without a blocking model call.
//! Overlap is observed *directly* — the router records how many provider
//! streams are alive at once, so a peak above one is concurrency itself rather
//! than an inference from elapsed time. This deliberately does not use tokio's
//! paused clock: the actor now awaits the log writer thread's fsync ack, and a
//! paused clock auto-advances past any wake that comes from outside the
//! runtime (see 0011-remediation-durability's decision log).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// How many provider streams were alive at the same moment. A peak of 2 means
/// the summarize really did ride alongside a sample; a serial summarize can
/// never exceed 1. No clock involved, so nothing here can flake on timing.
#[derive(Default)]
struct Concurrency {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl Concurrency {
    fn enter(self: &Arc<Self>) -> InFlight {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        InFlight(Arc::clone(self))
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

/// Alive for exactly as long as the stream it is attached to.
struct InFlight(Arc<Concurrency>);

impl InFlight {
    /// Identity. Exists only so a `map` closure can own the guard and thereby
    /// tie its lifetime to the stream's.
    fn pass<T>(&self, event: T) -> T {
        event
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Routes summarize requests (identified by the compaction system prompt) to
/// their own scripts, delaying every stream by `delay_ms` and recording how
/// many streams overlap.
struct Router {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    delay_ms: u64,
    concurrency: Arc<Concurrency>,
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
        let in_flight = self.concurrency.enter();
        Box::pin(
            futures_util::stream::once(async move {
                tokio::time::sleep(delay).await;
                inner.stream(req)
            })
            .flatten()
            .map(move |event| in_flight.pass(event)),
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
        initial_goal: None,
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

#[tokio::test]
async fn speculative_digest_overlaps_the_turn() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("GOAL: SPEC DIGEST of the early work"),
        ScriptedProvider::text_reply("LATE DIGEST"),
    ]));
    let concurrency = Arc::<Concurrency>::default();
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
        // Wide enough that the speculative summarize and the sample it rides
        // alongside are unambiguously in flight together.
        delay_ms: 300,
        concurrency: Arc::clone(&concurrency),
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

    // The summarize must ride alongside a sample rather than following it: a
    // serial summarize never puts two streams in flight at once.
    assert!(
        concurrency.peak() >= 2,
        "summarize did not overlap the turn: peak in-flight streams was {}",
        concurrency.peak()
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
        concurrency: Arc::default(),
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

/// Hangs the FIRST summarize request (the speculative one) forever and answers
/// every later one — the fold must give up on the speculation and take the
/// inline path rather than wait on it.
struct HangFirstSummarize {
    main: Arc<ScriptedProvider>,
    summarize: Arc<ScriptedProvider>,
    seen: AtomicUsize,
}

impl Provider for HangFirstSummarize {
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        if !req.system.contains("compress") {
            return self.main.stream(req);
        }
        if self.seen.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(futures_util::stream::pending());
        }
        self.summarize.stream(req)
    }
}

/// T1-4, the no-human half: nobody presses Ctrl-C, so the wall-clock bound
/// alone has to save the turn. Deliberately on the real clock — a paused clock
/// can no longer drive the actor's write path (0011's standing constraint), so
/// this pays `SPECULATION_WAIT` in real time. The precise race (bound fires,
/// cancel wins, handle is aborted) is unit-tested in `turn.rs` instead.
#[tokio::test]
async fn hung_speculation_falls_back_to_the_inline_summarize() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
        "INLINE DIGEST after the speculation was abandoned",
    )]));
    let provider = Arc::new(HangFirstSummarize {
        main: Arc::clone(&main),
        summarize: Arc::clone(&summarize),
        seen: AtomicUsize::new(0),
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
    let (digest, degraded) = compaction_digest(&s.log_path).expect("compaction entry");
    assert!(digest.contains("INLINE DIGEST"), "digest was: {digest}");
    assert!(!degraded, "the inline path succeeded; no floor digest");
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
        concurrency: Arc::default(),
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
