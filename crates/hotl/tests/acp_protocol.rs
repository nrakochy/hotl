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
    Box::new(|_spec| {
        let dir = tempfile::tempdir().expect("tmp");
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).expect("log");
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
            ScriptedProvider::text_reply("all done via acp"),
        ]));
        // Keep the tempdir alive for the session's lifetime.
        std::mem::forget(dir);
        Ok(spawn_session(SessionDeps {
            provider,
            registry: Arc::new(Registry::builtin()),
            rules: Arc::new(Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "sys".into(),
            cwd: std::env::temp_dir(),
            snapshots: None,
            initial_items: Vec::new(),
            config: EngineConfig { max_turns: 6, ..Default::default() },
        }))
    })
}

async fn send(w: &mut (impl AsyncWriteExt + Unpin), v: Value) {
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

#[tokio::test]
async fn initialize_new_prompt_permission_and_result() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory()));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    // 1. initialize → carries the stable schema version.
    send(&mut cwrite, json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).await;
    let init = next(&mut lines).await;
    assert_eq!(init["result"]["schemaVersion"], acp::UPDATE_SCHEMA_VERSION);

    // 2. session/new → a session id.
    send(&mut cwrite, json!({"jsonrpc":"2.0","id":2,"method":"session/new"})).await;
    let new = next(&mut lines).await;
    let session_id = new["result"]["sessionId"].as_str().expect("session id").to_string();

    // 3. session/prompt → streams updates, requests permission, resolves.
    send(&mut cwrite, json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}})).await;

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
                send(&mut cwrite, json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}})).await;
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
    assert_eq!(result["result"]["schemaVersion"], acp::UPDATE_SCHEMA_VERSION);
    assert!(result["result"].get("usage").is_some(), "usage rides in the stable result");
    assert!(saw_tool_start, "tool status streamed as an update");

    // 4. unknown method → JSON-RPC error, no crash.
    send(&mut cwrite, json!({"jsonrpc":"2.0","id":9,"method":"bogus/method"})).await;
    let err = loop {
        let m = next(&mut lines).await;
        if m.get("id") == Some(&json!(9)) {
            break m;
        }
    };
    assert!(err["error"]["message"].as_str().unwrap().contains("unknown method"));
}

async fn next(lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>) -> Value {
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("acp frame timeout")
        .expect("io")
        .expect("eof");
    serde_json::from_str(&line).expect("valid json frame")
}
