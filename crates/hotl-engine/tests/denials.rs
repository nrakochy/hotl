//! T3-1: a denial is a decision, not a tool malfunction — it must not draw
//! down the per-tool failure budget, and it must not carry the last-chance
//! `<system-hint>` warning that contradicts its own message.

use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::{Provider, ScriptedProvider};
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::json;

struct Session {
    handle: SessionHandle,
    /// Kept alive for the session's lifetime — the log lives in it.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

fn session(provider: Arc<dyn Provider>, config: EngineConfig) -> Session {
    session_with_rules(provider, config, Rules::default())
}

fn session_with_rules(provider: Arc<dyn Provider>, config: EngineConfig, rules: Rules) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let handle = spawn_session(SessionDeps {
        concurrency: Default::default(),
        provider,
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(rules),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "test-system".into(),
        cwd: dir.path().to_path_buf(),
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

/// Drive to the terminal outcome, answering every ask with `answer`.
async fn run_answering(s: &mut Session, answer: impl Fn() -> AskReply) -> Outcome {
    loop {
        match next_event(s).await {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(answer());
            }
            EngineEvent::TurnDone { outcome, .. } => return outcome,
            _ => {}
        }
    }
}

/// T3-1: six consecutive user denials must not end the turn with
/// `ToolFailureBudget` (default budget 5). A denial is a decision, not a tool
/// malfunction, and it is not retryable.
#[tokio::test]
async fn user_denials_are_not_charged_to_the_tool_failure_budget() {
    let scripts: Vec<_> = (0..6)
        .map(|i| {
            ScriptedProvider::tool_call(
                &format!("t{i}"),
                "bash",
                json!({"command": format!("echo {i}")}),
            )
        })
        .chain(std::iter::once(ScriptedProvider::text_reply(
            "gave up on bash",
        )))
        .collect();
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let mut s = session(
        provider,
        EngineConfig {
            max_turns: 20,
            ..Default::default()
        },
    );

    s.handle.prompt("try bash a lot".into()).await;
    let outcome = run_answering(&mut s, || AskReply::Deny { message: None }).await;
    assert_eq!(
        outcome,
        Outcome::Done {
            text: "gave up on bash".into()
        }
    );
}

/// T3-9: `DontAsk` is an unattended posture. The doom guard is a malfunction
/// brake, not a permission — it must stop the turn, never emit an `Ask` that
/// nobody is there to answer.
#[tokio::test]
async fn dont_ask_mode_hard_stops_on_a_doom_loop() {
    // The same call three times over: `CallSig` ignores the tool_use id, so
    // these are one repeating signature and the detector's period-1 rule fires.
    let repeat = |i: usize| {
        ScriptedProvider::tool_call(&format!("t{i}"), "bash", json!({"command": "echo same"}))
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        repeat(0),
        repeat(1),
        repeat(2),
        ScriptedProvider::text_reply("must never be reached"),
    ]));
    let mut s = session(
        provider,
        EngineConfig {
            max_turns: 20,
            ..Default::default()
        },
    );
    s.handle
        .set_mode(hotl_tools::rules::PermissionMode::DontAsk)
        .await;

    s.handle.prompt("loop forever".into()).await;
    let mut saw_ask = false;
    let outcome = loop {
        match next_event(&mut s).await {
            // Deliberately unanswered: nobody is watching in an unattended
            // posture, which is the whole point.
            EngineEvent::Ask { .. } => saw_ask = true,
            EngineEvent::TurnDone { outcome, .. } => break outcome,
            _ => {}
        }
    };
    assert!(
        matches!(outcome, Outcome::DoomLoop { .. }),
        "expected a hard stop, got {outcome:?}"
    );
    assert!(
        !saw_ask,
        "an unattended mode must never emit an Ask nobody can answer"
    );
}

/// Vuln 8: a tool's summary is model-authored text rendered into the human's
/// y/N prompt. The engine must flatten it to a single control-free line at the
/// ask chokepoint, so a `bash` command carrying `\r\x1b[2K` (a line-erase) or a
/// bidi override cannot spoof what the human is about to approve.
#[tokio::test]
async fn ask_summaries_are_sanitized_before_they_reach_the_human() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call(
            "t1",
            "bash",
            json!({"command": "echo hi\n\u{1b}[2Krm -rf / \u{202e}"}),
        ),
        ScriptedProvider::text_reply("done"),
    ]));
    let mut s = session(provider, EngineConfig::default());
    s.handle.prompt("run bash".into()).await;

    let summary = loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { summary, reply, .. } => {
                let _ = reply.send(AskReply::Deny { message: None });
                break summary;
            }
            EngineEvent::TurnDone { .. } => panic!("expected an ask, got turn done"),
            _ => {}
        }
    };
    assert!(!summary.contains('\n'), "newline survived: {summary:?}");
    assert!(!summary.contains('\u{1b}'), "ESC survived: {summary:?}");
    assert!(
        !summary.contains('\u{202e}'),
        "bidi override survived: {summary:?}"
    );
}

/// Vuln 6: `read` is `Permission::None` in-workspace and short-circuited the
/// gate before any rule ran, so a `[[deny]]` on it was silently dead. The gate
/// must now consult the deny tiers for a `Permission::None` tool too.
#[tokio::test]
async fn a_deny_rule_bites_a_permission_none_tool() {
    let rules = Rules::from_toml("[[deny]]\ntool = \"read\"\npath_prefix = \".env\"\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "read", json!({"path": ".env"})),
        ScriptedProvider::text_reply("ok"),
    ]));
    let mut s = session_with_rules(provider, EngineConfig::default(), rules);
    s.handle.prompt("read the env".into()).await;

    let mut denied = false;
    loop {
        match next_event(&mut s).await {
            EngineEvent::ToolDenied { .. } => denied = true,
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(AskReply::Allow);
            }
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(
        denied,
        "a [[deny]] on read must refuse the call, not run it"
    );
}
