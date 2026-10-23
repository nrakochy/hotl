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

/// The roster moves in *both* directions at the toggle, and two per-session
/// tools now ride it: `recall` (0057 T5) in every roster, `present_plan`
/// (0056 T3) in the plan roster only. The price must still be one break.
///
/// `Registry::builtin` carries neither — the binary registers them per
/// session — so the test above prices a roster no real session advertises.
/// This one hands both in.
#[tokio::test]
async fn the_toggle_is_still_one_break_with_both_per_session_tools() {
    let mut registry = hotl_tools::Registry::builtin();
    registry.register(Box::new(hotl_tools::PresentPlanTool::new(
        std::sync::Arc::new(|_, _| {}),
        None,
    )));
    registry.register(Box::new(hotl_retrieval::RecallTool::new(vec![Box::new(
        hotl_retrieval::session_log::SessionLogRetriever::new(
            std::path::Path::new("/nonexistent"),
            "s".to_string(),
        ),
    )])));

    let mut h = Harness::with_rules_and_registry(
        vec![
            ScriptedProvider::text_reply("one"),
            ScriptedProvider::text_reply("two"),
            ScriptedProvider::text_reply("three"),
        ],
        config(),
        Rules::default()
            .with_mode(PermissionMode::Ask)
            .with_plan(true),
        registry,
    );
    h.prompt_and_wait("first").await;
    h.prompt_and_wait("second").await;
    h.handle.set_plan(false).await;
    h.prompt_and_wait("third").await;

    let requests = h.provider.requests();
    let names =
        |i: usize| -> Vec<String> { requests[i].tools.iter().map(|t| t.name.clone()).collect() };
    // Plan on: present_plan is offered, write/edit are not, and recall — which
    // is neither an edit tool nor plan-only — rides both rosters.
    let on = names(0);
    assert!(on.contains(&"present_plan".to_string()), "{on:?}");
    assert!(on.contains(&"recall".to_string()), "{on:?}");
    assert!(!on.contains(&"write".to_string()), "{on:?}");
    let off = names(2);
    assert!(!off.contains(&"present_plan".to_string()), "{off:?}");
    assert!(off.contains(&"recall".to_string()), "{off:?}");
    assert!(off.contains(&"write".to_string()), "{off:?}");

    let breaks = wire::cache_prefix_breaks(&requests);
    assert_eq!(
        breaks,
        vec![1],
        "two tools moving at one toggle is still one break: {breaks:?}"
    );
}
