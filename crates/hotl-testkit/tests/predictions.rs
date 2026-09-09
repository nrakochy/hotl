//! Expectation-checked tool batches (0050 T5), through the real actor, the
//! real tool registry and a scripted provider.
//!
//! The claim: a model that states what it expects a call to return gets the
//! rest of that batch cancelled when the result misses, plus one reminder
//! naming the call — and a batch that states nothing behaves exactly as it
//! did before, byte for byte.
//!
//! It lives in `hotl-testkit` rather than `hotl-engine` because both halves
//! it needs are this crate's: `Harness` drives the real stack, and `wire`
//! owns the cache-prefix relation the last test asserts.

use hotl_engine::EngineConfig;
use hotl_provider::ScriptedProvider;
use hotl_testkit::{tool_batch, Harness};
use hotl_types::{EntryPayload, Item, SyntheticReason};
use serde_json::{json, Value};

fn config() -> EngineConfig {
    EngineConfig {
        model: "test-model".into(),
        ..Default::default()
    }
}

/// The `t1`/`t2`/`t3` results of the batch, in log order.
fn results(h: &Harness) -> Vec<(String, String, bool)> {
    h.items()
        .into_iter()
        .filter_map(|i| match i {
            Item::ToolResults { results } => Some(results),
            _ => None,
        })
        .flatten()
        .map(|r| (r.tool_use_id, r.content, r.is_error))
        .collect()
}

fn reminders(h: &Harness) -> Vec<String> {
    h.entries()
        .into_iter()
        .filter_map(|e| match e.payload {
            EntryPayload::Item {
                item:
                    Item::User {
                        text,
                        synthetic: Some(SyntheticReason::Misprediction),
                        ..
                    },
            } => Some(text),
            _ => None,
        })
        .collect()
}

/// Three calls; the first states an expectation the shell will not meet.
fn batch(
    expect: Option<Value>,
) -> Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>> {
    let mut bash = json!({"command": "false"});
    if let Some(e) = expect {
        bash["expect"] = e;
    }
    tool_batch(&[
        ("t1", "bash", bash),
        ("t2", "read", json!({"path": "Cargo.toml"})),
        ("t3", "grep", json!({"pattern": "hotl"})),
    ])
}

/// The headline: the miss is reported on the call that made it, the rest of
/// the batch does not run, one reminder follows, and the turn continues.
#[tokio::test]
async fn a_missed_expectation_stops_the_rest_of_the_batch() {
    let mut h = Harness::new(
        vec![
            batch(Some(json!({"exit": "zero"}))),
            ScriptedProvider::text_reply("I was wrong about that"),
        ],
        config(),
    );
    let outcome = h.prompt_and_wait("go").await;
    assert!(
        matches!(outcome, hotl_engine::Outcome::Done { .. }),
        "{outcome:?}"
    );

    let r = results(&h);
    assert_eq!(r.len(), 3, "{r:?}");
    assert!(
        r[0].1
            .ends_with("[expectation failed: expected exit 0, got exit 1]"),
        "{}",
        r[0].1
    );
    for skipped in &r[1..] {
        assert_eq!(
            skipped.1, "Not executed: call 1 mispredicted (expected exit 0, got exit 1).",
            "{skipped:?}"
        );
        assert!(skipped.2, "a not-executed call is an error result");
    }

    let said = reminders(&h);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("Call 1 (`bash`)"), "{}", said[0]);
    assert!(said[0].contains("exit 0"), "{}", said[0]);
    assert!(said[0].contains("observed exit 1"), "{}", said[0]);
    assert!(said[0].contains("2 call(s)"), "{}", said[0]);
    assert_eq!(h.mispredictions, 1);

    // The reminder follows the results, not the other way round.
    let kinds = h.kinds();
    assert!(kinds.len() > 2, "{kinds:?}");
    assert_eq!(h.provider.request_count(), 2, "the turn continued");
}

/// The unaffected path: the same batch without `expect` runs whole, says
/// nothing, and counts nothing.
#[tokio::test]
async fn a_batch_without_expect_is_untouched() {
    let mut h = Harness::new(
        vec![batch(None), ScriptedProvider::text_reply("done")],
        config(),
    );
    h.prompt_and_wait("go").await;

    let r = results(&h);
    assert_eq!(r.len(), 3, "{r:?}");
    assert!(!r[0].1.contains("expectation failed"), "{}", r[0].1);
    for ran in &r[1..] {
        assert!(
            !ran.1.starts_with("Not executed"),
            "every call must run: {ran:?}"
        );
    }
    assert!(reminders(&h).is_empty());
    assert_eq!(h.mispredictions, 0);
}

/// `grep`'s prediction reads the typed `matched` fact, so "no matches" —
/// which is a *success* with prose — is still checkable.
///
/// It also pins the chunk boundary the stop respects: `grep` and `read` are
/// both parallel-safe, so they run together and BOTH results are real; the
/// stop takes effect at the next chunk. Cancelling a call that already ran
/// would be a lie about what happened.
#[tokio::test]
async fn a_search_miss_stops_the_next_chunk_not_the_one_already_running() {
    let mut h = Harness::new(
        vec![
            tool_batch(&[
                (
                    "t1",
                    "grep",
                    json!({"pattern": "package", "path": "Cargo.toml", "expect": {"matches": "none"}}),
                ),
                ("t2", "read", json!({"path": "Cargo.toml"})),
                ("t3", "bash", json!({"command": "true"})),
            ]),
            ScriptedProvider::text_reply("noted"),
        ],
        config(),
    );
    h.prompt_and_wait("go").await;

    let r = results(&h);
    assert!(
        r[0].1
            .contains("expectation failed: expected no matches, got matches"),
        "{}",
        r[0].1
    );
    assert!(
        !r[1].1.starts_with("Not executed"),
        "the chunk had already run: {:?}",
        r[1]
    );
    assert_eq!(
        r[2].1,
        "Not executed: call 1 mispredicted (expected no matches, got matches)."
    );
    assert_eq!(h.mispredictions, 1);
    let said = reminders(&h);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("1 call(s)"), "{}", said[0]);
}

/// A surprise is not an error: a command that exited 0 when the model
/// predicted failure keeps `is_error: false` and still stops the batch.
#[tokio::test]
async fn a_miss_on_a_successful_call_keeps_its_result_clean() {
    let mut h = Harness::new(
        vec![
            tool_batch(&[
                (
                    "t1",
                    "bash",
                    json!({"command": "true", "expect": {"exit": "nonzero"}}),
                ),
                ("t2", "read", json!({"path": "Cargo.toml"})),
            ]),
            ScriptedProvider::text_reply("huh"),
        ],
        config(),
    );
    h.prompt_and_wait("go").await;
    let r = results(&h);
    assert!(!r[0].2, "a surprise is not a tool error: {r:?}");
    assert!(r[0].1.contains("expectation failed"), "{}", r[0].1);
    assert!(r[1].1.starts_with("Not executed"), "{r:?}");
    assert_eq!(h.mispredictions, 1);
}

/// A malformed prediction is the model's own mistake, not a surprise about
/// the world: reported on the call, batch runs on, nothing counted.
#[tokio::test]
async fn a_malformed_expect_is_reported_and_does_not_stop_the_batch() {
    let mut h = Harness::new(
        vec![
            tool_batch(&[
                (
                    "t1",
                    "bash",
                    json!({"command": "true", "expect": {"colour": "blue"}}),
                ),
                ("t2", "read", json!({"path": "Cargo.toml"})),
            ]),
            ScriptedProvider::text_reply("fixed"),
        ],
        config(),
    );
    h.prompt_and_wait("go").await;
    let r = results(&h);
    assert!(r[0].1.contains("expectation ignored"), "{}", r[0].1);
    assert!(r[0].1.contains("colour"), "{}", r[0].1);
    assert!(!r[1].1.starts_with("Not executed"), "{r:?}");
    assert!(reminders(&h).is_empty());
    assert_eq!(h.mispredictions, 0);
}

/// Two mispredictions in one prompt sum to one number on the single
/// `TurnDone` — the same shape `usage` has.
#[tokio::test]
async fn the_count_accumulates_over_a_prompt() {
    let mut h = Harness::new(
        vec![
            tool_batch(&[(
                "t1",
                "bash",
                json!({"command": "false", "expect": {"exit": "zero"}}),
            )]),
            tool_batch(&[(
                "t2",
                "bash",
                json!({"command": "false", "expect": {"exit": "zero"}}),
            )]),
            ScriptedProvider::text_reply("twice wrong"),
        ],
        config(),
    );
    h.prompt_and_wait("go").await;
    assert_eq!(h.mispredictions, 2);
    assert_eq!(reminders(&h).len(), 2);
}

/// The cache claim: everything a miss adds — the trailer, the not-executed
/// results, the reminder — rides *after* the last durable marker, so the
/// prefix a provider already cached stays a prefix.
#[tokio::test]
async fn a_mismatch_does_not_break_the_cache_prefix() {
    let mut h = Harness::new(
        vec![
            batch(Some(json!({"exit": "zero"}))),
            ScriptedProvider::text_reply("ok"),
        ],
        config(),
    );
    h.prompt_and_wait("go").await;
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    let breaks = hotl_testkit::wire::cache_prefix_breaks(&requests);
    assert!(breaks.is_empty(), "prefix broke at {breaks:?}");
}
