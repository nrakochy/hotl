//! Pins the `-p --json` frame schema. Nothing pinned it before, which is
//! exactly how `"outcome": "Done { text: \"…\" }"` — a Rust `Debug` string —
//! survived inside a stream declared a Tier-1 contract (evaluation §5.7).
//! Every frame must be machine-parseable and version-stamped.
//!
//! The renderer module is pulled in by path, the way `acp_protocol.rs` and
//! `tui_e2e.rs` reach `acp.rs`: `crates/hotl` is a binary crate with no lib
//! target, so `wire.rs` is deliberately self-contained (it depends on
//! `hotl-engine` and `serde_json`, nothing else in the crate).

use hotl_engine::{EngineEvent, Outcome};
use hotl_types::TokenUsage;
use serde_json::json;

#[path = "../src/wire.rs"]
#[allow(dead_code)]
mod wire;

#[test]
fn every_frame_is_tagged_and_versioned() {
    let events = [
        EngineEvent::TextDelta("hi".into()),
        EngineEvent::ThinkingDelta("mm".into()),
        EngineEvent::ToolStart {
            name: "read".into(),
            summary: "read ./x".into(),
        },
        EngineEvent::ToolDone {
            name: "read".into(),
            ok: true,
        },
        EngineEvent::ToolDenied {
            name: "write".into(),
        },
        EngineEvent::ToolAutoAllowed {
            name: "bash".into(),
            rule: "ls*".into(),
        },
        EngineEvent::Retrying {
            attempt: 1,
            reason: "429".into(),
        },
        EngineEvent::FallbackModel { model: "m2".into() },
        EngineEvent::PromptQueued,
        EngineEvent::Compacted { degraded: false },
        EngineEvent::TodosChanged { items: Vec::new() },
        EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage: TokenUsage::default(),
        },
    ];
    for e in events {
        let f = wire::json_frame(&e);
        assert!(f["type"].is_string(), "untagged frame: {f}");
        assert_eq!(f["schema_version"], json!(wire::JSON_STREAM_SCHEMA_VERSION));
        // No frame value may be a Rust Debug rendering.
        let text = f.to_string();
        assert!(!text.contains(" { "), "Debug-formatted payload in {f}");
    }
}

#[test]
fn turn_done_carries_a_structured_outcome() {
    let f = wire::json_frame(&EngineEvent::TurnDone {
        outcome: Outcome::DoomLoop {
            pattern: "read ./x".into(),
        },
        usage: TokenUsage::default(),
    });
    assert_eq!(f["type"], "turn_done");
    assert_eq!(f["outcome"]["kind"], "doom_loop");
    assert_eq!(f["outcome"]["pattern"], "read ./x");
    assert!(f["usage"]["input_tokens"].is_u64());
}

/// Every `Outcome` variant is a distinguishable tag — a consumer must be able
/// to tell `Done` from `DoomLoop` without a Rust `Debug` parser.
#[test]
fn every_outcome_variant_is_tagged_and_carries_its_payload() {
    let cases = [
        (
            Outcome::Done { text: "t".into() },
            "done",
            Some(("text", "t")),
        ),
        (Outcome::Cancelled, "cancelled", None),
        (Outcome::TurnLimit, "turn_limit", None),
        (Outcome::Refused, "refused", None),
        (
            Outcome::DoomLoop {
                pattern: "p".into(),
            },
            "doom_loop",
            Some(("pattern", "p")),
        ),
        (
            Outcome::ToolFailureBudget {
                tool: "bash".into(),
            },
            "tool_failure_budget",
            Some(("tool", "bash")),
        ),
        (
            Outcome::Error {
                message: "m".into(),
            },
            "error",
            Some(("message", "m")),
        ),
    ];
    for (outcome, kind, payload) in cases {
        let f = wire::outcome_frame(&outcome);
        assert_eq!(f["kind"], kind, "outcome tag for {kind}");
        if let Some((key, value)) = payload {
            assert_eq!(f[key], value, "{kind} must carry `{key}`");
        }
    }
}

#[test]
fn thinking_deltas_carry_their_text() {
    // T3-15: the payload had no `text` field, so a consumer could not render
    // reasoning it is being billed for.
    let f = wire::json_frame(&EngineEvent::ThinkingDelta("reasoning".into()));
    assert_eq!(f["type"], "thinking_delta");
    assert_eq!(f["text"], "reasoning");
}

#[test]
fn turn_done_usage_carries_hit_ratio_when_cache_activity_is_present() {
    let usage = TokenUsage {
        input_tokens: 25,
        output_tokens: 5,
        cache_read_input_tokens: 50,
        cache_creation_input_tokens: 25,
    };
    let f = wire::json_frame(&EngineEvent::TurnDone {
        outcome: Outcome::Done { text: "ok".into() },
        usage,
    });
    assert_eq!(f["usage"]["hit_ratio"], json!(0.5));
}

#[test]
fn turn_done_usage_omits_hit_ratio_without_cache_activity() {
    // Today's exact bytes for a plain, uncached scripted scenario: no
    // `hit_ratio` key at all, not `null` — the golden-transcript byte
    // stability rule extended to this new derived field.
    let f = wire::json_frame(&EngineEvent::TurnDone {
        outcome: Outcome::Done { text: "ok".into() },
        usage: TokenUsage::default(),
    });
    assert!(
        f["usage"].get("hit_ratio").is_none(),
        "no cache activity must mean no hit_ratio key: {f}"
    );
}

#[test]
fn the_schema_version_reflects_the_breaking_outcome_change() {
    // Read off a real frame rather than the constant: what a consumer pins to
    // is the stamped value, and this way the stamping itself is under test.
    let stamped = wire::json_frame(&EngineEvent::PromptQueued)["schema_version"]
        .as_u64()
        .expect("every frame carries a numeric schema_version");
    assert!(
        stamped >= 2,
        "the outcome shape changed from a Debug string to a tagged object"
    );
}
