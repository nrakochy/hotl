//! The GPT-5.6 cache claim for the Responses dialect, asserted on wire bytes
//! (plan 0046): **every sample's durable prefix is a byte-identical extension
//! of the previous sample's, markers included, and no ephemeral item ever
//! carries a marker.** Under GPT-5.6's rules (explicit-breakpoint reads only,
//! writes billed at 1.25×) that is the difference between a cache that hits
//! from the second sample on and one that rewrites the whole history every
//! sample — the implicit-breakpoint-on-the-MOIM bug this plan fixed.
//!
//! The twin of `fork_cache.rs` / the in-crate Anthropic cache scenarios: same
//! `Harness`, same `ScriptedProvider`, the comparison is
//! [`hotl_testkit::wire::is_responses_prefix`] so the definition of "unchanged
//! prefix" cannot drift from the kit's. The list is edited at a turn boundary
//! (`set_todos`) rather than by a mid-turn `todo_write`: the harness owns the
//! session's command channel, so a test tool cannot reach the sink, and a
//! racing mid-turn send would make the fixture nondeterministic. Both axes
//! are still covered — durable growth inside a turn, and a changed tail plus
//! a new prompt across the boundary.

use futures_util::future::BoxFuture;
use hotl_engine::{EngineConfig, Outcome};
use hotl_provider::ScriptedProvider;
use hotl_testkit::{wire, Harness};
use hotl_tools::{Permission, Registry, Tool, ToolOutcome};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Ping;

impl Tool for Ping {
    fn name(&self) -> &'static str {
        "ping"
    }
    fn description(&self) -> &str {
        "answers immediately"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission(&self, _: &serde_json::Value) -> Permission {
        Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        input: serde_json::Value,
        _: CancellationToken,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { ToolOutcome::ok(format!("pong {input}")) })
    }
}

fn registry() -> Registry {
    let mut reg = Registry::builtin();
    reg.register(Box::new(Ping));
    reg
}

fn cached_config() -> EngineConfig {
    EngineConfig {
        max_turns: 8,
        cache_static: true,
        ..Default::default()
    }
}

/// Completed, so the reminder renders while the TodoGate never adds samples
/// this scenario would have to script for.
fn done_todo(content: &str) -> hotl_types::Todo {
    hotl_types::Todo {
        content: content.into(),
        status: hotl_types::TodoStatus::Completed,
        active_form: None,
    }
}

#[tokio::test]
async fn every_sample_extends_the_previous_prefix_and_marks_nothing_ephemeral() {
    let mut h = Harness::with_registry(
        vec![
            ScriptedProvider::tool_call("t1", "ping", json!({"n": 1})),
            ScriptedProvider::tool_call("t2", "ping", json!({"n": 2})),
            ScriptedProvider::text_reply("first done"),
            ScriptedProvider::tool_call("t3", "ping", json!({"n": 3})),
            ScriptedProvider::text_reply("second done"),
        ],
        cached_config(),
        registry(),
    );
    let first = h.prompt_and_wait("first").await;
    assert!(matches!(first, Outcome::Done { .. }), "turn 1: {first:?}");
    h.handle.set_todos(vec![done_todo("write the suite")]).await;
    let second = h.prompt_and_wait("second").await;
    assert!(matches!(second, Outcome::Done { .. }), "turn 2: {second:?}");

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 5, "one sample per scripted reply");
    // Non-vacuous: the tail really changes across the boundary, and the MOIM
    // is on every request.
    assert!(requests[2].ephemeral_tail.is_empty());
    assert_eq!(requests[3].ephemeral_tail.len(), 1);
    assert!(requests.iter().all(|r| r.turn_context.is_some()));

    for (i, pair) in requests.windows(2).enumerate() {
        let earlier = wire::responses_durable_body(&pair[0]);
        let later = wire::responses_durable_body(&pair[1]);
        assert!(
            wire::is_responses_prefix(&earlier, &later),
            "sample {i} → {}: the durable prefix moved.\nearlier:\n{earlier:#}\n\nlater:\n{later:#}",
            i + 1
        );
        assert!(
            wire::responses_markers_are_append_stable(&earlier, &later),
            "sample {i} → {}: a marker moved: {:?} then {:?}",
            i + 1,
            wire::responses_marker_positions(&earlier),
            wire::responses_marker_positions(&later)
        );
        assert!(
            later["input"].as_array().unwrap().len() > earlier["input"].as_array().unwrap().len(),
            "sample {} must have appended something",
            i + 1
        );
    }

    for (i, req) in requests.iter().enumerate() {
        let durable_len = wire::responses_durable_body(req)["input"]
            .as_array()
            .unwrap()
            .len();
        let full = hotl_provider_openai_responses::body_for(req, true);
        let input = full["input"].as_array().unwrap();
        assert!(
            input.len() > durable_len,
            "sample {i}: the tail/MOIM must be on the wire"
        );
        assert!(
            input[durable_len..]
                .iter()
                .all(|v| wire::count_breakpoints(v) == 0),
            "sample {i}: a marker on ephemeral content:\n{full:#}"
        );
        assert!(
            wire::count_breakpoints(&full) >= 1,
            "sample {i}: the durable prefix must carry at least one marker"
        );
        assert_eq!(full["prompt_cache_options"], json!({"mode": "explicit"}));
    }
    // The newest marker of each sample sits on its last durable user-role
    // item, so it is exactly what the next sample's read matches on.
    let last = wire::responses_durable_body(&requests[4]);
    let positions = wire::responses_marker_positions(&last);
    assert_eq!(positions, vec![0, 2, 4, 6, 8], "{last:#}");
}
