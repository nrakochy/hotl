//! Plan mode through the real engine. Plan is an *overlay*, not a mode: it
//! puts `write`/`edit` on the protected floor (always ask, never auto) and
//! leaves every other tool to `mode`. These tests pin both halves — the ask a
//! file edit must raise even under `Bypass`, and the shell/network call that
//! must still run without one.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::{spawn_session, AskReply, EngineConfig, EngineEvent, SessionDeps, SessionHandle};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::rules::{PermissionMode, Rules};
use hotl_tools::{Permission, Registry, Tool, ToolOutcome};
use hotl_types::{EntryPayload, Item};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Session {
    handle: SessionHandle,
    dir: tempfile::TempDir,
}

async fn next_event(s: &mut Session) -> EngineEvent {
    tokio::time::timeout(Duration::from_secs(30), s.handle.events.recv())
        .await
        .expect("event timeout")
        .expect("event channel closed")
}

/// One scripted tool call under a given (mode, plan) pair, with an extra tool
/// optionally registered. Returns the events of interest plus the log text.
struct Ran {
    asked: bool,
    auto_allowed: bool,
    denied: bool,
    log: String,
}

/// True when `hotl-tools` was compiled `security-enforced`, the build where
/// `Bypass` cannot exist at runtime (`rules::enforced_mode` coerces it to
/// `Ask`). Every bypass-premised test below is then **void**, not violated.
///
/// A runtime check rather than `#[cfg(not(feature = ...))]`: this crate has no
/// `security-enforced` feature of its own, so a `cfg` here is always false —
/// while `cargo test --workspace --all-features` turns hotl-tools' on through
/// feature unification.
fn bypass_unavailable() -> bool {
    hotl_tools::rules::enforced_build()
}

async fn run_one(
    mode: PermissionMode,
    plan: bool,
    tool: &str,
    input: Value,
    extra: Option<Box<dyn Tool>>,
    answer: AskReply,
) -> Ran {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let mut registry = Registry::builtin();
    if let Some(t) = extra {
        registry.register(t);
    }
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", tool, input),
        ScriptedProvider::text_reply("done"),
    ]));
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(registry),
        rules: Arc::new(Rules::default().with_mode(mode).with_plan(plan)),
        // The bash allow-rule carve-out needs the floor; `Bypass` gates bash on
        // it too, so a false here would make "bypass runs bash" untestable.
        sandbox_enforced: true,
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
    let mut s = Session { handle, dir };
    s.handle.prompt("go".into()).await;

    let (mut asked, mut auto_allowed, mut denied) = (false, false, false);
    loop {
        match next_event(&mut s).await {
            EngineEvent::Ask { reply, .. } => {
                asked = true;
                let _ = reply.send(answer.clone());
            }
            EngineEvent::ToolAutoAllowed { .. } => auto_allowed = true,
            EngineEvent::ToolDenied { .. } => denied = true,
            EngineEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    drop(s.handle);

    let log_path = std::fs::read_dir(s.dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    let log = std::fs::read_to_string(&log_path).expect("read log");
    Ran {
        asked,
        auto_allowed,
        denied,
        log,
    }
}

/// The `t1` tool result's (is_error, content) as the log recorded it.
fn result_of(log: &str) -> (bool, String) {
    for line in log.lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("entry");
        if let EntryPayload::Item {
            item: Item::ToolResults { results },
        } = entry.payload
        {
            for r in results {
                if r.tool_use_id == "t1" {
                    return (r.is_error, r.content);
                }
            }
        }
    }
    panic!("no tool result found for t1");
}

/// `write` resolves against the process-global fsguard workspace root, not
/// `deps.cwd`, so a scripted write lands in the process cwd. Same shape as
/// `set_mode.rs`: a unique name, asserted on, then removed.
fn write_call(name: &str) -> (Value, std::path::PathBuf) {
    let target = std::env::current_dir().expect("cwd").join(name);
    let _ = std::fs::remove_file(&target);
    (json!({"path": name, "content": "nope"}), target)
}

/// Plan's one permission effect. Under `Bypass` the same write would run
/// silently; the overlay is what forces the human beat.
#[tokio::test]
async fn plan_asks_before_a_write_even_under_bypass() {
    let (input, target) = write_call("plan-bypass-declined.txt");
    let r = run_one(
        PermissionMode::Bypass,
        true,
        "write",
        input,
        None,
        AskReply::Deny { message: None },
    )
    .await;
    let landed = target.exists();
    let _ = std::fs::remove_file(&target);
    assert!(r.asked, "plan must raise an ask for a write under bypass");
    assert!(
        !r.auto_allowed,
        "plan's floor sits above the bypass tier — never auto"
    );
    assert!(!landed, "a declined write must not touch the filesystem");
}

/// The control for the test above: same call, plan off, and it runs unattended.
#[tokio::test]
async fn without_plan_bypass_writes_without_asking() {
    if bypass_unavailable() {
        return;
    }
    let (input, target) = write_call("plan-off-bypass-wrote.txt");
    let r = run_one(
        PermissionMode::Bypass,
        false,
        "write",
        input,
        None,
        AskReply::Allow,
    )
    .await;
    let landed = target.exists();
    let _ = std::fs::remove_file(&target);
    assert!(!r.asked, "bypass alone must not ask for a write");
    assert!(r.auto_allowed);
    assert!(landed, "the write must actually have run");
}

/// plan+dontask raises the ask rather than auto-allowing. The engine has no
/// opinion about who answers it — headless surfaces default-deny (see
/// `agent.rs`'s no-human path), which is what makes this posture refuse a
/// write in practice, landing where the old hard-blocking plan mode did.
#[tokio::test]
async fn plan_plus_dontask_asks_and_a_declined_write_never_lands() {
    let (input, target) = write_call("plan-dontask-declined.txt");
    let r = run_one(
        PermissionMode::DontAsk,
        true,
        "write",
        input,
        None,
        AskReply::Deny { message: None },
    )
    .await;
    let landed = target.exists();
    let _ = std::fs::remove_file(&target);
    assert!(r.asked, "plan's floor must ask, not silently deny");
    assert!(!r.auto_allowed);
    assert!(r.denied, "the declined ask must surface as a denial");
    let (is_error, _) = result_of(&r.log);
    assert!(is_error);
    assert!(!landed);
}

/// The motivating case for the whole change: under plan+bypass the agent can
/// still shell out — reaching JIRA, fetching a page, running `git log` — while
/// it works out what to propose.
#[tokio::test]
async fn plan_plus_bypass_still_runs_bash() {
    if bypass_unavailable() {
        return;
    }
    let r = run_one(
        PermissionMode::Bypass,
        true,
        "bash",
        json!({"command": "echo reached-the-network"}),
        None,
        AskReply::Deny { message: None },
    )
    .await;
    assert!(!r.asked, "plan must not gate bash — that is the mode's job");
    assert!(r.auto_allowed);
    let (is_error, content) = result_of(&r.log);
    assert!(!is_error, "bash must have run: {content}");
    assert!(content.contains("reached-the-network"), "{content}");
}

/// The same call under plan+ask does prompt — plan takes its cue from the
/// mode rather than overriding it.
#[tokio::test]
async fn plan_plus_ask_prompts_for_bash() {
    let r = run_one(
        PermissionMode::Ask,
        true,
        "bash",
        json!({"command": "echo hi"}),
        None,
        AskReply::Allow,
    )
    .await;
    assert!(r.asked, "plan+ask must prompt before a shell command");
    let (is_error, content) = result_of(&r.log);
    assert!(!is_error, "{content}");
}

/// A tool whose permission still asks (not `Permission::None`) but which is
/// structurally read-only. Plan must not touch it, and neither must `dontask`.
///
/// This used to be `RecallTool` wrapped around a static backend. It is a
/// hand-rolled tool now because `recall` left this class: its shipped backend
/// spawns an arbitrary configured program, so `RecallTool::read_only()` is
/// `false` (T2-8). The gate's behaviour is unchanged — only the vehicle is.
struct AskingReadOnlyTool;

impl Tool for AskingReadOnlyTool {
    fn name(&self) -> &'static str {
        "peek"
    }
    fn description(&self) -> &str {
        "look something up without changing it"
    }
    fn read_only(&self) -> bool {
        true
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        })
    }
    fn permission(&self, input: &Value) -> Permission {
        let query = input.get("query").and_then(Value::as_str).unwrap_or("?");
        Permission::Ask {
            summary: format!("peek: \"{query}\""),
        }
    }
    fn run<'a>(&'a self, _input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async { ToolOutcome::ok("Prefer thiserror for library errors.") })
    }
}

#[tokio::test]
async fn plan_does_not_block_a_read_only_tool_that_still_asks() {
    let r = run_one(
        PermissionMode::Ask,
        true,
        "peek",
        json!({"query": "error handling style"}),
        Some(Box::new(AskingReadOnlyTool)),
        AskReply::Allow,
    )
    .await;
    let (is_error, content) = result_of(&r.log);
    assert!(!r.denied, "a read-only tool must never be plan-blocked");
    assert!(!is_error, "peek must succeed: {content}");
    assert!(content.contains("thiserror"), "{content}");
}

/// A two-prompt session with no tool calls, so every request is a plain
/// sample: the roster and the reminder items are all these tests read.
struct Visible {
    requests: Vec<hotl_provider::SamplingRequest>,
    log: String,
}

/// `toggles[n]` is applied (if any) before prompt `n`.
async fn run_visible(start_in_plan: bool, toggles: [Option<bool>; 2]) -> Visible {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0)
        .expect("session log");
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::text_reply("one"),
        ScriptedProvider::text_reply("two"),
    ]));
    let handle = spawn_session(SessionDeps {
        provider: provider.clone(),
        registry: Arc::new(Registry::builtin()),
        rules: Arc::new(
            Rules::default()
                .with_mode(PermissionMode::Ask)
                .with_plan(start_in_plan),
        ),
        sandbox_enforced: true,
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
    let mut s = Session { handle, dir };

    for (round, toggle) in toggles.into_iter().enumerate() {
        if let Some(plan) = toggle {
            s.handle.set_plan(plan).await;
        }
        s.handle.prompt(format!("go {round}")).await;
        loop {
            if let EngineEvent::TurnDone { .. } = next_event(&mut s).await {
                break;
            }
        }
    }
    drop(s.handle);

    let log_path = std::fs::read_dir(s.dir.path())
        .expect("session dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("session log");
    Visible {
        requests: provider.requests(),
        log: std::fs::read_to_string(&log_path).expect("read log"),
    }
}

fn tool_names(req: &hotl_provider::SamplingRequest) -> Vec<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

/// Every synthetic user item in the log, in commit order.
fn reminders(log: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in log.lines() {
        let entry: hotl_types::Entry = serde_json::from_str(line).expect("entry");
        if let EntryPayload::Item {
            item:
                Item::User {
                    text,
                    synthetic: Some(_),
                    ..
                },
        } = entry.payload
        {
            out.push(text);
        }
    }
    out
}

/// A2, first half: the overlay used to be invisible in the roster too — the
/// model was offered `write`/`edit` and then refused at the gate.
#[tokio::test]
async fn plan_hides_write_and_edit_from_the_roster() {
    let v = run_visible(true, [None, None]).await;
    assert_eq!(v.requests.len(), 2, "one request per prompt");
    for req in &v.requests {
        let names = tool_names(req);
        assert!(!names.contains(&"write".to_string()), "{names:?}");
        assert!(!names.contains(&"edit".to_string()), "{names:?}");
        assert!(names.contains(&"read".to_string()), "{names:?}");
        assert!(names.contains(&"bash".to_string()), "{names:?}");
    }
}

/// A2, second half: the toggle tells the model, both ways, and the roster it
/// advertises next follows the same flag.
#[tokio::test]
async fn plan_toggle_tells_the_model_and_moves_the_roster() {
    let v = run_visible(false, [Some(true), Some(false)]).await;
    assert_eq!(v.requests.len(), 2, "one request per prompt");

    let on = tool_names(&v.requests[0]);
    assert!(!on.contains(&"write".to_string()), "{on:?}");
    assert!(!on.contains(&"edit".to_string()), "{on:?}");
    let off = tool_names(&v.requests[1]);
    assert!(off.contains(&"write".to_string()), "{off:?}");
    assert!(off.contains(&"edit".to_string()), "{off:?}");

    let said = reminders(&v.log);
    assert!(
        said.iter().any(|t| t.contains("Plan mode is on")),
        "no plan-on reminder in {said:?}"
    );
    assert!(
        said.iter().any(|t| t.contains("Plan mode is off")),
        "no plan-off reminder in {said:?}"
    );
}

/// The reminder rides the projection the model actually sees, not just the
/// log: a `SetPlan` that appended nowhere the request reads would pass the
/// test above and teach the model nothing.
#[tokio::test]
async fn the_plan_reminder_reaches_the_next_request() {
    let v = run_visible(false, [Some(true), None]).await;
    let req = &v.requests[0];
    let rendered: String = req
        .items
        .iter()
        .chain(req.ephemeral_tail.iter())
        .filter_map(|i| match i.as_ref() {
            Item::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("Plan mode is on"),
        "the first request never carried the reminder: {rendered}"
    );
}
