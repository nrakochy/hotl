//! The claim that pays for phase forking, asserted on wire bytes: **a fork's
//! first request is a byte-identical durable-prefix extension of its parent's
//! last request**, so the provider serves the inherited transcript from cache
//! instead of re-billing it.
//!
//! Two things make this a proof rather than a restatement of intent:
//!
//! 1. The fork is seeded through the **production path** — `replay_chain` over
//!    the parent's on-disk log, exactly what `load_lineage` hands the session
//!    factory — not by copying the parent's in-memory items. A shortcut there
//!    would prove the harness byte-stable and say nothing about forking.
//! 2. The comparison is [`hotl_testkit::wire::is_structural_prefix`], the same
//!    relation the in-session cache scenarios assert between consecutive
//!    requests. A fork is that relation across a session boundary; asserting
//!    it from a second, hand-rolled comparison would let the two definitions
//!    of "unchanged prefix" drift apart.
//!
//! What these tests prove is the byte *precondition*. Whether the provider's
//! bounded breakpoint lookback then covers the whole inherited transcript is
//! economics a scripted provider cannot observe — documented, not asserted
//! (see the sufficiency caveat in the sessions docs).
//!
//! It lives in `hotl-testkit` rather than `hotl-engine` because both halves it
//! needs — `Harness` and `wire` — are this crate's, and hosting it in
//! `hotl-engine` would only buy a dev-dependency cycle.

use hotl_engine::{EngineConfig, Outcome};
use hotl_provider::{ProviderError, ScriptedProvider, StreamEvent};
use hotl_testkit::{wire, Harness};
use hotl_types::Item;

fn cached_config() -> EngineConfig {
    EngineConfig {
        max_turns: 6,
        cache_static: true,
        ..Default::default()
    }
}

fn replies(texts: &[&str]) -> Vec<Vec<Result<StreamEvent, ProviderError>>> {
    texts
        .iter()
        .map(|t| ScriptedProvider::text_reply(t))
        .collect()
}

/// A parent session with `turns` completed turns, seeded the way a real
/// session is (memory / project instructions as leading synthetic items) so
/// the inherited prefix under test is shaped like a real one.
async fn parent_session(turns: usize) -> Harness {
    let seed = vec![Item::User {
        text: "<memory>the project uses hotl</memory>".into(),
        synthetic: Some(hotl_types::SyntheticReason::SubagentResult),
        images: Vec::new(),
    }];
    let scripts: Vec<_> = (0..turns)
        .map(|n| ScriptedProvider::text_reply(&format!("explored area {n}")))
        .collect();
    let mut h = Harness::with_items(scripts, cached_config(), seed);
    for n in 0..turns {
        let outcome = h.prompt_and_wait(&format!("explore area {n}")).await;
        assert!(
            matches!(outcome, Outcome::Done { .. }),
            "turn {n}: {outcome:?}"
        );
    }
    h
}

/// The projection a fork inherits, loaded the way the session factory loads it.
fn replayed_projection(parent: &Harness) -> Vec<Item> {
    hotl_store::replay_chain(parent.sessions_dir(), parent.session_id())
        .expect("the parent's own log must replay")
        .items
}

#[tokio::test]
async fn a_forks_first_request_extends_the_parents_last_request_byte_identically() {
    let parent = parent_session(3).await;
    let parent_last = parent
        .provider
        .last_request()
        .expect("the parent sampled at least once");

    let inherited = replayed_projection(&parent);
    // The seed plus three turns. Load-bearing, not decorative: the seed lives
    // in the log only because a fresh session commits it (`record_fresh_seed`)
    // — without that, replay returns 6 items, the fork's projection starts one
    // block short of the parent's, every message shifts, and the byte claim
    // below fails outright rather than degrading.
    assert_eq!(
        inherited.len(),
        7,
        "the parent's seed must survive into its own replay: {inherited:#?}"
    );
    assert!(
        matches!(&inherited[0], Item::User { text, .. } if text.contains("<memory>")),
        "the inherited projection starts where the parent's did"
    );

    let mut fork = Harness::with_items(
        replies(&["here is the plan"]),
        cached_config(),
        inherited.clone(),
    );
    fork.prompt_and_wait(
        "Entering phase: Plan. Using only what you learned above, write the plan.",
    )
    .await;
    let fork_first = fork
        .provider
        .requests()
        .into_iter()
        .next()
        .expect("the fork sampled");

    let parent_body = wire::durable_wire_body(&parent_last);
    let fork_body = wire::durable_wire_body(&fork_first);
    assert!(
        wire::is_structural_prefix(&parent_body, &fork_body),
        "a fork must extend its parent's request without rewriting a byte.\n\
         parent:\n{parent_body:#}\n\nfork:\n{fork_body:#}"
    );
    // Non-vacuous: the fork really did add something, so the assertion above
    // is not passing on two identical bodies.
    assert!(
        fork_body["messages"].as_array().expect("messages").len()
            > parent_body["messages"].as_array().expect("messages").len(),
        "the phase instruction must actually be on the wire"
    );
}

#[tokio::test]
async fn a_prefix_forks_first_request_matches_the_parents_bytes_through_the_kept_items() {
    let parent = parent_session(3).await;
    let parent_last = parent.provider.last_request().expect("the parent sampled");

    // Fork at the end of turn 2 — a turn boundary, which is the only kind the
    // `--keep` resolver admits: a mid-turn cut would need `pair_tool_results`
    // repair, and a repaired projection is not byte-identical to the parent's
    // prefix, which would quietly void the very claim this test makes.
    let mut inherited = replayed_projection(&parent);
    let cut = inherited
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i, Item::Assistant { .. }))
        .map(|(n, _)| n + 1)
        .nth(1)
        .expect("three turns give three boundaries");
    inherited.truncate(cut);

    let mut fork = Harness::with_items(replies(&["revised"]), cached_config(), inherited);
    fork.prompt_and_wait("Entering phase: Refine.").await;
    let fork_first = fork
        .provider
        .requests()
        .into_iter()
        .next()
        .expect("the fork sampled");

    // The shared prefix is byte-identical through the kept items, and only
    // then do the two diverge — the fork is *not* a prefix extension of the
    // parent's last request here, because the parent went on past the cut.
    let parent_msgs = wire::without_markers(&wire::durable_wire_body(&parent_last));
    let fork_msgs = wire::without_markers(&wire::durable_wire_body(&fork_first));
    let (a, b) = (
        parent_msgs["messages"].as_array().expect("messages"),
        fork_msgs["messages"].as_array().expect("messages"),
    );
    let shared = b.len() - 1; // everything but the fork's own new prompt
    assert!(
        shared > 0 && shared < a.len(),
        "fixture: {shared} of {}",
        a.len()
    );
    for i in 0..shared {
        assert_eq!(
            a[i], b[i],
            "message {i} was rewritten; a truncated fork must still reuse the \
             parent's bytes through the cut"
        );
    }
    assert_eq!(
        parent_msgs["system"], fork_msgs["system"],
        "the system prompt is byte-stable by construction; a fork must not touch it"
    );
}

/// The tripwire. A fork that changes the system prompt is a full-price cold
/// start in a different cache namespace — the exact "helpful" injection this
/// design forbids. If someone adds per-phase system prompts later, this test
/// is what tells them what it cost.
#[tokio::test]
async fn a_fork_with_a_different_system_prompt_is_detected_as_a_prefix_break() {
    let parent = parent_session(2).await;
    let parent_last = parent.provider.last_request().expect("the parent sampled");
    let inherited = replayed_projection(&parent);

    // What a per-phase system prompt would do. The system string is fixed for
    // a session's lifetime by construction, so this is the only place it can
    // vary — which is the point.
    let mut fork = Harness::with_items_and_system(
        replies(&["plan"]),
        cached_config(),
        inherited,
        "You are now the PLANNER.",
    );
    fork.prompt_and_wait("Entering phase: Plan.").await;
    let fork_first = fork
        .provider
        .requests()
        .into_iter()
        .next()
        .expect("the fork sampled");

    assert!(
        !wire::is_structural_prefix(
            &wire::durable_wire_body(&parent_last),
            &wire::durable_wire_body(&fork_first),
        ),
        "a changed system prompt must register as a prefix break — if this \
         passes, the cache claim is being asserted vacuously"
    );
}
