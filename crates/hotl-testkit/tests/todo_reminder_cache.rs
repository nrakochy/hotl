//! The todo reminder (0056 T1) is ephemeral session context, not history:
//! it rides the request's ephemeral tail, so growing it — or gaining a
//! `failed` step, which adds a `failed="N"` attribute the block did not carry
//! before — must cost nothing in cache.
//!
//! This is the claim the T1 conditional attribute rests on, re-measured after
//! 0057 made the per-model chars-per-token ratio drive every estimate: an
//! estimator change moves *when* a fold happens, never which bytes are
//! cached, and this pins that the two stay separate.

use hotl_engine::EngineConfig;
use hotl_provider::ScriptedProvider;
use hotl_testkit::{wire, Harness};
use hotl_types::{Todo, TodoStatus};

fn config() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        ..Default::default()
    }
}

fn todo(content: &str, status: TodoStatus) -> Todo {
    Todo {
        content: content.into(),
        status,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_changing_todo_list_never_breaks_the_cached_prefix() {
    let mut h = Harness::new(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("two"),
            ScriptedProvider::text_reply("three"),
        ],
        config(),
    );
    h.prompt_and_wait("first").await;

    h.handle
        .set_plan_nodes(
            // Both settled: an open step would make the TodoGate extend the
            // turn, which is a different mechanism and not what this prices.
            vec![
                todo("build", TodoStatus::Completed),
                todo("test", TodoStatus::Completed),
            ],
            Vec::new(),
        )
        .await;
    h.prompt_and_wait("second").await;

    // A step fails: the block gains `failed="1"` and a `[!]` mark.
    h.handle
        .set_plan_nodes(
            vec![
                todo("build", TodoStatus::Completed),
                todo("test", TodoStatus::Failed),
            ],
            Vec::new(),
        )
        .await;
    h.prompt_and_wait("third").await;

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3, "one request per prompt");
    let breaks = wire::cache_prefix_breaks(&requests);
    assert!(
        breaks.is_empty(),
        "the todo list is ephemeral; changing it must not break cache: {breaks:?}"
    );

    // …and it really did reach the model, in the tail, with the attribute —
    // a test that only proved "no break" would also pass if the reminder had
    // silently stopped riding at all.
    let tail = |i: usize| -> String {
        requests[i]
            .ephemeral_tail
            .iter()
            .filter_map(|it| match it.as_ref() {
                hotl_types::Item::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(tail(1).contains("<todos>"), "{}", tail(1));
    assert!(tail(2).contains("<todos failed=\"1\">"), "{}", tail(2));
    assert!(tail(2).contains("[!] test"), "{}", tail(2));
}
