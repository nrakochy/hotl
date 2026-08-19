//! Pins the `-p --json` frame schema. Nothing pinned it before, which is
//! exactly how `"outcome": "Done { text: \"…\" }"` — a Rust `Debug` string —
//! survived inside a stream declared a Tier-1 contract (evaluation §5.7).
//! Every frame must be machine-parseable and version-stamped.
//!
//! The renderer module is pulled in by path, the way `acp_protocol.rs` and
//! `tui_e2e.rs` reach `acp.rs`: `crates/hotl` is a binary crate with no lib
//! target, so `wire.rs` is deliberately self-contained (it depends on
//! `hotl-engine` and `serde_json`, nothing else in the crate).

use hotl_engine::{EngineEvent, LedgerSummary, Outcome, Phase, PhaseDeltaSummary};
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
            id: "t1".into(),
            name: "read".into(),
            summary: "read ./x".into(),
        },
        EngineEvent::ToolDone {
            id: "t1".into(),
            name: "read".into(),
            ok: true,
        },
        EngineEvent::ToolDenied {
            id: "t2".into(),
            name: "write".into(),
        },
        EngineEvent::ToolAutoAllowed {
            id: "t3".into(),
            name: "bash".into(),
            rule: "ls*".into(),
        },
        EngineEvent::ToolFlagged {
            id: "t4".into(),
            name: "write".into(),
            summary: "write Makefile".into(),
            why: "protected write".into(),
            denied: false,
        },
        EngineEvent::ChildTool {
            parent_id: "t5".into(),
            id: "c1".into(),
            name: "read".into(),
            summary: "read ./x".into(),
            ok: None,
        },
        EngineEvent::ChildTool {
            parent_id: "t5".into(),
            id: "c1".into(),
            name: "read".into(),
            summary: String::new(),
            ok: Some(true),
        },
        EngineEvent::Retrying {
            attempt: 1,
            reason: "429".into(),
        },
        EngineEvent::FallbackModel { model: "m2".into() },
        EngineEvent::PromptQueued,
        EngineEvent::Compacted { degraded: false },
        EngineEvent::TodosChanged { items: Vec::new() },
        EngineEvent::GoalChanged {
            condition: Some("tests pass".into()),
        },
        EngineEvent::GoalVerdict {
            verdict: hotl_engine::GoalVerdictKind::NotYet,
            reason: "no commit yet".into(),
            turns: 1,
        },
        EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage: TokenUsage::default(),
        },
    ];
    for e in events {
        let f = wire::json_frame(&e, "test-model");
        assert!(f["type"].is_string(), "untagged frame: {f}");
        assert_eq!(f["schema_version"], json!(wire::JSON_STREAM_SCHEMA_VERSION));
        // No frame value may be a Rust Debug rendering.
        let text = f.to_string();
        assert!(!text.contains(" { "), "Debug-formatted payload in {f}");
    }
}

#[test]
fn turn_done_carries_a_structured_outcome() {
    let f = wire::json_frame(
        &EngineEvent::TurnDone {
            outcome: Outcome::DoomLoop {
                pattern: "read ./x".into(),
            },
            usage: TokenUsage::default(),
        },
        "test-model",
    );
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

/// 0034: the goal frames are additive (`JSON_STREAM_SCHEMA_VERSION` stands).
/// `goal_changed` carries the condition or an explicit `null` on clear;
/// `goal_verdict` carries a stable snake_case tag, never a Debug rendering.
#[test]
fn goal_frames_carry_their_payloads() {
    let set = wire::update_frame(&EngineEvent::GoalChanged {
        condition: Some("tests pass".into()),
    })
    .unwrap();
    assert_eq!(set["type"], "goal_changed");
    assert_eq!(set["goal"], "tests pass");
    let cleared = wire::update_frame(&EngineEvent::GoalChanged { condition: None }).unwrap();
    assert_eq!(cleared["goal"], json!(null));

    for (kind, tag) in [
        (hotl_engine::GoalVerdictKind::NotYet, "not_yet"),
        (hotl_engine::GoalVerdictKind::Met, "met"),
        (hotl_engine::GoalVerdictKind::Impossible, "impossible"),
        (hotl_engine::GoalVerdictKind::EvalFailed, "eval_failed"),
    ] {
        let f = wire::update_frame(&EngineEvent::GoalVerdict {
            verdict: kind,
            reason: "because".into(),
            turns: 3,
        })
        .unwrap();
        assert_eq!(f["type"], "goal_verdict");
        assert_eq!(f["verdict"], tag);
        assert_eq!(f["reason"], "because");
        assert_eq!(f["turns"], 3);
    }
}

/// 0037: every tool lifecycle frame carries the provider's `tool_use` id —
/// additive (`JSON_STREAM_SCHEMA_VERSION` stands, same ruling as the goal
/// frames) — so a client can pair a `tool_done` with its own `tool_start`
/// when N same-name calls run concurrently.
#[test]
fn tool_frames_carry_the_call_id() {
    let events = [
        EngineEvent::ToolStart {
            id: "toolu_1".into(),
            name: "read".into(),
            summary: "read ./x".into(),
        },
        EngineEvent::ToolDone {
            id: "toolu_1".into(),
            name: "read".into(),
            ok: true,
        },
        EngineEvent::ToolDenied {
            id: "toolu_1".into(),
            name: "write".into(),
        },
        EngineEvent::ToolAutoAllowed {
            id: "toolu_1".into(),
            name: "bash".into(),
            rule: "ls*".into(),
        },
        EngineEvent::ToolFlagged {
            id: "toolu_1".into(),
            name: "write".into(),
            summary: "write Makefile".into(),
            why: "protected write".into(),
            denied: false,
        },
    ];
    for e in events {
        let f = wire::update_frame(&e).expect("tool events are stream frames");
        assert_eq!(f["id"], "toolu_1", "missing call id on {}", f["type"]);
    }
}

/// 0039 D1: `child_tool` frames route by `parent_id`, carry an explicit
/// `phase`, and `ok` rides only the done frame — omitted, not `null`, on
/// start (the `hit_ratio` precedent), so later phases fit without reshaping.
#[test]
fn child_tool_frames_carry_parent_id_phase_and_ok_only_on_done() {
    let start = wire::update_frame(&EngineEvent::ChildTool {
        parent_id: "toolu_spawn".into(),
        id: "toolu_child".into(),
        name: "read".into(),
        summary: "read ./x".into(),
        ok: None,
    })
    .expect("child_tool is a stream frame");
    assert_eq!(start["type"], "child_tool");
    assert_eq!(start["parent_id"], "toolu_spawn");
    assert_eq!(start["id"], "toolu_child");
    assert_eq!(start["phase"], "start");
    assert!(start.get("ok").is_none(), "start must omit ok: {start}");

    let done = wire::update_frame(&EngineEvent::ChildTool {
        parent_id: "toolu_spawn".into(),
        id: "toolu_child".into(),
        name: "read".into(),
        summary: String::new(),
        ok: Some(false),
    })
    .expect("child_tool is a stream frame");
    assert_eq!(done["phase"], "done");
    assert_eq!(done["ok"], json!(false));
}

#[test]
fn thinking_deltas_carry_their_text() {
    // T3-15: the payload had no `text` field, so a consumer could not render
    // reasoning it is being billed for.
    let f = wire::json_frame(
        &EngineEvent::ThinkingDelta("reasoning".into()),
        "test-model",
    );
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
        ..Default::default()
    };
    let f = wire::json_frame(
        &EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage,
        },
        "test-model",
    );
    assert_eq!(f["usage"]["hit_ratio"], json!(0.5));
}

#[test]
fn turn_done_usage_omits_hit_ratio_without_cache_activity() {
    // Today's exact bytes for a plain, uncached scripted scenario: no
    // `hit_ratio` key at all, not `null` — the golden-transcript byte
    // stability rule extended to this new derived field.
    let f = wire::json_frame(
        &EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage: TokenUsage::default(),
        },
        "test-model",
    );
    assert!(
        f["usage"].get("hit_ratio").is_none(),
        "no cache activity must mean no hit_ratio key: {f}"
    );
}

/// Task 5: `cost_usd` rides the same `usage` object once the frame's model is
/// catalogued — it is `catalog::cost_usd` computed at the frame site, not a
/// UI estimate.
#[test]
fn turn_done_usage_carries_cost_usd_for_a_catalogued_model() {
    let usage = TokenUsage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        ..Default::default()
    };
    let f = wire::json_frame(
        &EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage,
        },
        "claude-opus-4-8",
    );
    // Opus 4.8: 5.00 input + 25.00 output per million tokens.
    let cost = f["usage"]["cost_usd"]
        .as_f64()
        .expect("a catalogued model must carry cost_usd");
    assert!((cost - 30.0).abs() < 1e-9, "was {cost}");
}

/// The acceptance instrument's other half: an uncatalogued model must never
/// fabricate a price — the key is absent entirely, not `null` and not `0`.
#[test]
fn turn_done_usage_omits_cost_usd_for_an_uncatalogued_model() {
    let usage = TokenUsage {
        input_tokens: 10,
        ..Default::default()
    };
    let f = wire::json_frame(
        &EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage,
        },
        "totally-unheard-of-model",
    );
    assert!(
        f["usage"].get("cost_usd").is_none(),
        "an uncatalogued model must omit cost_usd, not guess: {f}"
    );
}

/// 0033 Task 1: the per-phase deltas are the point of a wall-clock profiler —
/// they ride the frame (additive, `UPDATE_SCHEMA_VERSION` stands); only the
/// raw `samples` array stays off the wire.
#[test]
fn ledger_report_frame_carries_phase_deltas() {
    let summary = LedgerSummary {
        sample_count: 1,
        overhead_p50_ns: 10,
        overhead_p99_ns: 20,
        phase_deltas: vec![PhaseDeltaSummary {
            from: Phase::SnapshotReady,
            to: Phase::RequestBuilt,
            p50_ns: 5,
            p99_ns: 9,
        }],
        max_rss_bytes: 0,
        samples: vec![],
    };
    let frame = wire::update_frame(&EngineEvent::LedgerReport(summary)).unwrap();
    assert_eq!(frame["phase_deltas"][0]["from"], "SnapshotReady");
    assert_eq!(frame["phase_deltas"][0]["to"], "RequestBuilt");
    assert_eq!(frame["phase_deltas"][0]["p50_ns"], 5);
    assert_eq!(frame["phase_deltas"][0]["p99_ns"], 9);
    assert!(
        frame.get("samples").is_none(),
        "raw samples stay off the wire"
    );
}

#[test]
fn the_schema_version_reflects_the_breaking_outcome_change() {
    // Read off a real frame rather than the constant: what a consumer pins to
    // is the stamped value, and this way the stamping itself is under test.
    let stamped = wire::json_frame(&EngineEvent::PromptQueued, "test-model")["schema_version"]
        .as_u64()
        .expect("every frame carries a numeric schema_version");
    assert!(
        stamped >= 2,
        "the outcome shape changed from a Debug string to a tagged object"
    );
}
