//! 0072 T3 — a background child's completion through the real turn loop,
//! driven by hand-sent `Notify`/`Unnotify` commands where no real child is
//! needed: idle starts a `SubagentResult` turn, running holds for the batch
//! boundary, `Unnotify` withdraws a held one by key, a child's bill charges
//! the parent's caps on admit, the goal gate waits, and children are stopped
//! before the session-end hook.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use hotl_engine::hooks::InProcessHooks;
use hotl_engine::{EngineConfig, EngineEvent, Outcome, SessionCmd};
use hotl_provider::ScriptedProvider;
use hotl_testkit::Harness;
use hotl_tools::children::{ChildCharge, ChildResult, ChildSpec, Deliver};
use hotl_tools::{DelegationFacts, OutcomeFacts, Permission, Registry, Tool, ToolOutcome};
use hotl_types::{Item, SyntheticReason, TokenUsage};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// `(kind, synthetic)` per item, the coarse order proof.
fn shape(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            Item::User { synthetic, .. } => format!("user {synthetic:?}"),
            Item::Assistant { blocks, .. } => {
                if blocks.iter().any(|b| b["type"] == "tool_use") {
                    "assistant tool_use".into()
                } else {
                    "assistant".into()
                }
            }
            Item::ToolResults { .. } => "tool_results".into(),
            other => format!("{other:?}").chars().take(12).collect(),
        })
        .collect()
}

fn bash(
    id: &str,
    command: &str,
) -> Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>> {
    ScriptedProvider::tool_call(id, "bash", json!({"command": command}))
}

async fn notify(h: &Harness, text: &str, key: Option<&str>, charge: Option<ChildCharge>) {
    let tx = h.handle.commands().upgrade().unwrap();
    tx.send(SessionCmd::Notify {
        text: text.into(),
        synthetic: SyntheticReason::SubagentResult,
        key: key.map(str::to_string),
        charge,
    })
    .await
    .unwrap();
}

async fn unnotify(h: &Harness, key: &str) {
    let tx = h.handle.commands().upgrade().unwrap();
    tx.send(SessionCmd::Unnotify { key: key.into() })
        .await
        .unwrap();
}

fn big_bill() -> ChildCharge {
    ChildCharge {
        usage: TokenUsage {
            input_tokens: 2_000_000,
            output_tokens: 500_000,
            ..Default::default()
        },
        model: "claude-opus-4-8".into(),
        tool_calls: 10,
    }
}

fn subagent_items(items: &[Item]) -> usize {
    items
        .iter()
        .filter(|i| {
            matches!(
                i,
                Item::User {
                    synthetic: Some(SyntheticReason::SubagentResult),
                    ..
                }
            )
        })
        .count()
}

/// Drive the turn until `id`'s `ToolDone`, answering asks; the next sample is
/// then streaming (with a paused completion) and a command sent now is held.
async fn until_tool_done(h: &mut Harness, id: &str) {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), h.handle.events.recv())
            .await
            .expect("event timeout")
            .expect("event channel closed");
        h.seen.push(format!("{ev:?}"));
        match ev {
            EngineEvent::Ask { reply, .. } => {
                let _ = reply.send(hotl_engine::AskReply::Allow);
            }
            EngineEvent::ToolDone { id: done, .. } if done == id => break,
            EngineEvent::TurnDone { .. } => panic!("the turn ended before {id} finished"),
            _ => {}
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_child_notify_while_idle_starts_a_subagent_result_turn() {
    let mut h = Harness::new(
        vec![ScriptedProvider::text_reply("read the child's result")],
        EngineConfig::default(),
    );
    notify(
        &h,
        "<subagent-result child=\"abc123\">done</subagent-result>",
        Some("abc123"),
        None,
    )
    .await;
    let outcome = h.wait_for_outcome().await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        shape(&h.items()),
        vec!["user Some(SubagentResult)", "assistant"]
    );
}

#[tokio::test]
async fn a_child_notify_while_running_lands_after_the_batch_and_the_turn_continues() {
    let mut h = Harness::with_paused_completion(
        vec![
            bash("t1", "true"),
            bash("t2", "true"),
            ScriptedProvider::text_reply("answered the child"),
        ],
        EngineConfig::default(),
        Duration::from_millis(1500),
    );
    h.handle.prompt("go".into()).await;
    until_tool_done(&mut h, "t1").await;
    notify(&h, "child a1 is done", Some("a1"), None).await;
    h.wait_for_outcome().await;
    assert_eq!(
        shape(&h.items()),
        vec![
            "user None",
            "assistant tool_use",
            "tool_results",
            "assistant tool_use",
            "tool_results",
            "user Some(SubagentResult)",
            "assistant",
        ],
        "held through the in-flight sample, landed after its batch"
    );
    assert_eq!(
        h.seen.iter().filter(|e| e.starts_with("TurnDone")).count(),
        1
    );
}

#[tokio::test]
async fn a_child_notify_never_splits_a_tool_batch() {
    let mut h = Harness::with_paused_completion(
        vec![
            hotl_testkit::tool_batch(&[
                ("a", "bash", json!({"command": "sleep 0.4"})),
                ("b", "bash", json!({"command": "sleep 0.4"})),
            ]),
            ScriptedProvider::text_reply("ok"),
        ],
        EngineConfig::default(),
        Duration::from_millis(1000),
    );
    h.handle.prompt("go".into()).await;
    // Mid-batch: `a` is done, `b` still runs.
    until_tool_done(&mut h, "a").await;
    notify(&h, "child done mid-batch", Some("k"), None).await;
    h.wait_for_outcome().await;
    let items = h.items();
    assert_eq!(
        shape(&items),
        vec![
            "user None",
            "assistant tool_use",
            "tool_results",
            "user Some(SubagentResult)",
            "assistant",
        ],
        "after the whole batch's results, never between them"
    );
    for (i, item) in items.iter().enumerate() {
        if let Item::Assistant { blocks, .. } = item {
            if blocks.iter().any(|b| b["type"] == "tool_use") {
                assert!(
                    matches!(items.get(i + 1), Some(Item::ToolResults { .. })),
                    "{:?}",
                    shape(&items)
                );
            }
        }
    }
    assert_eq!(subagent_items(&items), 1, "{:?}", shape(&items));
}

#[tokio::test]
async fn unnotify_drops_a_held_notification_by_key() {
    let mut h = Harness::with_paused_completion(
        vec![
            bash("t1", "true"),
            ScriptedProvider::text_reply("done"),
            ScriptedProvider::text_reply("answered k2"),
        ],
        EngineConfig::default(),
        Duration::from_millis(1500),
    );
    h.handle.prompt("go".into()).await;
    until_tool_done(&mut h, "t1").await;
    notify(&h, "child k1 result", Some("k1"), None).await;
    notify(&h, "child k2 result", Some("k2"), None).await;
    unnotify(&h, "k1").await;
    h.wait_for_outcome().await;
    // k2 is released at turn end and earns a continuation; k1 never lands.
    h.wait_for_outcome().await;
    let items = h.items();
    assert_eq!(subagent_items(&items), 1, "{:?}", shape(&items));
    assert!(items
        .iter()
        .any(|i| matches!(i, Item::User { text, .. } if text == "child k2 result")));
    assert!(!items
        .iter()
        .any(|i| matches!(i, Item::User { text, .. } if text == "child k1 result")));
}

#[tokio::test]
async fn a_late_unnotify_after_release_is_a_no_op() {
    let mut h = Harness::new(
        vec![
            ScriptedProvider::text_reply("seen"),
            ScriptedProvider::text_reply("still fine"),
        ],
        EngineConfig::default(),
    );
    notify(&h, "child z9 result", Some("z9"), None).await;
    h.wait_for_outcome().await;
    unnotify(&h, "z9").await;
    let outcome = h.prompt_and_wait("carry on").await;
    assert!(matches!(outcome, Outcome::Done { .. }));
    assert_eq!(
        subagent_items(&h.items()),
        1,
        "released means the model has it"
    );
}

/// The D5 proof: the `Unnotify` is sent only after the `Notify` send
/// completed, from a different sender — channel order still drops it.
#[tokio::test]
async fn notify_then_unnotify_from_two_senders_keep_channel_order() {
    let mut h = Harness::with_paused_completion(
        vec![bash("t1", "true"), ScriptedProvider::text_reply("done")],
        EngineConfig::default(),
        Duration::from_millis(1500),
    );
    h.handle.prompt("go".into()).await;
    until_tool_done(&mut h, "t1").await;
    let a = h.handle.commands().upgrade().unwrap();
    let b = h.handle.commands().upgrade().unwrap();
    a.send(SessionCmd::Notify {
        text: "child q result".into(),
        synthetic: SyntheticReason::SubagentResult,
        key: Some("q".into()),
        charge: None,
    })
    .await
    .unwrap();
    b.send(SessionCmd::Unnotify { key: "q".into() })
        .await
        .unwrap();
    drop((a, b));
    h.wait_for_outcome().await;
    assert_eq!(subagent_items(&h.items()), 0, "{:?}", shape(&h.items()));
    assert_eq!(
        h.seen.iter().filter(|e| e.starts_with("TurnDone")).count(),
        1
    );
}

/// A foreground `spawn`'s facts, without a child: the tool returns a bill.
struct FakeSpawn(DelegationFacts);

impl Tool for FakeSpawn {
    fn name(&self) -> &'static str {
        "fake_spawn"
    }
    fn description(&self) -> &str {
        "returns a sub-agent's bill"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        let facts = self.0.clone();
        Box::pin(async move {
            ToolOutcome::ok("child said hi").with_facts(OutcomeFacts {
                delegation: Some(Box::new(facts)),
                ..Default::default()
            })
        })
    }
}

fn registry_with(tool: impl Tool + 'static) -> Registry {
    let mut r = Registry::builtin();
    r.register(Box::new(tool));
    r
}

fn facts(bill: &ChildCharge) -> DelegationFacts {
    DelegationFacts {
        usage: bill.usage,
        model: bill.model.clone(),
        tool_calls: bill.tool_calls,
        ..Default::default()
    }
}

#[tokio::test]
async fn child_spend_counts_toward_max_cost_usd_and_the_next_batch_is_budget_stopped() {
    let mut h = Harness::with_registry(
        vec![
            ScriptedProvider::tool_call("s1", "fake_spawn", json!({})),
            ScriptedProvider::text_reply("never sampled"),
        ],
        // Above one sample's worst-case projection, far below the child's bill.
        EngineConfig {
            max_cost_usd: 5.0,
            ..Default::default()
        },
        registry_with(FakeSpawn(facts(&big_bill()))),
    );
    let outcome = h.prompt_and_wait("delegate").await;
    assert!(
        matches!(&outcome, Outcome::Budget { kind, .. } if kind == "cost_usd"),
        "{outcome:?}"
    );
    assert!(
        h.seen
            .iter()
            .any(|e| e.starts_with("BudgetNotice") && e.contains("100")),
        "{:?}",
        h.seen
    );
}

#[tokio::test]
async fn a_background_charge_is_applied_on_admit_not_on_release() {
    let mut h = Harness::with_paused_completion(
        vec![
            bash("t1", "true"),
            ScriptedProvider::text_reply("done"),
            ScriptedProvider::text_reply("must not be sampled"),
        ],
        EngineConfig {
            max_cost_usd: 5.0,
            ..Default::default()
        },
        Duration::from_millis(1500),
    );
    h.handle.prompt("go".into()).await;
    until_tool_done(&mut h, "t1").await;
    notify(&h, "child paid for", Some("p"), Some(big_bill())).await;
    h.wait_for_outcome().await;
    let notice = h.seen.iter().position(|e| e.starts_with("BudgetNotice"));
    let done = h
        .seen
        .iter()
        .position(|e| e.starts_with("TurnDone"))
        .unwrap();
    assert!(
        notice.is_some_and(|n| n < done),
        "charged while held, not at release: {:?}",
        h.seen
    );
    // The notification's own turn is then budget-stopped before it samples.
    let outcome = h.wait_for_outcome().await;
    assert!(
        matches!(&outcome, Outcome::Budget { kind, .. } if kind == "cost_usd"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn child_tool_calls_count_toward_max_tool_calls() {
    let bill = ChildCharge {
        usage: TokenUsage::default(),
        model: String::new(),
        tool_calls: 10,
    };
    let mut h = Harness::with_registry(
        vec![
            ScriptedProvider::tool_call("s1", "fake_spawn", json!({})),
            bash("t2", "true"),
            ScriptedProvider::text_reply("never"),
        ],
        EngineConfig {
            max_tool_calls: 5,
            ..Default::default()
        },
        registry_with(FakeSpawn(facts(&bill))),
    );
    let outcome = h.prompt_and_wait("delegate").await;
    assert!(
        matches!(&outcome, Outcome::Budget { kind, used, .. } if kind == "tool_calls" && *used >= 11.0),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn turn_done_usage_excludes_child_tokens() {
    let mut h = Harness::with_registry(
        vec![
            ScriptedProvider::tool_call("s1", "fake_spawn", json!({})),
            ScriptedProvider::text_reply("done"),
        ],
        EngineConfig::default(),
        registry_with(FakeSpawn(facts(&big_bill()))),
    );
    h.handle.prompt("delegate".into()).await;
    let usage = loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), h.handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        if let EngineEvent::TurnDone { usage, .. } = ev {
            break usage;
        }
    };
    assert!(
        usage.input_tokens + usage.output_tokens < 1_000,
        "the parent's own tokens only: {usage:?}"
    );
}

/// Registers a running child with the session's registry and parks its ids
/// and stop tokens where the test can read them.
#[derive(Clone, Default)]
struct Registered {
    ids: Arc<Mutex<Vec<String>>>,
    stop_all: Arc<Mutex<Option<CancellationToken>>>,
}

struct RegisterChild(Registered);

impl Tool for RegisterChild {
    fn name(&self) -> &'static str {
        "register_child"
    }
    fn description(&self) -> &str {
        "starts a fake background child"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(children) = hotl_tools::children::registry() else {
                return ToolOutcome::err("no registry");
            };
            let (steer, _) = tokio::sync::mpsc::channel(1);
            let (resume, _) = tokio::sync::mpsc::channel(1);
            let entry = children
                .register(ChildSpec {
                    call_id: hotl_tools::current_call_id(),
                    agent_type: "explore".into(),
                    summary: "fake".into(),
                    background: true,
                    isolated: false,
                    read_only: true,
                    started_ms: 0,
                    steer,
                    resume,
                })
                .unwrap();
            self.0.ids.lock().unwrap().push(entry.id.clone());
            *self.0.stop_all.lock().unwrap() = Some(entry.stop_all);
            ToolOutcome::ok(format!("started {}", entry.id))
        })
    }
}

struct FinishChild(Registered);

impl Tool for FinishChild {
    fn name(&self) -> &'static str {
        "finish_child"
    }
    fn description(&self) -> &str {
        "finishes the fake child"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let children = hotl_tools::children::registry().unwrap();
            let id = self.0.ids.lock().unwrap()[0].clone();
            let d = children.finish(&id, ChildResult::default(), 1);
            assert_eq!(d, Deliver::Notify);
            children.published(&id, 1);
            ToolOutcome::ok("finished")
        })
    }
}

#[tokio::test]
async fn the_goal_gate_waits_for_a_running_background_child() {
    let reg = Registered::default();
    let mut registry = Registry::builtin();
    registry.register(Box::new(RegisterChild(reg.clone())));
    registry.register(Box::new(FinishChild(reg.clone())));
    let mut h = Harness::with_registry(
        vec![
            ScriptedProvider::tool_call("s1", "register_child", json!({})),
            ScriptedProvider::text_reply("started a child"),
            ScriptedProvider::tool_call("f1", "finish_child", json!({})),
            ScriptedProvider::text_reply("it finished"),
            ScriptedProvider::text_reply("VERDICT: met\nREASON: done"),
        ],
        EngineConfig::default(),
        registry,
    );
    h.handle.set_goal(Some("the child reports".into())).await;
    h.prompt_and_wait("start").await;
    assert!(
        !h.seen.iter().any(|e| e.starts_with("GoalEvaluating")),
        "the gate waits while a child runs: {:?}",
        h.seen
    );
    assert_eq!(h.handle.children().await.len(), 1);
    h.prompt_and_wait("finish it").await;
    assert!(
        h.seen.iter().any(|e| e.starts_with("GoalEvaluating")),
        "{:?}",
        h.seen
    );
    assert!(h
        .seen
        .iter()
        .any(|e| e.contains("GoalVerdict") && e.contains("Met")));
}

#[tokio::test]
async fn children_are_stopped_before_the_session_end_hook() {
    let reg = Registered::default();
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let hooks = {
        let reg = reg.clone();
        let order = order.clone();
        InProcessHooks::new().on_session_end(move || {
            let stopped = reg
                .stop_all
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|t| t.is_cancelled());
            order.lock().unwrap().push(if stopped {
                "children stopped, then hook"
            } else {
                "hook before stop"
            });
        })
    };
    let mut h = Harness::with_hooks_and_registry(
        vec![
            ScriptedProvider::tool_call("s1", "register_child", json!({})),
            ScriptedProvider::text_reply("started"),
        ],
        EngineConfig::default(),
        Arc::new(hooks),
        registry_with(RegisterChild(reg.clone())),
    );
    h.prompt_and_wait("start").await;
    let id = reg.ids.lock().unwrap()[0].clone();
    assert_eq!(h.handle.children().await[0].id, id);
    // `/agents stop` reaches it by id; an unknown id says so.
    assert!(!h.handle.stop_child("nope00").await);
    let Harness { handle, .. } = h;
    handle.finish(Duration::from_secs(5)).await;
    assert_eq!(*order.lock().unwrap(), vec!["children stopped, then hook"]);
}

#[tokio::test]
async fn stop_child_interrupts_one_by_id() {
    let reg = Registered::default();
    let mut h = Harness::with_registry(
        vec![
            ScriptedProvider::tool_call("s1", "register_child", json!({})),
            ScriptedProvider::text_reply("started"),
        ],
        EngineConfig::default(),
        registry_with(RegisterChild(reg.clone())),
    );
    h.prompt_and_wait("start").await;
    let id = reg.ids.lock().unwrap()[0].clone();
    assert!(h.handle.stop_child(&id).await);
    assert!(!h.handle.stop_child(&id).await, "already closed");
    assert_eq!(
        h.handle.children().await[0].state,
        hotl_tools::children::ChildState::Closed
    );
}
