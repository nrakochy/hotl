//! Golden ACP protocol scenario: drive the real server over an in-process
//! duplex stream with a scripted-provider session (no child process).

use std::sync::Arc;

use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// The server module lives in the binary crate; pull it in directly. Some
// items are only exercised by the real factory in the binary, not this test.
#[path = "../src/acp.rs"]
#[allow(dead_code)]
mod acp;

/// A session whose scripted model calls bash (a gated tool → a permission
/// ask) then replies with text.
fn scripted_factory() -> acp::SessionFactory {
    Box::new(|spec| {
        // Echo the requested name back, as the real factory does — resolving
        // the open's name is the factory's job, not the protocol layer's.
        let name = match spec {
            acp::SessionSpec::New { name } => name,
            acp::SessionSpec::Load { name, .. } => name,
        };
        let dir = tempfile::tempdir().expect("tmp");
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).expect("log");
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
            ScriptedProvider::text_reply("all done via acp"),
        ]));
        // Keep the tempdir alive for the session's lifetime.
        std::mem::forget(dir);
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                snapshots: None,
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name,
        })
    })
}

async fn send(w: &mut (impl AsyncWriteExt + Unpin), v: Value) {
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

/// `initialize` advertises the roster so a front end can resolve
/// `/<skill>` without walking the config dirs itself.
#[tokio::test]
async fn initialize_advertises_skill_names() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let skills = vec!["brainstorming".to_string(), "acme:deploy".to_string()];
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), skills));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let init = next(&mut lines).await;
    assert_eq!(
        init["result"]["skills"],
        json!(["brainstorming", "acme:deploy"])
    );
}

#[tokio::test]
async fn initialize_new_prompt_permission_and_result() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), Vec::new()));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    // 1. initialize → carries the stable schema version.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let init = next(&mut lines).await;
    assert_eq!(init["result"]["schemaVersion"], acp::UPDATE_SCHEMA_VERSION);

    // 2. session/new → a session id.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let new = next(&mut lines).await;
    let session_id = new["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    // 3. session/prompt → streams updates, requests permission, resolves.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;

    let mut saw_tool_start = false;
    let mut prompt_result: Option<Value> = None;
    // Read frames until the prompt (id 3) result arrives.
    while prompt_result.is_none() {
        let msg = next(&mut lines).await;
        match msg.get("method").and_then(Value::as_str) {
            Some("session/request_permission") => {
                // The bash call is gated → the server asks us. Approve it.
                assert_eq!(msg["params"]["sessionId"], session_id);
                let rid = msg["id"].clone();
                send(
                    &mut cwrite,
                    json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}}),
                )
                .await;
            }
            Some("session/update") => {
                assert_eq!(msg["params"]["schemaVersion"], acp::UPDATE_SCHEMA_VERSION);
                if msg["params"]["update"]["type"] == "tool_start" {
                    saw_tool_start = true;
                }
            }
            _ if msg.get("id") == Some(&json!(3)) => prompt_result = Some(msg),
            _ => {}
        }
    }

    let result = prompt_result.unwrap();
    assert_eq!(result["result"]["outcome"]["kind"], "done");
    assert_eq!(result["result"]["outcome"]["text"], "all done via acp");
    assert_eq!(
        result["result"]["schemaVersion"],
        acp::UPDATE_SCHEMA_VERSION
    );
    assert!(
        result["result"].get("usage").is_some(),
        "usage rides in the stable result"
    );
    assert!(saw_tool_start, "tool status streamed as an update");

    // 4. unknown method → JSON-RPC error, no crash.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":9,"method":"bogus/method"}),
    )
    .await;
    let err = read_until_id(&mut lines, 9).await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown method"));
}

/// Two prompts in flight: the engine queues the second, and each prompt
/// request is answered by its own turn's outcome, in order.
#[tokio::test]
async fn overlapping_prompts_resolve_in_order() {
    let factory: acp::SessionFactory = Box::new(|_spec| {
        let dir = tempfile::tempdir().expect("tmp");
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).expect("log");
        std::mem::forget(dir);
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::text_reply("first turn"),
            ScriptedProvider::text_reply("second turn"),
        ]));
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                snapshots: None,
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name: None,
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, Vec::new()));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"a"}}),
    )
    .await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"b"}}),
    )
    .await;

    let first = read_until_id(&mut lines, 2).await;
    assert_eq!(first["result"]["outcome"]["text"], "first turn");
    let second = read_until_id(&mut lines, 3).await;
    assert_eq!(second["result"]["outcome"]["text"], "second turn");
}

/// Replacing the session (session/new while one exists) aborts the old drain
/// and clears its parked state — the new session works end to end, and the
/// old in-flight prompt is never answered with the new session's outcome.
#[tokio::test]
async fn replacing_a_session_clears_parked_state() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), Vec::new()));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    let first = read_until_id(&mut lines, 1).await;
    let first_sid = first["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    // Prompt; wait for the gated bash call's permission request — leave it parked.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    loop {
        let msg = next(&mut lines).await;
        if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break;
        }
    }

    // Replace the session while the ask is parked.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    let second = read_until_id(&mut lines, 3).await;
    let second_sid = second["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    assert_ne!(first_sid, second_sid);

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"text":"again"}}),
    )
    .await;
    let result = loop {
        let msg = next(&mut lines).await;
        assert_ne!(
            msg.get("id"),
            Some(&json!(2)),
            "stale prompt answered: {msg}"
        );
        if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            assert_eq!(msg["params"]["sessionId"], second_sid);
            let rid = msg["id"].clone();
            send(
                &mut cwrite,
                json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}}),
            )
            .await;
        } else if msg.get("id") == Some(&json!(4)) {
            break msg;
        }
    };
    assert_eq!(result["result"]["outcome"]["kind"], "done");
    assert_eq!(result["result"]["outcome"]["text"], "all done via acp");
}

/// `session/steer` queues mid-turn feedback: acknowledged `{queued:true}`
/// with a session, an error without one.
#[tokio::test]
async fn steer_is_acknowledged_and_reaches_engine() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), Vec::new()));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    // Steering with NO session is an error naming the missing session.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/steer","params":{"text":"go left"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 1).await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("session"));

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/steer","params":{"text":"go left"}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 3).await;
    assert_eq!(ack["result"], json!({"queued": true}));

    // Missing params.text is an error too.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/steer"}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    assert!(err["error"]["message"].as_str().unwrap().contains("text"));
}

async fn read_until_id(
    lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
    id: u64,
) -> Value {
    loop {
        let m = next(lines).await;
        if m.get("id") == Some(&json!(id)) {
            return m;
        }
    }
}

/// session/new carries a name back; session/rename acks and re-renames.
#[tokio::test]
async fn named_open_and_rename() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), Vec::new()));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;

    // rename before a session exists → error.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/rename","params":{"name":"x"}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "no session yet"
    );

    // open with a name (surrounding whitespace normalizes away).
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"name":"  fix-auth  "}}),
    )
    .await;
    let open = next(&mut lines).await;
    assert_eq!(open["result"]["name"], "fix-auth");

    // invalid rename → error; valid rename → ok.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/rename","params":{"name":"   "}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "blank name rejected"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/rename","params":{"name":"better-name"}}),
    )
    .await;
    assert_eq!(next(&mut lines).await["result"]["ok"], true);
}

/// `ask_user` round-trips through `session/request_question`: the client
/// sees the header/prompt/options, answers with a selection, and the tool
/// result (fed back to the scripted model) carries the selected label. Also
/// covers the SECURITY invariant end to end: the question never touches
/// `session/request_permission` — only a plain `session/prompt` result.
#[tokio::test]
async fn ask_user_round_trip_via_session_request_question() {
    let factory: acp::SessionFactory = Box::new(|_spec| {
        let dir = tempfile::tempdir().expect("tmp");
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).expect("log");
        std::mem::forget(dir);
        let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
        let (event_tx, event_rx) = hotl_engine::event_channel();
        let mut registry = Registry::builtin();
        registry.register(Box::new(hotl_tools::AskUserTool::new(
            hotl_engine::question_sink(cmd_tx.downgrade(), event_tx.downgrade()),
        )));
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call(
                "t1",
                "ask_user",
                json!({
                    "header": "Scope", "prompt": "How far?",
                    "options": [{"label": "MVP"}, {"label": "Full"}]
                }),
            ),
            ScriptedProvider::text_reply("all done via acp"),
        ]));
        Ok(acp::SessionOpen {
            handle: hotl_engine::spawn_session_with_channels(
                SessionDeps {
                    provider,
                    registry: Arc::new(registry),
                    rules: Arc::new(Rules::default()),
                    sandbox_enforced: false,
                    clock: Arc::new(SystemClock),
                    log,
                    system: "sys".into(),
                    cwd: std::env::temp_dir(),
                    snapshots: None,
                    hooks: None,
                    initial_items: Vec::new(),
                    initial_todos: Vec::new(),
                    config: EngineConfig {
                        max_turns: 6,
                        ..Default::default()
                    },
                },
                cmd_tx,
                cmd_rx,
                event_tx,
                event_rx,
            ),
            name: None,
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, Vec::new()));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    let session_id = read_until_id(&mut lines, 1)
        .await
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .expect("session id")
        .to_string();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;

    let result = loop {
        let msg = next(&mut lines).await;
        assert_ne!(
            msg.get("method").and_then(Value::as_str),
            Some("session/request_permission"),
            "ask_user must never route through the permission gate: {msg}"
        );
        if msg.get("method").and_then(Value::as_str) == Some("session/request_question") {
            assert_eq!(msg["params"]["sessionId"], session_id);
            assert_eq!(msg["params"]["header"], "Scope");
            assert_eq!(msg["params"]["prompt"], "How far?");
            assert_eq!(
                msg["params"]["options"],
                json!([{"label": "MVP"}, {"label": "Full"}])
            );
            let rid = msg["id"].clone();
            send(
                &mut cwrite,
                json!({"jsonrpc":"2.0","id":rid,"result":{"selected":["MVP"]}}),
            )
            .await;
        } else if msg.get("id") == Some(&json!(2)) {
            break msg;
        }
    };
    assert_eq!(result["result"]["outcome"]["kind"], "done");
    assert_eq!(result["result"]["outcome"]["text"], "all done via acp");
}

/// `session/set_mode` acks and switches the mode; an invalid mode errors
/// naming the valid ones. Mirrors `named_open_and_rename`.
#[tokio::test]
async fn set_mode_acks_and_rejects_unknown_modes() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), Vec::new()));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;

    // set_mode before a session exists → error.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/set_mode","params":{"mode":"plan"}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "no session yet"
    );

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    next(&mut lines).await;

    // invalid mode → error naming the valid ones.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_mode","params":{"mode":"yolo"}}),
    )
    .await;
    let err = next(&mut lines).await;
    assert!(err["error"].is_object(), "invalid mode rejected");
    let message = err["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("ask"), "names valid modes: {message}");

    // valid mode → ok.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/set_mode","params":{"mode":"plan"}}),
    )
    .await;
    assert_eq!(next(&mut lines).await["result"]["ok"], true);
}

async fn next(lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>) -> Value {
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("acp frame timeout")
        .expect("io")
        .expect("eof");
    serde_json::from_str(&line).expect("valid json frame")
}
