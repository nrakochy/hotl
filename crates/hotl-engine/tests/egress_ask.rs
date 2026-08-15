//! Plan 0026's shown-hosts rule, end-to-end through the real gate.
//!
//! The rule is the whole reason the egress ask is bearable: a human who
//! approves `curl https://docs.example.com` has read the host, so prompting
//! again would be noise, and noise is what turns a gate into a rubber stamp.
//! It is also the rule most able to fail open — if a `Verdict::Auto` counted
//! as "a human saw it", every allow-rule would silently become a standing
//! egress grant. So these tests drive the actual `Turn` gate rather than
//! calling the extractor directly.
//!
//! The probe tool reports, from inside its own `run`, whether the registry
//! considers a host shown — which is exactly the question the egress sink
//! asks while the call is in flight.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::{
    spawn_session, AskReply, EngineConfig, EngineEvent, Outcome, SessionDeps, SessionHandle,
};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Permission, Registry, Tool, ToolOutcome};
use serde_json::{json, Value};

/// A tool whose summary carries a URL (like `bash`'s `curl …` or
/// `web_fetch`'s) and which, while running, records whether the shown-hosts
/// registry has the host in it.
struct ProbeTool {
    /// Set during `run` to `host_was_shown(&self.host)`.
    saw: Arc<AtomicBool>,
    /// The host this probe asks about. Per-test, never shared: `SHOWN_HOSTS`
    /// is a process-wide registry keyed by host (`hotl-tools/src/net.rs`) and
    /// these tests run in parallel in one process, so a single shared hostname
    /// let the one test that legitimately registers it flip every *other*
    /// test's `saw` for the width of its call — a real, load-dependent flake.
    host: String,
}

impl Tool for ProbeTool {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn description(&self) -> &str {
        "test probe"
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"cmd": {"type": "string"}}})
    }
    fn permission(&self, input: &Value) -> Permission {
        Permission::Ask {
            summary: input
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }
    }
    fn run<'a>(
        &'a self,
        _input: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        self.saw.store(
            hotl_tools::net::host_was_shown(&self.host),
            Ordering::SeqCst,
        );
        Box::pin(std::future::ready(ToolOutcome::ok("ran")))
    }
}

struct Session {
    handle: SessionHandle,
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

/// True when `hotl-tools` was compiled `security-enforced`, the build where
/// `Bypass` cannot exist at runtime (`rules::enforced_mode` coerces it to
/// `Ask`). A test whose premise is "the call auto-allows with no human" is
/// then **void**, not violated.
///
/// Deliberately a runtime check, not `#[cfg(not(feature = ...))]` as
/// `hotl-tools`' own suite uses: this crate has no `security-enforced` feature
/// of its own, so a `cfg` here is always false — while `cargo test --workspace
/// --all-features` turns hotl-tools' on through feature unification, which is
/// exactly the build that used to fail.
fn bypass_unavailable() -> bool {
    hotl_tools::rules::enforced_build()
}

/// `host` is the one this test asks the registry about, and it must be unique
/// to the test — see [`ProbeTool::host`].
fn session(cmd: &str, host: &str, rules: Rules) -> (Session, Arc<AtomicBool>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = EngineConfig::default();
    let log = SessionLog::create(dir.path(), &config.model, None, Masker::empty(), 0).expect("log");
    let saw = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::builtin();
    registry.register(Box::new(ProbeTool {
        saw: saw.clone(),
        host: host.to_string(),
    }));
    let provider = Arc::new(ScriptedProvider::new(vec![
        ScriptedProvider::tool_call("t1", "probe", json!({"cmd": cmd})),
        ScriptedProvider::text_reply("done"),
    ]));
    let handle = spawn_session(SessionDeps {
        provider,
        registry: Arc::new(registry),
        rules: Arc::new(rules),
        sandbox_enforced: false,
        clock: Arc::new(SystemClock),
        log,
        system: "sys".into(),
        cwd: dir.path().to_path_buf(),
        snapshots: None,
        hooks: None,
        initial_items: Vec::new(),
        initial_todos: Vec::new(),
        initial_goal: None,
        config,
    });
    (Session { handle, dir }, saw)
}

/// Drive one turn, answering any `Ask` with `reply`. Returns the outcome.
async fn run_turn(session: &mut Session, reply_with: impl Fn() -> AskReply) -> Outcome {
    session.handle.prompt("go".into()).await;
    let drive = async {
        while let Some(event) = session.handle.events.recv().await {
            match event {
                EngineEvent::Ask { reply, .. } => {
                    let _ = reply.send(reply_with());
                }
                EngineEvent::TurnDone { outcome, .. } => return outcome,
                _ => {}
            }
        }
        Outcome::Error {
            message: "session ended".into(),
        }
    };
    tokio::time::timeout(Duration::from_secs(20), drive)
        .await
        .expect("turn should not hang")
}

#[tokio::test]
async fn human_approved_host_is_shown_while_the_call_runs() {
    let (mut s, saw) = session(
        "curl https://approved.example.com/x",
        "approved.example.com",
        Rules::default(),
    );
    let outcome = run_turn(&mut s, || AskReply::Allow).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(
        saw.load(Ordering::SeqCst),
        "a host the human read in the approval summary must not prompt again"
    );
    assert!(
        !hotl_tools::net::host_was_shown("approved.example.com"),
        "the registration is scoped to the call, never a durable grant"
    );
}

/// **The single most important test in plan 0026.** Without it the
/// shown-hosts rule silently becomes "allow-rules grant egress": a
/// `Verdict::Auto` returns the same `AskReply::Allow` a human does, and any
/// code that infers "was there a human?" downstream gets it wrong.
#[tokio::test]
async fn auto_allowed_call_does_not_suppress_the_ask() {
    let rules =
        Rules::from_toml("[[allow]]\ntool = \"probe\"\nfield = \"cmd\"\nprefix = \"curl \"\n")
            .expect("rules");
    let (mut s, saw) = session("curl https://auto.example.com/x", "auto.example.com", rules);
    let outcome = run_turn(&mut s, || {
        panic!("an auto-allowed call must not reach the human")
    })
    .await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(
        !saw.load(Ordering::SeqCst),
        "an allow-rule is not a human; a rule-approved call reaching an unlisted \
         host is exactly the case the egress ask exists for"
    );
}

/// Bypass mode is the other way a call auto-allows with no human in the loop,
/// and it reaches `Verdict::Auto` without any rule at all.
#[tokio::test]
async fn bypass_mode_does_not_suppress_the_ask() {
    if bypass_unavailable() {
        return;
    }
    let rules = Rules::default().with_mode(hotl_tools::rules::PermissionMode::Bypass);
    let (mut s, saw) = session(
        "curl https://bypass.example.com/x",
        "bypass.example.com",
        rules,
    );
    let outcome = run_turn(&mut s, || panic!("bypass mode must not reach the human")).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(!saw.load(Ordering::SeqCst));
}

/// `https://good.com@evil.com/` parses to host `evil.com` while a reader's eye
/// lands on `good.com`. The approval is therefore not evidence about either.
#[tokio::test]
async fn a_userinfo_url_shows_nothing() {
    let (mut s, saw) = session(
        "curl https://userinfo.example.com@evil.example/",
        // The host the *eye* reads, which is the one that must NOT register.
        "userinfo.example.com",
        Rules::default(),
    );
    let outcome = run_turn(&mut s, || AskReply::Allow).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(
        !saw.load(Ordering::SeqCst),
        "a userinfo URL must still prompt — the host the eye reads is not the host reached"
    );
}

/// The human edited the input after reading the summary, so the summary is
/// stale and no longer evidence of what is about to run.
#[tokio::test]
async fn allow_edited_does_not_suppress() {
    let (mut s, saw) = session(
        "curl https://edited.example.com/x",
        "edited.example.com",
        Rules::default(),
    );
    let outcome = run_turn(&mut s, || AskReply::AllowEdited {
        input: json!({"cmd": "curl https://edited.example.com/y"}),
    })
    .await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(!saw.load(Ordering::SeqCst));
}

/// Prose that merely mentions a hostname must not pre-authorize it — only
/// URL-shaped tokens count, and they are parsed, never substring-matched.
#[tokio::test]
async fn a_bare_hostname_in_prose_shows_nothing() {
    let (mut s, saw) = session(
        "echo talking about prose.example.com",
        "prose.example.com",
        Rules::default(),
    );
    let outcome = run_turn(&mut s, || AskReply::Allow).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(!saw.load(Ordering::SeqCst));
}

/// A denial registers nothing: there is no approval to reuse.
#[tokio::test]
async fn a_denied_call_shows_nothing() {
    let (mut s, saw) = session(
        "curl https://denied.example.com/x",
        "denied.example.com",
        Rules::default(),
    );
    let outcome = run_turn(&mut s, || AskReply::Deny { message: None }).await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert!(!saw.load(Ordering::SeqCst));
}
