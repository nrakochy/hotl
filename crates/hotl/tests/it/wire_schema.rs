//! 0076: the published wire schemas are generated from the frame types, and
//! the committed files under `site/public/schema/` must match them. Set
//! `UPDATE_WIRE_SCHEMA=1` to regenerate. Every pinned frame validates against
//! the committed schema — the proof it describes the bytes, not just the types.

use std::path::PathBuf;

use hotl_engine::{EngineEvent, Outcome};
use hotl_types::TokenUsage;
use serde_json::{json, Value};

use crate::{acp, acp_ext, acp_map, acp_types, features, schema_cli, wire};

fn server_info() -> acp::ServerInfo {
    acp::ServerInfo {
        skills: Vec::new(),
        workflows: Vec::new(),
        workflows_report: || json!({"runs": []}),
        default_mode: "ask".into(),
        default_plan: false,
        context_window: 200_000,
        stream_idle_secs: 90,
        model_resolver: None,
        key_available: true,
        sessions_dir: None,
        fallback_models: Vec::new(),
        model: "m".into(),
    }
}

/// Read at runtime: a compile-time `env!` goes stale under a shared `target/`.
fn schema_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"))
        .join("../../site/public/schema")
}

fn check(file: &str, which: &str) {
    let path = schema_dir().join(file);
    let want = schema_cli::render(which).unwrap();
    // A Windows checkout may carry CRLF.
    let have = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
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
fn the_committed_acp_extensions_schema_matches_the_types() {
    check("acp-hotl-extensions.schema.json", "acp");
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
        committed("acp-hotl-extensions.schema.json")["$id"],
        "https://nrakochy.github.io/hotl/schema/acp-hotl-extensions.schema.json"
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
            input: serde_json::Value::Null,
        },
        EngineEvent::ToolDone {
            id: "t1".into(),
            name: "read".into(),
            ok: true,
            lines: 3,
            bytes: 5,
            output: String::new(),
            before: None,
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

fn ext_schema() -> Value {
    committed("acp-hotl-extensions.schema.json")
}

fn validates(schema: &Value, part: &Value, value: &Value) -> Result<(), Vec<String>> {
    let root = json!({"allOf": [part], "$defs": schema["$defs"]});
    let v = jsonschema::validator_for(&root).expect("a valid schema");
    if v.is_valid(value) {
        Ok(())
    } else {
        Err(v.iter_errors(value).map(|e| e.to_string()).collect())
    }
}

fn ext_valid(def: &str, value: &Value) {
    let schema = ext_schema();
    let part = json!({"$ref": format!("#/$defs/{def}")});
    if let Err(e) = validates(&schema, &part, value) {
        panic!("{def} rejects {value}: {e:?}");
    }
}

fn check_meta(schema: &Value, value: &Value, seen: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(meta)) = map.get("_meta") {
                for (key, payload) in meta {
                    assert!(key.starts_with("hotl.dev/"), "foreign _meta key {key}");
                    let places = schema["meta"][key]
                        .as_array()
                        .unwrap_or_else(|| panic!("{key} is not in the extension schema"));
                    let errors: Vec<_> = places
                        .iter()
                        .filter_map(|p| validates(schema, &p["schema"], payload).err())
                        .collect();
                    assert!(
                        errors.len() < places.len(),
                        "{key} = {payload} matches no schema: {errors:?}"
                    );
                    seen.insert(key.clone());
                }
            }
            for v in map.values() {
                check_meta(schema, v, seen);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| check_meta(schema, v, seen)),
        _ => {}
    }
}

fn ext_caps() -> acp::ClientCaps {
    acp::ClientCaps {
        ext: true,
        ..Default::default()
    }
}

#[test]
fn every_hotl_meta_payload_validates_against_the_published_schema() {
    let schema = ext_schema();
    let caps = ext_caps();
    let mut calls = acp_map::ToolCalls::rooted(std::env::temp_dir());
    let mut sent = vec![
        serde_json::to_value(acp::initialize_response(&server_info(), caps)).unwrap(),
        json!({"stopReason": "end_turn", "_meta": {acp::META_TURN: acp::TurnMeta {
            outcome: wire::OutcomeFrame::of(&Outcome::Done { text: "x".into() }),
            usage: wire::UsageFrame::of("claude-opus-5", &TokenUsage { input_tokens: 3, ..Default::default() }),
            mispredictions: Some(1),
        }, acp::META_OUTPUT: {"n": 3}}}),
        json!({"prompt": [], "_meta": {acp::META_OUTPUT_SCHEMA: {"type": "object"}}}),
        json!({"_meta": {acp::META_MODEL: acp_ext::ModelChange {
            model: "a".into(), previous: "b".into(), context_tokens: 1, context_window: 2,
        }}}),
        json!({"_meta": {acp::META_MODEL: acp::ModelAck {
            ok: true, model: "m".into(), applied: false, warnings: vec![],
        }}}),
        serde_json::to_value(acp_map::egress_tool_call(3, "example.com", &caps)).unwrap(),
        serde_json::to_value(acp_map::question_form(
            "s",
            &hotl_types::Question {
                header: "h".into(),
                prompt: "p".into(),
                options: vec![hotl_types::QuestionOption {
                    label: "a".into(),
                    description: None,
                }],
                multi: false,
            },
            true,
        ))
        .unwrap(),
        serde_json::to_value(calls.ask(
            &acp_map::AskView {
                call_id: "e1",
                tool: "edit",
                input: &json!({"path": "x.txt", "old_string": "a", "new_string": "b"}),
                summary: "edit x.txt",
                protected_why: Some("protected"),
            },
            &caps,
        ))
        .unwrap(),
        json!({"outcome": {"outcome": "selected", "optionId": "deny"}, "_meta": {acp_map::META_REPLY: {"message": "no"}}}),
        json!({"_meta": {acp_ext::META_EXT: 1, acp_ext::META_SOCKET_TOKEN: "t"}}),
        json!({"_meta": {acp::META_OPEN: {"name": "n", "worktree": "w"}}}),
        json!({"_meta": {acp::META_OPEN: {"ephemeral": true}}}),
        json!({"_meta": {acp_map::META_RETRY: acp_types::RetryMeta {
            attempt: 1,
            max: 3,
            status: Some(429),
            delay_ms: 100,
            scope: "sample".into(),
        }}}),
        json!({"sessionId": "s", "cwd": "/w", "_meta": {acp::META_SESSION: acp_types::SessionRowMeta {
            parent_id: Some("p".into()),
            run_id: Some("bg-1".into()),
        }}}),
    ];
    let events = [
        EngineEvent::TextDelta("hi".into()),
        EngineEvent::ToolStart {
            id: "t1".into(),
            name: "spawn".into(),
            summary: "spawn".into(),
            input: json!({}),
        },
        EngineEvent::ChildTool {
            parent_id: "t1".into(),
            id: "c".into(),
            name: "read".into(),
            summary: "s".into(),
            ok: None,
            tokens: None,
        },
        EngineEvent::ToolAutoAllowed {
            id: "t1".into(),
            name: "spawn".into(),
            rule: "spawn *".into(),
        },
        EngineEvent::ToolFlagged {
            id: "t1".into(),
            name: "spawn".into(),
            summary: "s".into(),
            why: "w".into(),
            denied: false,
        },
        EngineEvent::ToolDenied {
            id: "t1".into(),
            name: "spawn".into(),
        },
        EngineEvent::GoalChanged { condition: None },
    ];
    for event in &events {
        match acp_map::outgoing(event, &caps, &mut calls) {
            Some(acp_map::Outgoing::Update(u)) => sent.push(serde_json::to_value(u).unwrap()),
            Some(acp_map::Outgoing::Ext(frame)) => ext_valid(
                "ExtSessionUpdateParams",
                &json!({"sessionId": "s", "update": frame}),
            ),
            None => panic!("{event:?} sent nothing"),
        }
    }
    let notes: Vec<hotl_types::Item> = [Some("timed_out"), None]
        .into_iter()
        .map(|end| hotl_types::Item::User {
            text: hotl_engine::shell_note_text("ls", "a", Some(0), end),
            synthetic: Some(hotl_types::SyntheticReason::ShellCommand),
            images: Vec::new(),
        })
        .collect();
    for out in acp_map::replay_updates(&notes, true) {
        let acp_map::Outgoing::Ext(frame) = out else {
            panic!("a shell note replays as an extension update");
        };
        ext_valid(
            "ExtSessionUpdateParams",
            &json!({"sessionId": "s", "update": frame}),
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for value in &sent {
        check_meta(&schema, value, &mut seen);
    }
    let documented: std::collections::BTreeSet<String> = schema["meta"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(seen, documented, "every documented key is exercised");
}

#[test]
fn every_extension_method_is_in_the_published_schema() {
    let schema = ext_schema();
    let mut listed: Vec<&str> = acp::EXTENSION_METHODS.to_vec();
    listed.sort_unstable();
    let mut documented: Vec<&str> = schema["methods"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    documented.sort_unstable();
    assert_eq!(listed, documented);
}

#[test]
fn extension_results_validate_against_the_published_schema() {
    assert!(validates(
        &ext_schema(),
        &json!({"$ref": "#/$defs/Ack"}),
        &json!({"ok": "yes"})
    )
    .is_err());
    ext_valid("Ack", &serde_json::to_value(acp::Ack { ok: true }).unwrap());
    ext_valid(
        "ModelAck",
        &json!({"ok": true, "model": "m", "applied": false, "warnings": []}),
    );
    ext_valid("Queued", &json!({"queued": true}));
    ext_valid("StoppedAck", &json!({"ok": true, "stopped": false}));
    ext_valid(
        "JobsReport",
        &serde_json::to_value(acp::AcpOnlyUpdate::JobsReport { jobs: vec![] }).unwrap(),
    );
    ext_valid(
        "ConfigReloadFailed",
        &serde_json::to_value(acp::AcpOnlyUpdate::ConfigReloadFailed { reason: "r".into() })
            .unwrap(),
    );
    ext_valid("QuestionReply", &json!({"freeText": "later"}));
    ext_valid(
        "RequestQuestionParams",
        &serde_json::to_value(acp::RequestQuestionParams {
            session_id: "s".into(),
            header: "h".into(),
            prompt: "p".into(),
            options: vec![],
            multi: false,
        })
        .unwrap(),
    );
    ext_valid(
        "WorkflowsReport",
        &json!({"type": "workflows_report", "runs": []}),
    );
}

/// The features page's generated block is `features::markdown()`; regenerate
/// under `UPDATE_WIRE_SCHEMA=1` like the schemas.
#[test]
fn the_features_page_matches_the_table() {
    let page = schema_dir().join("../../src/content/docs/features.md");
    let text = std::fs::read_to_string(&page)
        .unwrap()
        .replace("\r\n", "\n");
    let (begin, end) = ("<!-- features:begin -->\n", "<!-- features:end -->");
    let i = text.find(begin).expect("begin marker") + begin.len();
    let j = text.find(end).expect("end marker");
    let want = features::markdown();
    if text[i..j] == want {
        return;
    }
    if std::env::var("UPDATE_WIRE_SCHEMA").as_deref() == Ok("1") {
        std::fs::write(&page, format!("{}{}{}", &text[..i], want, &text[j..])).unwrap();
        return;
    }
    panic!(
        "{} is out of date with crates/hotl/src/features.rs. Regenerate: \
         UPDATE_WIRE_SCHEMA=1 cargo test -p hotl --test wire_schema",
        page.display()
    );
}
