//! 0030 Task 4: `MaxTokens` and `PauseTurn` must not fall into the done arm.
//! A truncated response silently accepted as a complete answer is the exact
//! opposite of what the stop reason means; the recovery is bounded
//! (`MAX_TOKENS_CONTINUE_MAX`) and the counter crosses compaction folds like
//! every other per-turn safety counter (T2-2).

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
use hotl_types::{Entry, EntryPayload, Item, StopReason, SyntheticReason};
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
    log_path: std::path::PathBuf,
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
        dir,
        log_path,
    }
}

/// Drive to the terminal outcome, allowing any asks along the way.
async fn run_to_done(s: &mut Session) -> Outcome {
    s.handle.prompt("go".into()).await;
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
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

/// `SystemReminder` items whose text contains `needle`, counted in the RAW
/// log (append-only, so a compaction fold can never hide one). An empty
/// needle counts every reminder.
fn reminders_containing(path: &std::path::Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .expect("read log")
        .lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .filter(|e| {
            matches!(
                &e.payload,
                EntryPayload::Item {
                    item: Item::User {
                        text,
                        synthetic: Some(SyntheticReason::SystemReminder),
                        ..
                    },
                } if text.contains(needle)
            )
        })
        .count()
}

fn cut_off_reminders(path: &std::path::Path) -> usize {
    reminders_containing(path, "cut off")
}

/// [`ScriptedProvider::text_reply_with_stop`] with a chosen reported
/// input_tokens — the anchor (A12b) the engine's context estimate builds on.
fn text_with_stop_reporting(
    text: &str,
    stop: StopReason,
    input_tokens: u64,
) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut script = ScriptedProvider::text_reply_with_stop(text, stop);
    if let Some(Ok(StreamEvent::Completed { usage, .. })) = script.last_mut() {
        usage.input_tokens = input_tokens;
    }
    script
}

#[tokio::test]
async fn max_tokens_truncation_injects_a_reminder_and_continues() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply_with_stop("partial", StopReason::MaxTokens),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider.clone(), EngineConfig::default());

    let outcome = run_to_done(&mut s).await;

    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done".into()
        }
    );
    assert_eq!(cut_off_reminders(&s.log_path), 1);
}

#[tokio::test]
async fn max_tokens_with_complete_tool_calls_runs_the_tools() {
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut s = session(provider.clone(), EngineConfig::default());
    let file = s.dir.path().join("f.txt");
    std::fs::write(&file, "file body").expect("fixture");
    let path = file.to_str().expect("utf8 path");
    let mut tool_script = ScriptedProvider::tool_call("t1", "read", json!({ "path": path }));
    if let Some(Ok(StreamEvent::Completed { stop, .. })) = tool_script.last_mut() {
        // Complete tool call, but the stream hit the output cap after it.
        *stop = StopReason::MaxTokens;
    }
    provider.push_script(tool_script);
    provider.push_script(ScriptedProvider::text_reply("done"));

    let outcome = run_to_done(&mut s).await;

    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done".into()
        }
    );
    // The results are the natural continuation: the tool ran, no reminder.
    let replayed = hotl_store::replay(&s.log_path).expect("replay");
    let tool_ran = replayed.items.iter().any(|i| {
        matches!(i, Item::ToolResults { results }
            if results.iter().any(|r| !r.is_error && r.content.contains("file body")))
    });
    assert!(
        tool_ran,
        "the truncated sample's complete tool call must run"
    );
    assert_eq!(cut_off_reminders(&s.log_path), 0);
}

#[tokio::test]
async fn max_tokens_continuations_are_bounded() {
    let provider = Arc::new(ScriptedProvider::new(
        (1..=4)
            .map(|i| {
                ScriptedProvider::text_reply_with_stop(
                    &format!("partial {i}"),
                    StopReason::MaxTokens,
                )
            })
            .collect(),
    ));
    let mut s = session(provider.clone(), EngineConfig::default());

    let outcome = run_to_done(&mut s).await;

    // Three reminders, then the fourth truncated sample is accepted as done
    // rather than looping forever on an output the cap can't fit.
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "partial 4".into()
        }
    );
    assert_eq!(cut_off_reminders(&s.log_path), 3);
    assert_eq!(provider.request_count(), 4);
}

#[tokio::test]
async fn max_tokens_continues_survive_a_compaction_fold() {
    let main = Arc::new(ScriptedProvider::new(Vec::new()));
    let summarize = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("DIGEST"),
        ScriptedProvider::text_reply("DIGEST 2"),
    ]));
    let provider = Arc::new(Router {
        main: Arc::clone(&main),
        summarize,
    });
    // 650 arms speculation; 850 puts the NEXT request over the fold trigger —
    // same anchor arithmetic as `max_turns_is_enforced_across_a_compaction`.
    main.push_script(text_with_stop_reporting(
        "part 1",
        StopReason::MaxTokens,
        650,
    ));
    main.push_script(text_with_stop_reporting(
        "part 2",
        StopReason::MaxTokens,
        850,
    ));
    // Post-fold: a reset counter would grant three MORE reminders here.
    for i in 3..=8 {
        main.push_script(text_with_stop_reporting(
            &format!("part {i}"),
            StopReason::MaxTokens,
            100,
        ));
    }
    let mut s = session(
        provider,
        EngineConfig {
            context_window: 1000,
            ..Default::default()
        },
    );

    let outcome = run_to_done(&mut s).await;

    // continues spent: 2 pre-fold, 1 post-fold; the 4th truncated sample ends
    // the turn. A counter that reset at the fold would emit up to 5 reminders.
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "part 4".into()
        }
    );
    assert_eq!(
        cut_off_reminders(&s.log_path),
        3,
        "the recovery budget must cross the fold"
    );
}

#[tokio::test]
async fn pause_turn_resamples_without_a_reminder() {
    // The committed projection already ends with the paused assistant turn —
    // exactly the resume shape the API expects, so nothing is injected.
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply_with_stop("pausing", StopReason::PauseTurn),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider.clone(), EngineConfig::default());

    let outcome = run_to_done(&mut s).await;

    assert_eq!(
        outcome,
        Outcome::Done {
            text: "done".into()
        }
    );
    assert_eq!(reminders_containing(&s.log_path, ""), 0);
    assert_eq!(provider.request_count(), 2);
}
