//! Plan mode changes the advertised tool roster (0050 T2), and the roster is
//! part of every cached prefix — so a toggle is a cache miss by construction.
//! This test pins the *price*: exactly one break, at the toggle, and none
//! before or after. A helper that rebuilt the roster per sample, or a
//! reminder committed outside the toggle's own actor arm, would show up here
//! as a second break.

use hotl_engine::EngineConfig;
use hotl_provider::ScriptedProvider;
use hotl_testkit::{wire, Harness};
use hotl_tools::rules::{PermissionMode, Rules};

fn config() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn plan_toggle_is_one_cache_break() {
    let mut h = Harness::with_rules(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("two"),
            ScriptedProvider::text_reply("three"),
        ],
        config(),
        Rules::default()
            .with_mode(PermissionMode::Ask)
            .with_plan(true),
    );
    h.prompt_and_wait("first").await;
    h.prompt_and_wait("second").await;
    h.handle.set_plan(false).await;
    h.prompt_and_wait("third").await;

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3, "one request per prompt");
    let breaks = wire::cache_prefix_breaks(&requests);
    assert_eq!(
        breaks.len(),
        1,
        "expected exactly one break (the toggle), got {breaks:?}"
    );
    // The break is the toggle, not the first pair.
    assert_eq!(breaks[0], 1, "the break must be at the toggle: {breaks:?}");
}
