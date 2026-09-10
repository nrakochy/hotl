//! Clearing rewrites the durable prefix (0057 T1), so it is a cache miss by
//! construction — the same shape as a fold. This test pins the *price*:
//! exactly one break, at the clear, and none before or after.
//!
//! It lives here rather than in `hotl-engine` because `wire` owns the
//! cache-prefix relation, and only `Harness` drives the real actor against it.

use hotl_engine::EngineConfig;
use hotl_provider::ScriptedProvider;
use hotl_testkit::{wire, Harness};
use hotl_types::{Item, ToolResultItem};

/// A 2000-token window: the seeded history sits near 900 (well under the
/// 1200-token clear trigger), and the long second prompt pushes it to ~1400 —
/// past the trigger, short of the 1600 the fold needs.
fn config() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        context_window: 2_000,
        keep_results_turns: 2,
        ..Default::default()
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.into(),
        synthetic: None,
        images: Vec::new(),
    }
}

/// Two finished user turns, the first of which ran a tool with a big result.
fn seeded() -> Vec<Item> {
    vec![
        user("first"),
        Item::Assistant {
            blocks: vec![serde_json::json!({
                "type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}
            })],
        },
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: "t1".into(),
                content: "y".repeat(2_400),
                is_error: false,
            }],
        },
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "listed"})],
        },
        user("second"),
        Item::Assistant {
            blocks: vec![serde_json::json!({"type": "text", "text": "noted"})],
        },
    ]
}

#[tokio::test]
async fn clearing_is_one_cache_break() {
    let mut h = Harness::with_items(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("two"),
            ScriptedProvider::text_reply("three"),
        ],
        config(),
        seeded(),
    );
    h.prompt_and_wait("third").await;
    h.prompt_and_wait(&"a long fourth prompt. ".repeat(68))
        .await;
    h.prompt_and_wait("fifth").await;

    let requests: Vec<_> = h
        .provider
        .requests()
        .into_iter()
        .filter(|r| r.system.contains("test-system"))
        .collect();
    assert_eq!(requests.len(), 3, "one sample per prompt");
    // The clear is committed before the second prompt's request goes out, so
    // that request already carries the stub.
    assert!(
        requests[0].items.iter().any(|i| match &**i {
            Item::ToolResults { results } => results.iter().any(|r| r.content.contains("yyyy")),
            _ => false,
        }),
        "the first request still carries the body"
    );
    assert!(
        requests[1].items.iter().any(|i| match &**i {
            Item::ToolResults { results } =>
                results.iter().any(|r| r.content.starts_with("<cleared ")),
            _ => false,
        }),
        "the request after the clear carries the stub"
    );

    let breaks = wire::cache_prefix_breaks(&requests);
    assert_eq!(
        breaks.len(),
        1,
        "expected exactly one break (the clear), got {breaks:?}"
    );
    assert_eq!(breaks[0], 0, "the break is the clear: {breaks:?}");
    // And after it the prefix only grows again.
    wire::assert_stable_cache_prefix(&requests[1..]);
}
