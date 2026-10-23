//! `wait`, `send`, `resume` (0072 D1): the three verbs for a background
//! sub-agent. Each reaches this session's registry through the task-local a
//! turn scopes around the call, so none of them holds session state and none
//! can reach another session's children.
//!
//! A result reaches exactly one reader (D5): `wait` claims it from the
//! registry and, when the inbox was about to carry it, withdraws that
//! notification by key — strictly after it, in channel order.

use futures_util::future::BoxFuture;
use hotl_engine::SessionCmd;
use hotl_tools::children::{Claim, ResumeRefusal, ResumeRequest, SendRefusal};
use hotl_tools::{Permission, Tool, ToolOutcome};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// How long `wait` blocks without a `timeout_ms`.
pub const DEFAULT_WAIT_MS: u64 = 120_000;

fn no_session(verb: &str) -> ToolOutcome {
    ToolOutcome::err(format!(
        "`{verb}` needs a live session with sub-agents, which isn't available in this context."
    ))
}

fn known_line(known: &[String]) -> String {
    match known.is_empty() {
        true => "no sub-agent is known to this session (children do not survive a resumed \
                 session)"
            .to_string(),
        false => format!("known sub-agents: {}", known.join(", ")),
    }
}

fn result_outcome(result: hotl_tools::children::ChildResult) -> ToolOutcome {
    let facts = hotl_tools::OutcomeFacts {
        delegation: Some(Box::new(result.facts)),
        ..Default::default()
    };
    match result.ok {
        true => ToolOutcome::ok(result.text).with_facts(facts),
        false => ToolOutcome::err(result.text).with_facts(facts),
    }
}

/// `wait {child?, timeout_ms?}`: the typed result of a background child.
pub struct WaitTool {
    notify: Option<tokio::sync::mpsc::WeakSender<SessionCmd>>,
}

impl WaitTool {
    pub fn new(notify: Option<tokio::sync::mpsc::WeakSender<SessionCmd>>) -> Self {
        Self { notify }
    }

    async fn unnotify(&self, key: &str) {
        if let Some(tx) = self.notify.as_ref().and_then(|n| n.upgrade()) {
            let _ = tx
                .send(SessionCmd::Unnotify {
                    key: key.to_string(),
                })
                .await;
        }
    }

    async fn run_impl(&self, input: Value, cancel: CancellationToken) -> ToolOutcome {
        let Some(children) = hotl_tools::children::registry() else {
            return no_session("wait");
        };
        let id = input.get("child").and_then(Value::as_str);
        let timeout = std::time::Duration::from_millis(
            input
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_WAIT_MS),
        );
        let rx =
            match children.claim(id) {
                // Finished already: the inbox may be about to carry it (D5).
                Claim::Ready(id, result) => {
                    self.unnotify(&id).await;
                    return result_outcome(result);
                }
                Claim::Parked(rx) => rx,
                Claim::Busy => return ToolOutcome::err(
                    "another `wait` is already parked on that sub-agent; one reader per result.",
                ),
                Claim::Closed => {
                    return ToolOutcome::err(format!(
                        "sub-agent {} is closed (stopped, evicted, or the session ended); its \
                     result is gone. `spawn` again if you need the work.",
                        id.unwrap_or("?")
                    ))
                }
                Claim::Unknown(known) => {
                    return ToolOutcome::err(format!(
                        "no sub-agent `{}`; {}.",
                        id.unwrap_or("?"),
                        known_line(&known)
                    ))
                }
                Claim::Nothing(known) => {
                    return ToolOutcome::err(format!(
                        "nothing to wait for: no sub-agent is running and every result has been \
                     read; {}.",
                        known_line(&known)
                    ))
                }
            };
        tokio::select! {
            biased;
            // The turn ended: the wait ends, the child does not (D6).
            _ = cancel.cancelled() => ToolOutcome::err(
                "the wait was cancelled; the sub-agent keeps running and will notify you.",
            ),
            got = tokio::time::timeout(timeout, rx) => match got {
                Ok(Ok(h)) => {
                    // Handed over at publish, after a `Notify` was sent (D5).
                    if h.notified {
                        self.unnotify(&h.id).await;
                    }
                    result_outcome(h.result)
                }
                Ok(Err(_)) => ToolOutcome::err(
                    "the sub-agent ended without a result (its session closed).",
                ),
                Err(_) => ToolOutcome::err(format!(
                    "still running after {}s; it will notify you when it finishes, or `wait` \
                     again.",
                    timeout.as_secs()
                )),
            },
        }
    }
}

impl Tool for WaitTool {
    fn name(&self) -> &'static str {
        "wait"
    }
    fn description(&self) -> &str {
        "Block until a background sub-agent finishes and return its typed result (the same \
         text its notification would carry). `wait` blocks your turn — prefer letting the \
         result arrive on its own. Omit `child` to take whichever finished first. Times out \
         after `timeout_ms` (default 120000) with the child still running."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "child": {"type": "string", "description": "The sub-agent id from `spawn`. Omit for the first one to finish."},
                "timeout_ms": {"type": "integer", "description": "How long to block. Default 120000."}
            },
            "additionalProperties": false
        })
    }
    /// Reads a result the human already approved producing (D2).
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    /// Blocks on another session's turn, not on a subprocess.
    fn awaits_child_session(&self) -> bool {
        true
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.run_impl(input, cancel))
    }
}

/// `send {child, text}`: steer a running background child.
pub struct SendTool;

impl Tool for SendTool {
    fn name(&self) -> &'static str {
        "send"
    }
    fn description(&self) -> &str {
        "Steer a running background sub-agent: the text lands in its context before its \
         next sample, like a user steer. For a finished one use `resume`."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "child": {"type": "string", "description": "The sub-agent id from `spawn`."},
                "text": {"type": "string", "description": "What to tell it."}
            },
            "required": ["child", "text"],
            "additionalProperties": false
        })
    }
    /// The child's own rules bound what it can do with the text (D2).
    fn permission(&self, _input: &Value) -> Permission {
        Permission::None
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(children) = hotl_tools::children::registry() else {
                return no_session("send");
            };
            let (Some(id), Some(text)) = (
                input.get("child").and_then(Value::as_str),
                input.get("text").and_then(Value::as_str),
            ) else {
                return ToolOutcome::err("`child` and `text` are required.");
            };
            match children.steer(id) {
                Ok(tx) => match tx.send(text.to_string()).await {
                    Ok(()) => ToolOutcome::ok(format!(
                        "sent to sub-agent {id}; it reads this before its next sample."
                    )),
                    Err(_) => ToolOutcome::err(format!("sub-agent {id} is no longer running.")),
                },
                Err(SendRefusal::Parked) => ToolOutcome::err(format!(
                    "sub-agent {id} has finished; use `resume {{child: \"{id}\", text}}` to \
                     continue it."
                )),
                Err(SendRefusal::Closed) => ToolOutcome::err(format!(
                    "sub-agent {id} is closed; `spawn` again if you need the work."
                )),
                Err(SendRefusal::Unknown(known)) => {
                    ToolOutcome::err(format!("no sub-agent `{id}`; {}.", known_line(&known)))
                }
            }
        })
    }
}

/// `resume {child, text, background?}`: continue a finished child with its
/// context intact (D7).
pub struct ResumeTool {
    notify: Option<tokio::sync::mpsc::WeakSender<SessionCmd>>,
}

impl ResumeTool {
    pub fn new(notify: Option<tokio::sync::mpsc::WeakSender<SessionCmd>>) -> Self {
        Self { notify }
    }

    async fn run_impl(&self, input: Value, cancel: CancellationToken) -> ToolOutcome {
        let Some(children) = hotl_tools::children::registry() else {
            return no_session("resume");
        };
        let (Some(id), Some(text)) = (
            input.get("child").and_then(Value::as_str),
            input.get("text").and_then(Value::as_str),
        ) else {
            return ToolOutcome::err("`child` and `text` are required.");
        };
        let background = input
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let tx = match children.resume(id) {
            Ok(tx) => tx,
            Err(ResumeRefusal::Running) => {
                return ToolOutcome::err(format!(
                    "sub-agent {id} is still running; use `send {{child: \"{id}\", text}}` \
                     to steer it, or `wait` for it."
                ))
            }
            Err(ResumeRefusal::Closed) => {
                return ToolOutcome::err(format!(
                    "sub-agent {id} is closed (stopped, evicted past `[agents] max_children`, \
                     or the session ended); `spawn` again with `fork: true` if you need its \
                     context."
                ))
            }
            Err(ResumeRefusal::Isolated) => {
                return ToolOutcome::err(format!(
                    "sub-agent {id} ran in its own worktree, which was spent at merge-back; an \
                     isolated child is not resumable. `spawn` again with `fork: true` if you \
                     need its context."
                ))
            }
            Err(ResumeRefusal::Unknown(known)) => {
                return ToolOutcome::err(format!("no sub-agent `{id}`; {}.", known_line(&known)))
            }
        };
        let record = children.record(id);
        let read_only = record.as_ref().is_some_and(|r| r.read_only);
        // D8 holds for a resume too: a background turn that can write would
        // share the tree with a parent that keeps working.
        if background && !read_only {
            return ToolOutcome::err(format!(
                "`background: true` is refused for sub-agent {id}: its tools can edit the \
                 working tree. Resume it in the foreground (drop `background`)."
            ));
        }
        if background {
            if tx
                .send(ResumeRequest {
                    text: text.to_string(),
                    reply: None,
                })
                .await
                .is_err()
            {
                return ToolOutcome::err(format!("sub-agent {id} is no longer reachable."));
            }
            // The child is running again; the band and `/agents` see it.
            let _ = &self.notify;
            return ToolOutcome::ok(format!(
                "Resumed sub-agent {id} in the background. You will be told when it finishes; \
                 `wait {{child: \"{id}\"}}` blocks for its result. Carry on."
            ));
        }
        // A foreground resume of a child that can write serializes against
        // other shared-tree children, as its first run did.
        let _guard = match read_only {
            true => None,
            false => Some(crate::spawn::mutating_child_lock().lock().await),
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        if tx
            .send(ResumeRequest {
                text: text.to_string(),
                reply: Some(reply),
            })
            .await
            .is_err()
        {
            return ToolOutcome::err(format!("sub-agent {id} is no longer reachable."));
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // The wait ends; the child finishes its turn and parks (D6).
                children.stop(id);
                ToolOutcome::err("the resume was cancelled; the sub-agent was stopped.")
            }
            got = rx => match got {
                Ok(result) => result_outcome(result),
                Err(_) => ToolOutcome::err(format!(
                    "sub-agent {id} ended without a result (its session closed)."
                )),
            },
        }
    }
}

impl Tool for ResumeTool {
    fn name(&self) -> &'static str {
        "resume"
    }
    fn description(&self) -> &str {
        "Continue a finished sub-agent with its context intact: it reads `text` as its next \
         prompt and returns a typed result like `spawn`. Not resumable: isolated children \
         (their worktree is spent), stopped or evicted ones. `background: true` returns at \
         once (read-only children only)."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "child": {"type": "string", "description": "The sub-agent id from `spawn`."},
                "text": {"type": "string", "description": "Its next prompt."},
                "background": {"type": "boolean", "description": "Return at once; the result arrives as a notification. Default false."}
            },
            "required": ["child", "text"],
            "additionalProperties": false
        })
    }
    /// Asks like `spawn` (D2): new work under the human's eye.
    fn permission(&self, input: &Value) -> Permission {
        let id = input.get("child").and_then(Value::as_str).unwrap_or("?");
        let text = input.get("text").and_then(Value::as_str).unwrap_or("?");
        let short: String = text.chars().take(80).collect();
        let bg = match input.get("background").and_then(Value::as_bool) {
            Some(true) => " (background)",
            _ => "",
        };
        Permission::Ask {
            summary: format!("resume sub-agent {id}: {short}{bg}"),
        }
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn awaits_child_session(&self) -> bool {
        true
    }
    fn run<'a>(&'a self, input: Value, cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.run_impl(input, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::tests::{reports, test_concurrency, typed_tool, TypedChild};
    use crate::spawn::SpawnTool;
    use hotl_provider::ScriptedProvider;
    use hotl_tools::children::{ChildState, Children, CHILDREN};
    use hotl_types::{Item, SyntheticReason};
    use std::sync::Arc;
    use std::time::Duration;

    type Script = Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>>;

    struct Fixture {
        reg: Arc<Children>,
        tx: tokio::sync::mpsc::Sender<SessionCmd>,
        rx: tokio::sync::mpsc::Receiver<SessionCmd>,
        spawn: SpawnTool,
        builder: Arc<TypedChild>,
    }

    fn fixture(scripts: Vec<Script>) -> Fixture {
        let (tx, rx) = tokio::sync::mpsc::channel::<SessionCmd>(16);
        let builder = Arc::new(TypedChild::new(scripts));
        let spawn = typed_tool(builder.clone(), tempfile::tempdir().unwrap().keep())
            .with_notify(tx.downgrade())
            .with_prefix_stagger(Duration::ZERO);
        Fixture {
            reg: Arc::new(Children::default()),
            tx,
            rx,
            spawn,
            builder,
        }
    }

    fn slow(scripts: Vec<Script>, ms: u64) -> Fixture {
        let (tx, rx) = tokio::sync::mpsc::channel::<SessionCmd>(16);
        let builder = Arc::new(TypedChild::new(scripts).slow(ms));
        let spawn = typed_tool(builder.clone(), tempfile::tempdir().unwrap().keep())
            .with_notify(tx.downgrade())
            .with_prefix_stagger(Duration::ZERO);
        Fixture {
            reg: Arc::new(Children::default()),
            tx,
            rx,
            spawn,
            builder,
        }
    }

    fn hanging(scripts: Vec<Script>) -> Fixture {
        let (tx, rx) = tokio::sync::mpsc::channel::<SessionCmd>(16);
        let builder = Arc::new(TypedChild::new(scripts).hanging());
        let spawn = typed_tool(builder.clone(), tempfile::tempdir().unwrap().keep())
            .with_notify(tx.downgrade())
            .with_prefix_stagger(Duration::ZERO);
        Fixture {
            reg: Arc::new(Children::default()),
            tx,
            rx,
            spawn,
            builder,
        }
    }

    async fn in_session<T>(reg: &Arc<Children>, fut: impl std::future::Future<Output = T>) -> T {
        CHILDREN
            .scope(
                reg.clone(),
                hotl_tools::CURRENT_CALL_ID.scope("call_1".into(), fut),
            )
            .await
    }

    impl Fixture {
        async fn start(&self, background: bool) -> String {
            let out = in_session(
                &self.reg,
                self.spawn.run(
                    json!({"agent_type": "explore", "task": "survey", "background": background}),
                    CancellationToken::new(),
                ),
            )
            .await;
            assert!(!out.is_error, "{}", out.content);
            match background {
                true => out
                    .content
                    .strip_prefix("Started sub-agent ")
                    .and_then(|s| s.split(' ').next())
                    .unwrap()
                    .to_string(),
                false => self.reg.list()[0].id.clone(),
            }
        }
        fn wait(&self) -> WaitTool {
            WaitTool::new(Some(self.tx.downgrade()))
        }
        fn resume(&self) -> ResumeTool {
            ResumeTool::new(Some(self.tx.downgrade()))
        }
        async fn until_done(&self, id: &str) {
            for _ in 0..400 {
                if matches!(self.reg.record(id).unwrap().state, ChildState::Done { .. }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("child {id} never finished");
        }
        async fn next_cmd(&mut self) -> Option<SessionCmd> {
            tokio::time::timeout(Duration::from_millis(600), self.rx.recv())
                .await
                .ok()
                .flatten()
        }
    }

    fn done_scripts() -> Vec<Script> {
        vec![
            reports("completed", "found it"),
            ScriptedProvider::text_reply("stopping"),
        ]
    }

    #[tokio::test]
    async fn wait_returns_the_typed_result_and_sends_unnotify() {
        let mut f = fixture(done_scripts());
        let id = f.start(true).await;
        f.until_done(&id).await;
        // The inbox got its copy first…
        assert!(
            matches!(f.next_cmd().await, Some(SessionCmd::Notify { key: Some(k), .. }) if k == id)
        );
        let out = in_session(
            &f.reg,
            f.wait().run(json!({"child": id}), CancellationToken::new()),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("\"outcome\": \"completed\""));
        assert!(out.content.contains("typed=\"true\""));
        assert!(
            out.facts.delegation.is_some(),
            "the wait's reader charges the bill"
        );
        // …and the claim withdraws it, strictly after.
        assert!(matches!(f.next_cmd().await, Some(SessionCmd::Unnotify { key }) if key == id));
    }

    #[tokio::test]
    async fn wait_parked_before_the_child_finished_gets_it_directly_and_nothing_is_notified() {
        // A child that takes a while, so the wait parks before it finishes.
        let mut f = slow(done_scripts(), 400);
        let id = f.start(true).await;
        let out = in_session(
            &f.reg,
            f.wait().run(
                json!({"child": id, "timeout_ms": 10_000}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("found it"));
        // No `Notify` and no `Unnotify`: the result had one reader.
        while let Ok(cmd) = f.rx.try_recv() {
            match cmd {
                SessionCmd::Notify { text, .. } => panic!("notified: {text}"),
                SessionCmd::Unnotify { key } => panic!("unnotified {key}: nothing to withdraw"),
                _ => {}
            }
        }
        assert!(matches!(
            f.reg.record(&id).unwrap().state,
            ChildState::Done { claimed: true, .. }
        ));
    }

    #[tokio::test]
    async fn wait_times_out_and_the_child_keeps_running() {
        let f = hanging(vec![ScriptedProvider::tool_call(
            "g1",
            "glob",
            json!({"pattern": "*.rs"}),
        )]);
        let id = f.start(true).await;
        let out = in_session(
            &f.reg,
            f.wait().run(
                json!({"child": id, "timeout_ms": 200}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("still running after 0s"),
            "{}",
            out.content
        );
        assert_eq!(f.reg.record(&id).unwrap().state, ChildState::Running);
        // The slot is free for the next wait.
        let out = in_session(
            &f.reg,
            f.wait().run(
                json!({"child": id, "timeout_ms": 100}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(out.content.contains("still running"), "{}", out.content);
        f.reg.stop_all();
    }

    #[tokio::test]
    async fn wait_with_no_id_returns_the_first_finished() {
        let f = fixture(done_scripts());
        let id = f.start(true).await;
        f.until_done(&id).await;
        let out = in_session(&f.reg, f.wait().run(json!({}), CancellationToken::new())).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("found it"));
        let out = in_session(&f.reg, f.wait().run(json!({}), CancellationToken::new())).await;
        assert!(out.is_error);
        assert!(
            out.content.contains("nothing to wait for"),
            "{}",
            out.content
        );
        assert!(out.content.contains(&id), "{}", out.content);
    }

    #[tokio::test]
    async fn wait_cancel_cancels_the_wait_not_the_child() {
        let f = hanging(vec![ScriptedProvider::tool_call(
            "g1",
            "glob",
            json!({"pattern": "*.rs"}),
        )]);
        let id = f.start(true).await;
        let cancel = CancellationToken::new();
        let waiter = {
            let cancel = cancel.clone();
            let id = id.clone();
            let reg = f.reg.clone();
            let tool = f.wait();
            async move { in_session(&reg, tool.run(json!({"child": id}), cancel)).await }
        };
        let canceller = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        };
        let (out, ()) = tokio::join!(waiter, canceller);
        assert!(out.is_error);
        assert!(out.content.contains("keeps running"), "{}", out.content);
        assert_eq!(f.reg.record(&id).unwrap().state, ChildState::Running);
        f.reg.stop_all();
    }

    #[tokio::test]
    async fn wait_on_an_unknown_id_names_the_known_ones() {
        let f = fixture(done_scripts());
        let id = f.start(true).await;
        let out = in_session(
            &f.reg,
            f.wait()
                .run(json!({"child": "zzzzzz"}), CancellationToken::new()),
        )
        .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("no sub-agent `zzzzzz`"),
            "{}",
            out.content
        );
        assert!(out.content.contains(&id), "{}", out.content);
        f.reg.stop_all();
        let out = in_session(
            &Arc::new(Children::default()),
            f.wait()
                .run(json!({"child": "a"}), CancellationToken::new()),
        )
        .await;
        assert!(
            out.content.contains("do not survive a resumed session"),
            "{}",
            out.content
        );
    }

    /// The steer lands in the child's next sample as an `Item::User` tagged
    /// `Steer`, like a human's.
    #[tokio::test]
    async fn send_steers_a_running_child() {
        let f = slow(
            vec![
                ScriptedProvider::tool_call("g1", "glob", json!({"pattern": "*.rs"})),
                reports("completed", "steered"),
            ],
            300,
        );
        let id = f.start(true).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let out = in_session(
            &f.reg,
            SendTool.run(
                json!({"child": id, "text": "also check the docs"}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        f.until_done(&id).await;
        let requests = f.builder.requests_seen();
        let second = &requests[1];
        let steered = second.items.iter().any(|i| {
            matches!(
                i.as_ref(),
                Item::User { text, synthetic: Some(SyntheticReason::Steer), .. }
                    if text == "also check the docs"
            )
        });
        assert!(steered, "the child's second request carries the steer");
    }

    #[tokio::test]
    async fn send_to_a_parked_child_says_resume() {
        let f = fixture(done_scripts());
        let id = f.start(true).await;
        f.until_done(&id).await;
        let out = in_session(
            &f.reg,
            SendTool.run(
                json!({"child": id, "text": "more"}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(out.is_error);
        assert!(out.content.contains("use `resume"), "{}", out.content);
        let out = in_session(
            &f.reg,
            SendTool.run(
                json!({"child": "nope00", "text": "x"}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(
            out.content.contains("no sub-agent `nope00`"),
            "{}",
            out.content
        );
    }

    /// The child's second request body holds its first turn: no replay, the
    /// same actor.
    #[tokio::test]
    async fn resume_continues_a_parked_child_with_its_context() {
        let f = fixture(vec![
            reports("completed", "first answer"),
            reports("completed", "second answer"),
        ]);
        let id = f.start(true).await;
        f.until_done(&id).await;
        let out = in_session(
            &f.reg,
            f.resume().run(
                json!({"child": id, "text": "now the follow-up"}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("second answer"), "{}", out.content);
        assert!(out.facts.delegation.is_some());
        let requests = f.builder.requests_seen();
        let second = requests.last().unwrap();
        let texts: Vec<String> = second
            .items
            .iter()
            .filter_map(|i| match i.as_ref() {
                Item::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.contains("survey")), "{texts:?}");
        assert!(texts.iter().any(|t| t == "now the follow-up"), "{texts:?}");
        assert!(matches!(
            f.reg.record(&id).unwrap().state,
            ChildState::Done { claimed: true, .. }
        ));
    }

    #[tokio::test]
    async fn resume_background_true_returns_at_once_and_notifies() {
        let mut f = fixture(vec![
            reports("completed", "first answer"),
            reports("completed", "second answer"),
        ]);
        let id = f.start(true).await;
        f.until_done(&id).await;
        assert!(matches!(
            f.next_cmd().await,
            Some(SessionCmd::Notify { .. })
        ));
        let out = in_session(
            &f.reg,
            f.resume().run(
                json!({"child": id, "text": "again", "background": true}),
                CancellationToken::new(),
            ),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.starts_with("Resumed sub-agent"),
            "{}",
            out.content
        );
        let cmd = tokio::time::timeout(Duration::from_secs(10), f.rx.recv())
            .await
            .unwrap()
            .unwrap();
        let SessionCmd::Notify { text, key, .. } = cmd else {
            panic!()
        };
        assert_eq!(key.as_deref(), Some(id.as_str()));
        assert!(text.contains("second answer"), "{text}");
    }

    #[tokio::test]
    async fn resume_of_a_closed_child_is_refused_and_a_running_one_says_send() {
        let f = hanging(vec![ScriptedProvider::tool_call(
            "g1",
            "glob",
            json!({"pattern": "*.rs"}),
        )]);
        let id = f.start(true).await;
        let out = in_session(
            &f.reg,
            f.resume()
                .run(json!({"child": id, "text": "x"}), CancellationToken::new()),
        )
        .await;
        assert!(
            out.content.contains("still running; use `send"),
            "{}",
            out.content
        );
        f.reg.stop(&id);
        let out = in_session(
            &f.reg,
            f.resume()
                .run(json!({"child": id, "text": "x"}), CancellationToken::new()),
        )
        .await;
        assert!(out.content.contains("is closed"), "{}", out.content);
        assert!(out.content.contains("`fork: true`"), "{}", out.content);
    }

    #[test]
    fn resume_asks_like_spawn_and_wait_and_send_do_not() {
        let r = ResumeTool::new(None);
        let p = r.permission(&json!({"child": "abc123", "text": "x".repeat(100)}));
        let Permission::Ask { summary } = p else {
            panic!()
        };
        assert!(summary.starts_with("resume sub-agent abc123: xxxx"));
        assert_eq!(summary.len(), "resume sub-agent abc123: ".len() + 80);
        let Permission::Ask { summary } =
            r.permission(&json!({"child": "abc123", "text": "x", "background": true}))
        else {
            panic!()
        };
        assert!(summary.ends_with(" (background)"));
        assert!(matches!(
            WaitTool::new(None).permission(&json!({})),
            Permission::None
        ));
        assert!(matches!(SendTool.permission(&json!({})), Permission::None));
    }

    #[test]
    fn wait_and_resume_draw_no_subproc_permit() {
        assert!(WaitTool::new(None).awaits_child_session());
        assert!(ResumeTool::new(None).awaits_child_session());
        assert!(!SendTool.awaits_child_session());
        let _ = test_concurrency();
    }

    #[tokio::test]
    async fn the_verbs_need_a_session() {
        for out in [
            WaitTool::new(None)
                .run(json!({}), CancellationToken::new())
                .await,
            SendTool
                .run(json!({"child": "a", "text": "b"}), CancellationToken::new())
                .await,
            ResumeTool::new(None)
                .run(json!({"child": "a", "text": "b"}), CancellationToken::new())
                .await,
        ] {
            assert!(out.is_error);
            assert!(
                out.content.contains("needs a live session"),
                "{}",
                out.content
            );
        }
    }
}
