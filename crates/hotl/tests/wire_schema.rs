//! 0076: the published wire schemas are generated from the frame types, and
//! the committed files under `site/public/schema/` must match them. Set
//! `UPDATE_WIRE_SCHEMA=1` to regenerate. Every pinned frame validates against
//! the committed schema — the proof it describes the bytes, not just the types.

use std::path::PathBuf;

use hotl_engine::{EngineEvent, Outcome};
use hotl_types::TokenUsage;
use serde_json::{json, Value};

#[path = "../src/wire.rs"]
#[allow(dead_code)]
mod wire;

#[path = "../src/images.rs"]
#[allow(dead_code)]
mod images;

#[path = "../src/acp.rs"]
#[allow(dead_code)]
mod acp;

#[path = "../src/schema_cli.rs"]
#[allow(dead_code)]
mod schema_cli;

/// Read at runtime: a compile-time `env!` goes stale under a shared `target/`.
fn schema_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"))
        .join("../../site/public/schema")
}

fn check(file: &str, which: &str) {
    let path = schema_dir().join(file);
    let want = schema_cli::render(which).unwrap();
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    if have == want {
        return;
    }
    if std::env::var("UPDATE_WIRE_SCHEMA").as_deref() == Ok("1") {
        std::fs::create_dir_all(schema_dir()).unwrap();
        std::fs::write(&path, &want).unwrap();
        return;
    }
    panic!(
        "{} is out of date with the Rust types. Regenerate: \
         UPDATE_WIRE_SCHEMA=1 cargo test -p hotl --test wire_schema",
        path.display()
    );
}

#[test]
fn the_committed_json_stream_schema_matches_the_types() {
    check("json-stream-v2.schema.json", "json-stream");
}

#[test]
fn the_committed_acp_schema_matches_the_types() {
    check("acp-v1.schema.json", "acp");
}

fn committed(file: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(schema_dir().join(file)).unwrap()).unwrap()
}

#[test]
fn schema_ids_name_the_published_urls() {
    assert_eq!(
        committed("json-stream-v2.schema.json")["$id"],
        "https://nrakochy.github.io/hotl/schema/json-stream-v2.schema.json"
    );
    assert_eq!(
        committed("acp-v1.schema.json")["$id"],
        "https://nrakochy.github.io/hotl/schema/acp-v1.schema.json"
    );
}

#[test]
fn every_pinned_frame_validates_against_the_published_schema() {
    let schema = committed("json-stream-v2.schema.json");
    let v = jsonschema::validator_for(&schema).expect("a valid schema");
    let usage = TokenUsage {
        input_tokens: 10,
        cache_read_input_tokens: 90,
        cache_creation_5m_input_tokens: 4,
        ..Default::default()
    };
    let events = vec![
        EngineEvent::TextDelta("hi".into()),
        EngineEvent::ToolStart {
            id: "t1".into(),
            name: "read".into(),
            summary: "read ./x".into(),
        },
        EngineEvent::ToolDone {
            id: "t1".into(),
            name: "read".into(),
            ok: true,
            lines: 3,
            bytes: 5,
        },
        EngineEvent::ChildTool {
            parent_id: "p".into(),
            id: "c".into(),
            name: "n".into(),
            summary: "s".into(),
            ok: None,
            tokens: None,
        },
        EngineEvent::ChildTool {
            parent_id: "p".into(),
            id: "c".into(),
            name: "n".into(),
            summary: "s".into(),
            ok: Some(true),
            tokens: Some(3),
        },
        EngineEvent::Retrying {
            attempt: 1,
            max: 5,
            reason: "429".into(),
            delay_ms: 10,
            status: Some(429),
            request_id: None,
            scope: hotl_engine::RetryScope::Sample,
            discarded_partial: true,
        },
        EngineEvent::Compacted {
            degraded: false,
            focus: Some("f".into()),
            usage,
            estimate: 3,
        },
        EngineEvent::CacheWarmed {
            usage,
            cost_usd: Some(0.1),
        },
        EngineEvent::GoalVerdict {
            verdict: hotl_engine::GoalVerdictKind::Met,
            reason: "r".into(),
            turns: 1,
            usage,
            evidence: vec!["e".into()],
        },
        EngineEvent::JobStarted {
            id: None,
            job: "j".into(),
            pid: 1,
            command: "c".into(),
            detached: false,
        },
        EngineEvent::TodosChanged {
            items: vec![hotl_types::Todo {
                content: "x".into(),
                ..Default::default()
            }],
        },
        EngineEvent::GoalChanged { condition: None },
        EngineEvent::PromptQueued,
        EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage,
            mispredictions: 1,
        },
        EngineEvent::TurnDone {
            outcome: Outcome::Budget {
                kind: "cost".into(),
                used: 2.0,
                cap: 1.0,
            },
            usage,
            mispredictions: 0,
        },
    ];
    for e in &events {
        for model in ["test-model", "claude-opus-5"] {
            let frame = wire::json_frame(e, model);
            assert!(
                v.is_valid(&frame),
                "{frame} does not validate: {:?}",
                v.iter_errors(&frame)
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// Validate `value` against one `$defs` entry of the ACP schema.
fn acp_valid(def: &str, value: &Value) {
    let schema = committed("acp-v1.schema.json");
    let root = json!({"$ref": format!("#/$defs/{def}"), "$defs": schema["$defs"]});
    let v = jsonschema::validator_for(&root).expect("a valid schema");
    assert!(
        v.is_valid(value),
        "{def} rejects {value}: {:?}",
        v.iter_errors(value)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn every_acp_payload_validates_against_the_published_schema() {
    acp_valid(
        "InitializeResult",
        &serde_json::to_value(acp::InitializeResult {
            protocol_version: "0.1".into(),
            schema_version: 1,
            skills: vec![acp::RosterEntry {
                name: "a".into(),
                description: "b".into(),
            }],
            workflows: vec![],
            default_mode: "ask".into(),
            default_plan: false,
            context_window: 200_000,
            stream_idle_secs: 90,
            agent_info: acp::AgentInfo {
                name: "hotl".into(),
                title: "hotl".into(),
                version: "0".into(),
            },
            agent_capabilities: acp::AgentCapabilities {
                meta: acp::CapabilitiesMeta {
                    hotl: acp::HotlMeta {
                        dialect: "0.1".into(),
                        acp_schema: "schema-v1.26.0".into(),
                        extensions: vec!["session/new".into()],
                    },
                },
            },
        })
        .unwrap(),
    );
    acp_valid(
        "PromptResult",
        &json!({"schemaVersion": 1, "outcome": {"kind": "done", "text": "x"}, "usage": wire::usage_frame("claude-opus-5", &TokenUsage { input_tokens: 3, ..Default::default() })}),
    );
    acp_valid(
        "SessionUpdateParams",
        &json!({"schemaVersion": 1, "sessionId": "s", "update": wire::update_frame(&EngineEvent::TextDelta("x".into())).unwrap()}),
    );
    acp_valid(
        "SessionUpdateParams",
        &json!({"schemaVersion": 1, "sessionId": "s", "update": {"type": "turn_done", "outcome": {"kind": "cancelled"}}}),
    );
    acp_valid(
        "RequestPermissionParams",
        &json!({"sessionId": "s", "summary": "bash: ls", "protectedWhy": null}),
    );
    acp_valid("Ack", &json!({"ok": true}));
    acp_valid(
        "ModelAck",
        &json!({"ok": true, "model": "m", "applied": false, "warnings": []}),
    );
}
