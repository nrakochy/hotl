//! `hotl acp` — the ACP-shaped protocol surface (M4).
//!
//! A JSON-RPC 2.0 line protocol over stdio that drives the *same* engine the
//! REPL does: an editor or orchestrator is just another client of
//! `SessionHandle`. One session per connection (process-per-session — the
//! orchestrator pattern). Model output and tool status
//! arrive as `session/update` notifications carrying a `schema_version`
//! (Tier-1 stable, not a side-channel); permission asks are
//! `session/request_permission` round-trips to the client.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use hotl_engine::{AskReply, EngineEvent, Outcome, SessionHandle};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

/// Stable schema version of `session/update` / prompt-result payloads (an MD
/// Tier-1 contract — bump only on a breaking change).
pub const UPDATE_SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: &str = "0.1";

type Writer = Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;
type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<AskReply>>>>;

/// Map an ACP client's permission `result` to an `AskReply` (T1/§2b): a client
/// may `{"allow":true}`, `{"allow":false,"message":"…"}`, provide edited
/// `{"input":{…}}` (AllowEdited), or answer as the tool `{"respond":"…"}`.
fn ask_reply_from_result(result: Option<&Value>) -> AskReply {
    let Some(r) = result else {
        return AskReply::Deny { message: None };
    };
    if let Some(content) = r.get("respond").and_then(Value::as_str) {
        return AskReply::Respond {
            content: content.to_string(),
        };
    }
    if let Some(input) = r.get("input") {
        return AskReply::AllowEdited {
            input: input.clone(),
        };
    }
    if r.get("allow").and_then(Value::as_bool) == Some(true) {
        return AskReply::Allow;
    }
    AskReply::Deny {
        message: r.get("message").and_then(Value::as_str).map(String::from),
    }
}
/// JSON-RPC ids of in-flight prompt requests, answered in order on TurnDone
/// (the engine queues overlapping prompts and finishes them FIFO).
type PendingPrompt = Arc<std::sync::Mutex<VecDeque<Value>>>;

/// What a client asked the factory to produce.
pub enum SessionSpec {
    New {
        name: Option<String>,
    },
    Load {
        session_id: String,
        name: Option<String>,
    },
}

/// A session the factory opened: the handle plus its display name (the one
/// just given, or inherited from the resumed chain).
pub struct SessionOpen {
    pub handle: SessionHandle,
    pub name: Option<String>,
}

/// Builds a session per the client's request. The real binary wires engine
/// deps here; tests inject a scripted-provider session.
pub type SessionFactory = Box<dyn FnMut(SessionSpec) -> Result<SessionOpen, String> + Send>;

/// Drive the protocol over one connection until the client hangs up.
pub async fn serve(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
    mut factory: SessionFactory,
    skills: Vec<String>,
) {
    let writer: Writer = Arc::new(Mutex::new(Box::new(write)));
    let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let pending_prompt: PendingPrompt = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let mut next_id: u64 = 1;
    let mut session: Option<SessionState> = None;

    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // unparseable frame, no id to reply to
        };
        // A client response to one of our permission requests?
        if msg.get("method").is_none() {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some(reply) = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = reply.send(ask_reply_from_result(msg.get("result")));
                }
            }
            continue;
        }
        handle_request(
            &msg,
            &writer,
            &mut factory,
            &mut session,
            &pending,
            &pending_prompt,
            &mut next_id,
            &skills,
        )
        .await;
    }
}

struct SessionState {
    id: String,
    handle: SessionHandle,
    drain: tokio::task::JoinHandle<()>,
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    msg: &Value,
    writer: &Writer,
    factory: &mut SessionFactory,
    session: &mut Option<SessionState>,
    pending: &Pending,
    pending_prompt: &PendingPrompt,
    next_id: &mut u64,
    skills: &[String],
) {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => {
            // `skills` lets a front end resolve `/<skill>` itself — the
            // roster is server-side knowledge, so the client never has to
            // walk the config dirs to know what a slash could mean.
            reply_ok(writer, id, json!({"protocolVersion": PROTOCOL_VERSION, "schemaVersion": UPDATE_SCHEMA_VERSION, "skills": skills})).await;
        }
        method @ ("session/new" | "session/load") => {
            let name = match msg.pointer("/params/name") {
                None | Some(Value::Null) => None,
                Some(v) => match v.as_str().and_then(hotl_types::normalize_session_name) {
                    Some(n) => Some(n),
                    None => {
                        return reply_err(
                            writer,
                            id,
                            "params.name must be 1–64 chars after trimming",
                        )
                        .await
                    }
                },
            };
            let spec = if method == "session/load" {
                match msg.pointer("/params/sessionId").and_then(Value::as_str) {
                    Some(sid) => SessionSpec::Load {
                        session_id: sid.to_string(),
                        name,
                    },
                    None => {
                        return reply_err(writer, id, "session/load requires params.sessionId")
                            .await
                    }
                }
            } else {
                SessionSpec::New { name }
            };
            match factory(spec) {
                Ok(open) => {
                    // Replacing a session: interrupt its in-flight turn (its
                    // events are about to stop rendering anywhere — it must
                    // not keep running tools invisibly in the shared cwd),
                    // stop its drain task, and drop its parked state — a
                    // dropped ask sender reads as a deny to the old engine,
                    // and a stale prompt id must never be answered with the
                    // new session's first TurnDone.
                    if let Some(old) = session.take() {
                        old.handle.interrupt();
                        old.drain.abort();
                        pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clear();
                        pending_prompt
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clear();
                    }
                    let state = start_session(
                        open.handle,
                        writer.clone(),
                        pending.clone(),
                        pending_prompt.clone(),
                        next_id,
                    );
                    // Resume auto-continuation (M4/#8): a loaded projection
                    // that ends mid-turn (user prompt or unanswered tool
                    // results) picks the work back up; the engine no-ops when
                    // there is nothing to continue.
                    if method == "session/load" {
                        state.handle.continue_turn().await;
                    }
                    let sid = state.id.clone();
                    *session = Some(state);
                    reply_ok(writer, id, json!({"sessionId": sid, "name": open.name})).await;
                }
                Err(e) => reply_err(writer, id, &e).await,
            }
        }
        "session/prompt" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(text) = msg.pointer("/params/text").and_then(Value::as_str) else {
                return reply_err(writer, id, "session/prompt requires params.text").await;
            };
            // Stash the id; the drain task answers it on TurnDone so the read
            // loop stays free to service permission responses meanwhile.
            pending_prompt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(id);
            state.handle.prompt(text.to_string()).await;
        }
        "session/rename" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(name) = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .and_then(hotl_types::normalize_session_name)
            else {
                return reply_err(
                    writer,
                    id,
                    "session/rename requires params.name (1–64 chars after trimming)",
                )
                .await;
            };
            state.handle.rename(name).await;
            reply_ok(writer, id, json!({"ok": true})).await;
        }
        "session/set_mode" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(mode) = msg
                .pointer("/params/mode")
                .and_then(Value::as_str)
                .and_then(hotl_tools::rules::PermissionMode::from_str)
            else {
                return reply_err(
                    writer,
                    id,
                    "session/set_mode requires params.mode (ask | auto | plan | dontask)",
                )
                .await;
            };
            state.handle.set_mode(mode).await;
            reply_ok(writer, id, json!({"ok": true})).await;
        }
        "session/steer" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(text) = msg.pointer("/params/text").and_then(Value::as_str) else {
                return reply_err(writer, id, "session/steer requires params.text").await;
            };
            state.handle.steer(text.to_string()).await;
            reply_ok(writer, id, json!({"queued": true})).await;
        }
        "session/cancel" => {
            if let Some(state) = session.as_ref() {
                state.handle.interrupt();
            }
            reply_ok(writer, id, json!({"cancelled": true})).await;
        }
        other => reply_err(writer, id, &format!("unknown method `{other}`")).await,
    }
}

fn start_session(
    mut handle: SessionHandle,
    writer: Writer,
    pending: Pending,
    pending_prompt: PendingPrompt,
    next_id: &mut u64,
) -> SessionState {
    let id = format!("acp-{}", *next_id);
    // Permission request ids for this session are disjoint from every other id.
    let req_id_seed = *next_id * 1_000_000;
    *next_id += 1;
    let events = std::mem::replace(&mut handle.events, mpsc::channel(1).1);
    let sid = id.clone();
    let drain = tokio::spawn(drain_events(
        events,
        writer,
        pending,
        pending_prompt,
        sid,
        req_id_seed,
    ));
    SessionState { id, handle, drain }
}

/// Map engine events to `session/update` notifications, turn permission asks
/// into `session/request_permission` requests, and answer the pending prompt
/// on TurnDone.
async fn drain_events(
    mut events: mpsc::Receiver<EngineEvent>,
    writer: Writer,
    pending: Pending,
    pending_prompt: PendingPrompt,
    session_id: String,
    mut req_id: u64,
) {
    while let Some(event) = events.recv().await {
        match event {
            EngineEvent::Ask {
                summary,
                protected_why,
                reply,
            } => {
                req_id += 1;
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(req_id, reply);
                send(&writer, &json!({
                    "jsonrpc": "2.0", "id": req_id, "method": "session/request_permission",
                    "params": {"sessionId": session_id, "summary": summary, "protectedWhy": protected_why},
                }))
                .await;
            }
            EngineEvent::TurnDone { outcome, usage } => {
                // A turn that ended without its asks being answered left dead
                // reply channels behind — drop them so they can't leak.
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, tx| !tx.is_closed());
                notify(
                    &writer,
                    &session_id,
                    json!({"type": "turn_done", "outcome": outcome_tag(&outcome)}),
                )
                .await;
                // Take the id and drop the guard *before* awaiting (a
                // std::sync guard held across .await would make this non-Send).
                let prompt_id = pending_prompt
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front();
                if let Some(id) = prompt_id {
                    reply_ok(
                        &writer,
                        id,
                        json!({"schemaVersion": UPDATE_SCHEMA_VERSION, "outcome": outcome_tag(&outcome), "usage": usage}),
                    )
                    .await;
                }
            }
            other => {
                if let Some(update) = update_payload(&other) {
                    notify(&writer, &session_id, update).await;
                }
            }
        }
    }
}

pub(crate) fn update_payload(event: &EngineEvent) -> Option<Value> {
    Some(match event {
        EngineEvent::TextDelta(t) => json!({"type": "text_delta", "text": t}),
        EngineEvent::ThinkingDelta(_) => json!({"type": "thinking_delta"}),
        EngineEvent::ToolStart { name, summary } => {
            json!({"type": "tool_start", "name": name, "summary": summary})
        }
        EngineEvent::ToolDone { name, ok } => json!({"type": "tool_done", "name": name, "ok": ok}),
        EngineEvent::ToolDenied { name } => json!({"type": "tool_denied", "name": name}),
        EngineEvent::ToolAutoAllowed { name, rule } => {
            json!({"type": "tool_auto_allowed", "name": name, "rule": rule})
        }
        EngineEvent::Retrying { attempt, reason } => {
            json!({"type": "retrying", "attempt": attempt, "reason": reason})
        }
        EngineEvent::FallbackModel { model } => json!({"type": "fallback_model", "model": model}),
        EngineEvent::PromptQueued => json!({"type": "prompt_queued"}),
        EngineEvent::Compacted { degraded } => json!({"type": "compacted", "degraded": degraded}),
        EngineEvent::Ask { .. } | EngineEvent::TurnDone { .. } => return None,
    })
}

pub(crate) fn outcome_tag(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::Done { text } => json!({"kind": "done", "text": text}),
        Outcome::Cancelled => json!({"kind": "cancelled"}),
        Outcome::TurnLimit => json!({"kind": "turn_limit"}),
        Outcome::Refused => json!({"kind": "refused"}),
        Outcome::DoomLoop { pattern } => json!({"kind": "doom_loop", "pattern": pattern}),
        Outcome::ToolFailureBudget { tool } => json!({"kind": "tool_failure_budget", "tool": tool}),
        Outcome::Error { message } => json!({"kind": "error", "message": message}),
    }
}

async fn notify(writer: &Writer, session_id: &str, update: Value) {
    send(writer, &json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": {"schemaVersion": UPDATE_SCHEMA_VERSION, "sessionId": session_id, "update": update},
    }))
    .await;
}

async fn reply_ok(writer: &Writer, id: Value, result: Value) {
    send(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
    .await;
}

async fn reply_err(writer: &Writer, id: Value, message: &str) {
    send(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": message}}),
    )
    .await;
}

async fn send(writer: &Writer, msg: &Value) {
    let mut line = msg.to_string();
    line.push('\n');
    let mut w = writer.lock().await;
    let _ = w.write_all(line.as_bytes()).await;
    let _ = w.flush().await;
}
