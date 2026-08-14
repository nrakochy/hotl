//! 0033 Task 10: prompt admission rides the ticket pipeline — the turn
//! spawns with the prompt's eagerly-minted ticket plus a predicted snapshot
//! and dispatches sample 1 speculatively, so the prompt's fsync resolves
//! during TLS + TTFB instead of before request-building starts. Adoption,
//! mismatch and crash semantics are identical to mid-turn speculation.
//!
//! Every scenario here stands on the store's writer ack gate
//! (`SessionLog::pause_ack_handle`): closed, a forwarded entry is durable on
//! disk but its ack is withheld — exactly the window admission dispatch must
//! overlap.

use hotl_engine::{EngineConfig, EngineEvent, Outcome};
use hotl_provider::ScriptedProvider;
use hotl_testkit::Harness;

/// Poll (yielding) until `cond` holds or ~2s pass. The scripted provider
/// records requests synchronously at `stream()`, so this observes the
/// speculative dispatch the moment the turn makes it.
async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    for _ in 0..2_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The overlap itself: with the prompt's ack withheld, the provider must
/// receive the sample-1 request anyway; on release the turn adopts — exactly
/// one provider call, no sequential rebuild — and completes normally.
#[tokio::test]
async fn admission_dispatch_overlaps_the_prompt_fsync() {
    let mut h = Harness::new(
        vec![ScriptedProvider::text_reply("hi back")],
        EngineConfig::default(),
    );
    h.pause_writer(true);
    h.handle.prompt("hello".to_string()).await;
    let provider = h.provider.clone();
    wait_until(
        || !provider.requests().is_empty(),
        "the speculative sample-1 request while the ack is withheld",
    )
    .await;
    // The dispatch happened strictly before the prompt was ack'd durable.
    assert!(
        h.pause_acks.load(std::sync::atomic::Ordering::SeqCst),
        "the gate must still be closed when the request goes out"
    );
    h.pause_writer(false);
    let outcome = h.wait_for_outcome().await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    let requests = h.provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "adopted: exactly one provider call, no sequential rebuild"
    );
    // The projection carried the prompt: the request was built from it.
    assert!(
        requests[0]
            .items
            .iter()
            .any(|i| matches!(&**i, hotl_types::Item::User { text, .. } if text == "hello")),
        "the speculative request must carry the admitted prompt"
    );
}

/// The refusal path: an entry that lands between admission and the first
/// boundary (a steer released behind the prompt's ack) moves the leaf, so
/// the speculative stream is dropped and the sequential rebuild — steer
/// included, prefix byte-stable — is what samples. Exactly two provider
/// calls.
#[tokio::test]
async fn an_intervening_steer_refuses_adoption_and_rebuilds() {
    let mut h = Harness::new(
        vec![ScriptedProvider::text_reply("hi back")],
        EngineConfig::default(),
    );
    h.pause_writer(true);
    h.handle.prompt("hello".to_string()).await;
    let provider = h.provider.clone();
    wait_until(
        || !provider.requests().is_empty(),
        "the speculative dispatch",
    )
    .await;
    // A steer while the turn runs is held, and released exactly when the
    // prompt's commit settles — the intervening entry.
    h.handle.steer("actually, be brief".to_string()).await;
    h.pause_writer(false);
    let outcome = h.wait_for_outcome().await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    let requests = h.provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "mispredict: the dropped speculation plus the sequential rebuild"
    );
    let has_steer = |req: &hotl_provider::SamplingRequest| {
        req.items.iter().any(
            |i| matches!(&**i, hotl_types::Item::User { text, .. } if text.contains("be brief")),
        )
    };
    assert!(
        !has_steer(&requests[0]),
        "the speculative request predates the steer"
    );
    assert!(
        has_steer(&requests[1]),
        "the rebuild must include the steer"
    );
    // Prefix stability at the item level: the rebuild is the speculative
    // request's projection plus what intervened, never a reshuffle.
    let spec_items = &requests[0].items;
    let rebuilt = &requests[1].items;
    assert_eq!(
        &rebuilt[..spec_items.len()],
        &spec_items[..],
        "the rebuild extends the speculative projection as a strict prefix"
    );
}

/// A sealed log surfaces through the turn: the fast path forwards the
/// prompt, the writer fails it, and the ticket reports the seal at the first
/// boundary — the outcome kind is an error, exactly as the awaited path
/// reported it.
#[tokio::test]
async fn a_sealed_log_surfaces_as_a_turn_error() {
    let mut h = Harness::new(
        vec![ScriptedProvider::text_reply("never sampled to completion")],
        EngineConfig::default(),
    );
    h.inject_fault(hotl_store::WriteFault::FailBeforeWrite);
    h.handle.prompt("hello".to_string()).await;
    let outcome = h.wait_for_outcome().await;
    assert!(
        matches!(outcome, Outcome::Error { .. }),
        "a sealed log must end the turn with an error outcome, got {outcome:?}"
    );
}

/// The hook gate holds: with a `USER_PROMPT` hook unmasked, admission takes
/// the awaited path — the provider sees NO request until the prompt's ack
/// releases (the hook may append a leaf-moving reminder, which would make
/// every admission dispatch a billed mispredict).
#[tokio::test]
async fn a_user_prompt_hook_keeps_the_awaited_path() {
    struct PromptHook;
    impl hotl_engine::hooks::Hooks for PromptHook {
        fn pre_tool<'a>(
            &'a self,
            _name: &'a str,
            _input: &'a serde_json::Value,
        ) -> futures_util::future::BoxFuture<'a, hotl_engine::hooks::PreToolDecision> {
            Box::pin(std::future::ready(
                hotl_engine::hooks::PreToolDecision::Continue,
            ))
        }
        fn post_tool<'a>(
            &'a self,
            _name: &'a str,
            _result: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Option<String>> {
            Box::pin(std::future::ready(None))
        }
        fn event_mask(&self) -> hotl_engine::hooks::EventMask {
            hotl_engine::hooks::EventMask::USER_PROMPT
        }
    }
    let mut h = Harness::with_hooks(
        vec![ScriptedProvider::text_reply("hi back")],
        EngineConfig::default(),
        std::sync::Arc::new(PromptHook),
    );
    h.pause_writer(true);
    h.handle.prompt("hello".to_string()).await;
    // Bounded proof of "no request while the ack is withheld": the awaited
    // path blocks inside the admission append, so nothing reaches the
    // provider however long we let the runtime spin.
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        h.provider.requests().is_empty(),
        "the awaited path must not dispatch before the ack"
    );
    h.pause_writer(false);
    let outcome = h.wait_for_outcome().await;
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    assert_eq!(h.provider.requests().len(), 1);
    let _ = EngineEvent::PromptQueued; // keep the EngineEvent import honest
}
