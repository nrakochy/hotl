//! `hotl serve` — a detached session listening on a **unix socket** (the ACP
//! solution to backgrounding; no tmux). The engine outlives any client: you
//! `hotl attach` to drive it, detach (disconnect) freely, and reattach later.
//!
//! The load-bearing behavior: when the agent hits a permission ask while **no
//! client is attached**, the ask is **parked** (its reply channel held) and
//! re-issued the instant a client connects — so a detached session can still
//! act, once you return to approve. Render events that arrive while detached
//! are dropped (the full history is in the session log); pending asks are not.
//!
//! One session per process (process-per-session — the ACP model). Restart-
//! durability (surviving a reboot) is planned durable-asks work
//! and is deliberately out of scope; this parks in memory, in the live server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hotl_engine::{EngineEvent, SessionHandle};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex as AsyncMutex;

use crate::acp::{outcome_tag, update_payload, UPDATE_SCHEMA_VERSION};

type ClientWriter = Arc<AsyncMutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>>;

struct Shared {
    handle: Arc<SessionHandle>,
    client: ClientWriter,
    /// Parked permission asks: id → (reply channel, the request frame to re-send).
    pending: Mutex<HashMap<u64, (tokio::sync::oneshot::Sender<hotl_engine::AskReply>, Value)>>,
    next_ask: AtomicU64,
    session_id: String,
}

/// Directory holding one `<id>.sock` per live backgrounded session.
pub fn run_dir() -> PathBuf {
    crate::agent::sessions_dir().parent().map(|p| p.join("run")).unwrap_or_else(|| PathBuf::from("run"))
}

/// Live backgrounded sessions (their socket ids), from the run dir.
pub fn list_live() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(run_dir()) else { return Vec::new() };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "sock").then(|| p.file_stem()?.to_str().map(String::from))?
        })
        .collect();
    ids.sort();
    ids
}

/// Run a detached session bound to `run_dir/<session_id>.sock`. `handle` is a
/// freshly spawned engine session; `prompt` is an optional opening prompt.
pub async fn serve(session_id: String, handle: SessionHandle, prompt: Option<String>) -> i32 {
    let dir = run_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("hotl serve: cannot create {}: {e}", dir.display());
        return 1;
    }
    let sock = dir.join(format!("{session_id}.sock"));
    let _ = std::fs::remove_file(&sock); // clear a stale socket
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl serve: cannot bind {}: {e}", sock.display());
            return 1;
        }
    };
    let _guard = SockGuard(sock);
    serve_on(listener, session_id, handle, prompt).await;
    0
}

/// The socket-server core over a pre-bound listener (testable without the
/// filesystem run-dir): spawn the drain, submit the opening prompt, then serve
/// clients one at a time — the session lives across attach/detach.
pub async fn serve_on(
    listener: UnixListener,
    session_id: String,
    mut handle: SessionHandle,
    prompt: Option<String>,
) {
    let events = std::mem::replace(&mut handle.events, tokio::sync::mpsc::channel(1).1);
    let shared = Arc::new(Shared {
        handle: Arc::new(handle),
        client: Arc::new(AsyncMutex::new(None)),
        pending: Mutex::new(HashMap::new()),
        next_ask: AtomicU64::new(1),
        session_id,
    });
    tokio::spawn(drain_events(events, shared.clone()));
    if let Some(p) = prompt {
        shared.handle.prompt(p).await;
    }
    accept_loop(listener, shared).await;
}

/// Removes the socket file when the server exits.
struct SockGuard(PathBuf);
impl Drop for SockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn accept_loop(listener: UnixListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (read, write) = stream.into_split();
        *shared.client.lock().await = Some(Box::new(write));
        resend_pending(&shared).await;
        // Handle this client until it disconnects; the session lives on.
        let stop = handle_client(read, &shared).await;
        *shared.client.lock().await = None;
        if stop {
            break; // client asked to shut the session down
        }
    }
}

/// Read one client's frames until EOF or a `shutdown`. Returns true to stop
/// the whole server (shutdown), false to just detach.
async fn handle_client(read: impl AsyncRead + Unpin, shared: &Arc<Shared>) -> bool {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        match msg.get("t").and_then(Value::as_str).unwrap_or("") {
            "prompt" => shared.handle.prompt(str_field(&msg, "text")).await,
            "steer" => shared.handle.steer(str_field(&msg, "text")).await,
            "continue" => shared.handle.continue_turn().await,
            "cancel" => shared.handle.interrupt(),
            "ask_reply" => {
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    if let Some((reply, _)) = shared.pending.lock().expect("pending").remove(&id) {
                        let allow = msg.get("allow").and_then(Value::as_bool).unwrap_or(false);
                        let deny_msg = msg.get("message").and_then(Value::as_str).map(String::from);
                        let ans = if allow {
                            hotl_engine::AskReply::Allow
                        } else {
                            hotl_engine::AskReply::Deny { message: deny_msg }
                        };
                        let _ = reply.send(ans);
                    }
                }
            }
            "detach" => return false,
            "shutdown" => return true,
            _ => {}
        }
    }
    false // EOF = detach
}

/// Re-issue every parked ask to the newly-attached client (the whole point).
async fn resend_pending(shared: &Arc<Shared>) {
    let frames: Vec<Value> =
        shared.pending.lock().expect("pending").values().map(|(_, f)| f.clone()).collect();
    // Tell the client the current session id first (a lightweight hello).
    send(shared, &json!({"t": "hello", "sessionId": shared.session_id})).await;
    for frame in frames {
        send(shared, &frame).await;
    }
}

async fn drain_events(mut events: tokio::sync::mpsc::Receiver<EngineEvent>, shared: Arc<Shared>) {
    while let Some(event) = events.recv().await {
        match event {
            EngineEvent::Ask { summary, protected_why, reply } => {
                let id = shared.next_ask.fetch_add(1, Ordering::Relaxed);
                let frame = json!({
                    "t": "ask", "id": id, "summary": summary, "protectedWhy": protected_why,
                });
                shared.pending.lock().expect("pending").insert(id, (reply, frame.clone()));
                send(&shared, &frame).await; // no-op if detached; re-sent on attach
            }
            EngineEvent::TurnDone { outcome, usage } => {
                send(&shared, &json!({
                    "t": "turn_done",
                    "schemaVersion": UPDATE_SCHEMA_VERSION,
                    "outcome": outcome_tag(&outcome),
                    "usage": usage,
                }))
                .await;
            }
            other => {
                if let Some(update) = update_payload(&other) {
                    send(&shared, &json!({"t": "update", "update": update})).await;
                }
            }
        }
    }
}

/// Write a frame to the attached client, if any. A broken pipe drops the client.
async fn send(shared: &Arc<Shared>, frame: &Value) {
    let mut guard = shared.client.lock().await;
    if let Some(w) = guard.as_mut() {
        let mut line = frame.to_string();
        line.push('\n');
        if w.write_all(line.as_bytes()).await.is_err() || w.flush().await.is_err() {
            *guard = None;
        }
    }
}

fn str_field(v: &Value, field: &str) -> String {
    v.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// The socket path for a session id (used by the attach client).
pub fn socket_path(id: &str) -> PathBuf {
    run_dir().join(format!("{id}.sock"))
}

pub fn socket_exists(id: &str) -> bool {
    Path::new(&socket_path(id)).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
    use hotl_platform::SystemClock;
    use hotl_provider::ScriptedProvider;
    use hotl_store::{Masker, SessionLog};
    use hotl_tools::{rules::Rules, Registry};
    use tokio::net::UnixStream;

    fn scripted_session() -> SessionHandle {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).unwrap();
        std::mem::forget(dir);
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
            ScriptedProvider::text_reply("done in the background"),
        ]));
        spawn_session(SessionDeps {
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
            config: EngineConfig { max_turns: 6, ..Default::default() },
        })
    }

    async fn next(lines: &mut tokio::io::Lines<tokio::io::BufReader<impl AsyncRead + Unpin>>) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("frame timeout")
            .expect("io")
            .expect("eof");
        serde_json::from_str(&line).expect("json frame")
    }

    async fn send(w: &mut (impl AsyncWriteExt + Unpin), v: Value) {
        let mut s = v.to_string();
        s.push('\n');
        w.write_all(s.as_bytes()).await.unwrap();
        w.flush().await.unwrap();
    }

    #[tokio::test]
    async fn detach_while_asking_then_reattach_reissues_the_ask() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(serve_on(listener, "test".into(), scripted_session(), None));

        // Attach, prompt; the scripted bash call is gated → an `ask` frame.
        let (r, mut w) = UnixStream::connect(&sock).await.unwrap().into_split();
        let mut lines = tokio::io::BufReader::new(r).lines();
        send(&mut w, json!({"t":"prompt","text":"go"})).await;
        let ask_id = loop {
            let f = next(&mut lines).await;
            if f["t"] == "ask" {
                break f["id"].as_u64().unwrap();
            }
        };

        // Detach WITHOUT answering — the session (and the parked ask) live on.
        send(&mut w, json!({"t":"detach"})).await;
        drop((lines, w));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Reattach: the parked ask is re-issued (the whole point).
        let (r2, mut w2) = UnixStream::connect(&sock).await.unwrap().into_split();
        let mut lines2 = tokio::io::BufReader::new(r2).lines();
        let reissued = loop {
            let f = next(&mut lines2).await;
            if f["t"] == "ask" {
                break f["id"].as_u64().unwrap();
            }
        };
        assert_eq!(reissued, ask_id, "the same parked ask must re-issue on reattach");

        // Answer it → the turn completes.
        send(&mut w2, json!({"t":"ask_reply","id":reissued,"allow":true})).await;
        let done = loop {
            let f = next(&mut lines2).await;
            if f["t"] == "turn_done" {
                break f;
            }
        };
        assert_eq!(done["outcome"]["kind"], "done");
        assert_eq!(done["outcome"]["text"], "done in the background");
    }
}
