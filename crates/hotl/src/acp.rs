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

use futures_util::future::BoxFuture;
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
/// In-flight `session/request_question` round-trips, parallel to `Pending`.
/// Disjoint ids from `Pending`: both draw from the same per-session `req_id`
/// counter in `drain_events`, so an id never means two different things.
type PendingQuestions =
    Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<hotl_types::QuestionAnswer>>>>;
/// In-flight `session/request_egress` round-trips (plan 0026), parallel to
/// `Pending` and disjoint from it for the same reason: one `req_id` counter,
/// one meaning per id, but different reply types, so answering an egress
/// prompt through the tool-ask path could only mis-route.
type PendingEgress =
    Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<hotl_tools::net::EgressDecision>>>>;

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
        // Plan 0022: `{"allow":true,"secretReads":true}` also lifts the
        // credential read-deny, for this one command. A client that does not
        // send the field gets a plain `Allow` — the fail-closed direction.
        return match r.get("secretReads").and_then(Value::as_bool) {
            Some(true) => AskReply::AllowWithSecretReads,
            _ => AskReply::Allow,
        };
    }
    AskReply::Deny {
        message: r.get("message").and_then(Value::as_str).map(String::from),
    }
}

/// Map an ACP client's `session/request_egress` `result` (plan 0026) to an
/// `EgressDecision`: `{"allow": true}` or `{"allow": false}`.
///
/// Anything else — a missing result, a client that hung up, a non-boolean —
/// is `NoAnswer`, which refuses the connection exactly like `Deny` but is not
/// recorded as the human's decision, so the next connection asks again rather
/// than inheriting a denial nobody made.
fn egress_decision_from_result(result: Option<&Value>) -> hotl_tools::net::EgressDecision {
    use hotl_tools::net::EgressDecision;
    match result.and_then(|r| r.get("allow")).and_then(Value::as_bool) {
        Some(true) => EgressDecision::Allow,
        Some(false) => EgressDecision::Deny,
        None => EgressDecision::NoAnswer,
    }
}

/// Map an ACP client's `session/request_question` `result` to a
/// `QuestionAnswer`: `{"selected":["label", ...]}` or `{"freeText":"..."}`.
/// Anything else (including a missing/malformed result, or a client that
/// hangs up before replying) resolves to `NoHuman` — never a hang, and
/// never anything that could be mistaken for a permission grant.
fn question_answer_from_result(result: Option<&Value>) -> hotl_types::QuestionAnswer {
    let Some(r) = result else {
        return hotl_types::QuestionAnswer::NoHuman;
    };
    if let Some(text) = r.get("freeText").and_then(Value::as_str) {
        return hotl_types::QuestionAnswer::FreeText(text.to_string());
    }
    if let Some(arr) = r.get("selected").and_then(Value::as_array) {
        let labels: Vec<String> = arr
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        if !labels.is_empty() {
            return hotl_types::QuestionAnswer::Selected(labels);
        }
    }
    hotl_types::QuestionAnswer::NoHuman
}

/// JSON-RPC ids of in-flight prompt requests, answered in order on TurnDone
/// (the engine queues overlapping prompts and finishes them FIFO).
type PendingPrompt = Arc<std::sync::Mutex<VecDeque<Value>>>;

/// How much of a forked-from session's projection the fork keeps.
///
/// `Items` is the stored coordinate — the same one `BranchMove { keep_items }`
/// replays against (`items.truncate(n)`). `Turns` is a *resolver* on top of it,
/// never a second stored coordinate: raw item indices are unusable by hand
/// (compaction digests, tool-result batches and synthetic items all count),
/// but the log must record the coordinate replay understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepSpec {
    All,
    Items(usize),
    Turns(usize),
}

/// What a client asked the factory to produce.
pub enum SessionSpec {
    New {
        name: Option<String>,
    },
    Load {
        session_id: String,
        name: Option<String>,
    },
    /// A **new** session seeded with another's projection — optionally only a
    /// prefix of it — pinned to that session's fork-time tip. Unlike `Load`,
    /// the parent may still be live: the pin plus the fork's own `BranchMove`
    /// freeze the inherited history whatever the parent does next.
    Fork {
        session_id: String,
        keep: KeepSpec,
        name: Option<String>,
    },
}

/// A session the factory opened: the handle plus its display name (the one
/// just given, or inherited from the resumed chain).
pub struct SessionOpen {
    pub handle: SessionHandle,
    pub name: Option<String>,
    /// This session's effective permission mode at open (inherited-and-coerced
    /// on resume, else the configured default).
    /// INVARIANT: equals what the engine's `Rules` will actually enforce.
    /// Enforced by `tests/acp_protocol.rs::the_session_reports_its_effective_mode`.
    pub mode: String,
    /// Plan mode at open — the axis orthogonal to `mode`, inherited from the
    /// resumed chain (a `PlanSet`, or a legacy `mode: "plan"`) else the config
    /// default.
    pub plan: bool,
    /// The session's resolved starting effort (env > config > the 0030
    /// `xhigh` default on catalogued Anthropic), display-only: what a bare
    /// `/effort` reports when the user has set nothing. `None` = the model
    /// gets no depth field (uncatalogued model, nothing configured).
    pub default_effort: Option<String>,
    /// The **store** session id — what `hotl_store::replay_chain` resolves and
    /// what `SessionSpec::Load` takes, as distinct from the `acp-N` handle
    /// this connection hands clients. `session/reload_config` resumes through
    /// this; resuming through the handle id silently fails to replay.
    pub session_id: String,
}

/// Builds a session per the client's request. The real binary wires engine
/// deps here; tests inject a scripted-provider session.
pub type SessionFactory = Box<dyn FnMut(SessionSpec) -> Result<SessionOpen, String> + Send>;

/// Rebuilds the factory (and what `initialize` advertises) from a freshly-read
/// `config.toml`, plus any warnings that read produced. This is the whole of
/// `session/reload_config`'s engine knowledge: the protocol layer never learns
/// how a scaffold is built, only that one can be rebuilt.
///
/// `Send` because `hotl tui` `tokio::spawn`s `serve`.
pub type Reload = Box<
    dyn FnMut() -> BoxFuture<'static, Result<(SessionFactory, ServerInfo, Vec<String>), String>>
        + Send,
>;

/// One skill as `initialize` advertises it. The description is client-facing
/// only — front ends render it in the `/` completion menu. Content still
/// enters context only when asked for: the always-sent tool description
/// (`hotl_tools::skills::SkillTool`'s `describe`) omits it — the model sees
/// it only when it calls the skill tool itself.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// What `serve` advertises to a client before any session exists. Bundled
/// rather than passed positionally: `serve` is already at the argument count
/// where this workspace reaches for `#[allow(clippy::too_many_arguments)]`.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub skills: Vec<SkillInfo>,
    /// The configured permission mode a new session starts in, already run
    /// through `enforced_mode`. Clients render it; they never infer it.
    pub default_mode: String,
    /// Whether a new session starts with the plan overlay on.
    pub default_plan: bool,
    /// Model context window in tokens — what a UI's fullness gauge divides by.
    pub context_window: u64,
    /// The session's primary model — prices `turn_done`'s `usage.cost_usd`
    /// (Task 5 cost telemetry). One process serves one model for its whole
    /// lifetime (process-per-session), so this is fixed for every
    /// `session/new` and `session/load` this connection sees; a fallback
    /// model used mid-turn is still priced as this one (see
    /// `wire::usage_frame`).
    pub model: String,
}

/// Drive the protocol over one connection until the client hangs up.
///
/// `reload` grants the connection the ability to rebuild its engine from
/// `config.toml` mid-flight (`session/reload_config`). `None` is a connection
/// without that capability, and the method then answers with an explicit
/// error — never a silent no-op, which a client would read as a reload that
/// happened.
pub async fn serve(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
    mut factory: SessionFactory,
    mut info: ServerInfo,
    mut reload: Option<Reload>,
) {
    let writer: Writer = Arc::new(Mutex::new(Box::new(write)));
    let pending: Pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let pending_questions: PendingQuestions = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let pending_egress: PendingEgress = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let pending_prompt: PendingPrompt = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let mut next_id: u64 = 1;
    let mut session: Option<SessionState> = None;

    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // unparseable frame, no id to reply to
        };
        // A client response to one of our permission requests, or one of our
        // questions (disjoint id spaces — a hit in one map means a miss in
        // the other, so trying both is safe and order-independent)?
        if msg.get("method").is_none() {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some(reply) = pending_questions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = reply.send(question_answer_from_result(msg.get("result")));
                } else if let Some(reply) = pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = reply.send(egress_decision_from_result(msg.get("result")));
                } else if let Some(reply) = pending
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
            &pending_questions,
            &pending_egress,
            &pending_prompt,
            &mut next_id,
            &mut info,
            &mut reload,
        )
        .await;
    }
}

struct SessionState {
    id: String,
    /// The durable id this session's log carries — what a resume (and so
    /// `session/reload_config`) has to name. Not `id`, which is this
    /// connection's `acp-N` handle and means nothing to the store.
    store_id: String,
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
    pending_questions: &PendingQuestions,
    pending_egress: &PendingEgress,
    pending_prompt: &PendingPrompt,
    next_id: &mut u64,
    info: &mut ServerInfo,
    reload: &mut Option<Reload>,
) {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => {
            // `skills` lets a front end resolve `/<skill>` itself and build a
            // completion menu — the roster is server-side knowledge, so the
            // client never has to walk the config dirs to know what a slash
            // could mean. Descriptions ride this response only; they are
            // never part of any prompt.
            let skills: Vec<Value> = info
                .skills
                .iter()
                .map(|s| json!({"name": s.name, "description": s.description}))
                .collect();
            // `defaultMode` and `contextWindow` are server-side truth a client
            // renders rather than guesses: the badge used to read "ask" while
            // the shipped default ran "auto" (evaluation §5.7).
            reply_ok(
                writer,
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "schemaVersion": UPDATE_SCHEMA_VERSION,
                    "skills": skills,
                    "defaultMode": info.default_mode,
                    "defaultPlan": info.default_plan,
                    "contextWindow": info.context_window,
                }),
            )
            .await;
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
            let is_fork = msg
                .pointer("/params/fork")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let spec = if method == "session/load" {
                let keep = match keep_spec(msg) {
                    Ok(k) => k,
                    Err(e) => return reply_err(writer, id, &e).await,
                };
                if !is_fork && keep != KeepSpec::All {
                    return reply_err(
                        writer,
                        id,
                        "params.keepItems/keepTurns only apply to a fork (params.fork: true)",
                    )
                    .await;
                }
                match msg.pointer("/params/sessionId").and_then(Value::as_str) {
                    Some(sid) if is_fork => SessionSpec::Fork {
                        session_id: sid.to_string(),
                        keep,
                        name,
                    },
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
                    // Captured before `open.handle` moves into `install_session`.
                    let mode = open.mode.clone();
                    let plan = open.plan;
                    let default_effort = open.default_effort.clone();
                    let name = open.name.clone();
                    let sid = install_session(
                        open,
                        writer,
                        session,
                        pending,
                        pending_questions,
                        pending_egress,
                        pending_prompt,
                        next_id,
                        &info.model,
                    );
                    // Resume auto-continuation (M4/#8): a loaded projection
                    // that ends mid-turn (user prompt or unanswered tool
                    // results) picks the work back up; the engine no-ops when
                    // there is nothing to continue.
                    //
                    // A **fork** never does (D-A9). For resume, finishing the
                    // interrupted turn is what the user wants; for a fork the
                    // sense is inverted — they forked to send it somewhere
                    // else, and auto-continuing would spend a full-price
                    // sample re-answering the parent's stale prompt before the
                    // phase instruction ever arrives.
                    if method == "session/load" && !is_fork {
                        if let Some(state) = session.as_ref() {
                            state.handle.continue_turn().await;
                        }
                    }
                    // The session's own effective mode rides the open result:
                    // a client's handshake runs before it takes the screen and
                    // wants the seed synchronously, rather than waiting for a
                    // notification that only fires on a *change*.
                    // `images: true` is the client's feature detection: an
                    // old server simply never sends the key, so a client can
                    // tell "images accepted" from "images silently ignored".
                    reply_ok(
                        writer,
                        id,
                        json!({"sessionId": sid, "name": name, "mode": mode, "plan": plan,
                               "defaultEffort": default_effort, "images": true}),
                    )
                    .await;
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
            // Images are validated HERE, before the log: a poisoned base64
            // item would be durable and 400 every later sample (see
            // `crate::images`). A rejected prompt commits nothing.
            let images =
                match crate::images::parse_images(msg.pointer("/params").unwrap_or(&Value::Null)) {
                    Ok(v) => v,
                    Err(e) => return reply_err(writer, id, &format!("session/prompt: {e}")).await,
                };
            // Stash the id; the drain task answers it on TurnDone so the read
            // loop stays free to service permission responses meanwhile.
            pending_prompt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(id);
            state.handle.prompt_with(text.to_string(), images).await;
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
            let raw = msg.pointer("/params/mode").and_then(Value::as_str);
            // `"plan"` was a mode before it became an overlay. A client still
            // sending it means "turn plan on", and must not be told its mode
            // word was a typo.
            if raw.is_some_and(hotl_tools::rules::is_legacy_plan_word) {
                state.handle.set_plan(true).await;
                let session_id = state.id.clone();
                reply_ok(writer, id, json!({"ok": true, "plan": true})).await;
                return notify(
                    writer,
                    &session_id,
                    json!({"type": "plan_changed", "plan": true}),
                )
                .await;
            }
            let Some(mode) = raw.and_then(hotl_tools::rules::PermissionMode::from_str) else {
                return reply_err(
                    writer,
                    id,
                    "session/set_mode requires params.mode (ask | bypass | dontask)",
                )
                .await;
            };
            // The *effective* mode, post-coercion: a `security-enforced` build
            // forces Bypass→Ask inside `Rules::with_mode`, so acking the
            // requested mode would leave a client's badge reading `bypass` for
            // a session actually running `ask` — the §5.7 lie in reverse.
            let effective = hotl_tools::rules::enforced_mode(mode);
            state.handle.set_mode(mode).await;
            let session_id = state.id.clone();
            reply_ok(writer, id, json!({"ok": true, "mode": effective.as_str()})).await;
            // Broadcast, not just ack: a mode changed by *any* client (or by
            // resume inheritance) has to reach every attached surface, and
            // `session/update` needs no id-plumbing in a client's loop.
            notify(
                writer,
                &session_id,
                json!({"type": "mode_changed", "mode": effective.as_str()}),
            )
            .await;
        }
        "session/set_plan" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(plan) = msg.pointer("/params/plan").and_then(Value::as_bool) else {
                return reply_err(
                    writer,
                    id,
                    "session/set_plan requires params.plan (a boolean)",
                )
                .await;
            };
            // No `enforced_mode` counterpart: the overlay only ever adds an
            // ask, so what is requested is always what takes effect.
            state.handle.set_plan(plan).await;
            let session_id = state.id.clone();
            reply_ok(writer, id, json!({"ok": true, "plan": plan})).await;
            notify(
                writer,
                &session_id,
                json!({"type": "plan_changed", "plan": plan}),
            )
            .await;
        }
        "session/set_effort" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(raw) = msg.pointer("/params/effort").and_then(Value::as_str) else {
                return reply_err(
                    writer,
                    id,
                    "session/set_effort requires params.effort \
                     (low | medium | high | xhigh | max | default)",
                )
                .await;
            };
            // Four spellings of "back to the provider's own default", because
            // a client should not have to guess which one this server took.
            let effort = match raw.trim() {
                "" | "default" | "unset" | "none" => None,
                other => match other.parse::<hotl_provider::Effort>() {
                    Ok(e) => Some(e),
                    Err(_) => {
                        return reply_err(
                            writer,
                            id,
                            "session/set_effort requires params.effort \
                             (low | medium | high | xhigh | max | default)",
                        )
                        .await;
                    }
                },
            };
            // No `enforced_mode` counterpart: no build tightens effort.
            state.handle.set_effort(effort).await;
            let session_id = state.id.clone();
            let wire = effort.map(|e| e.as_str());
            reply_ok(writer, id, json!({"ok": true, "effort": wire})).await;
            // Broadcast for the same reason as `mode_changed`: a change made
            // by any client must reach every attached surface.
            notify(
                writer,
                &session_id,
                json!({"type": "effort_changed", "effort": wire}),
            )
            .await;
        }
        "session/steer" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "no session — call session/new first").await;
            };
            let Some(text) = msg.pointer("/params/text").and_then(Value::as_str) else {
                return reply_err(writer, id, "session/steer requires params.text").await;
            };
            let images =
                match crate::images::parse_images(msg.pointer("/params").unwrap_or(&Value::Null)) {
                    Ok(v) => v,
                    Err(e) => return reply_err(writer, id, &format!("session/steer: {e}")).await,
                };
            state.handle.steer_with(text.to_string(), images).await;
            reply_ok(writer, id, json!({"queued": true})).await;
        }
        "session/cancel" => {
            if let Some(state) = session.as_ref() {
                state.handle.interrupt();
            }
            reply_ok(writer, id, json!({"cancelled": true})).await;
        }
        // Re-read `config.toml` and rebuild the engine behind this connection,
        // then re-open the live session through the ordinary resume path so its
        // items, todos, name and mode carry forward. Everything a scaffold
        // owns reloads: model, `[[allow]]`, `[[mcp]]`, `[[hook]]`, skills, the
        // system prompt, `[context]`. What cannot is process-wide set-once
        // state — `[sandbox]` extras, `[network]` egress, the thread pools —
        // which the client is told about rather than left to guess at.
        "session/reload_config" => {
            let Some(hook) = reload.as_mut() else {
                return reply_err(
                    writer,
                    id,
                    "session/reload_config is not supported on this connection",
                )
                .await;
            };
            // A `config.toml` with a typo must never take the live session
            // down: on failure the running engine is left exactly as it was.
            let (new_factory, new_info, warnings) = match hook().await {
                Ok(triple) => triple,
                Err(e) => {
                    if let Some(state) = session.as_ref() {
                        let sid = state.id.clone();
                        notify(
                            writer,
                            &sid,
                            json!({"type": "config_reload_failed", "reason": e}),
                        )
                        .await;
                    }
                    return reply_err(writer, id, &format!("config reload failed: {e}")).await;
                }
            };
            *factory = new_factory;
            *info = new_info;
            // No live session: the new factory is in place and the next
            // `session/new` gets the reloaded engine. Nothing to re-open.
            let Some(store_id) = session.as_ref().map(|s| s.store_id.clone()) else {
                return reply_ok(
                    writer,
                    id,
                    json!({"ok": true, "model": info.model, "warnings": warnings}),
                )
                .await;
            };
            // The **store** id, not this connection's `acp-N` handle — the
            // factory replays a log chain, and `acp-1` names no log.
            //
            // `name: None` so the replay inherits the session's own name, and
            // `SessionSpec::Load` so it inherits its own mode: a logged
            // `/mode` outlives a `[permissions] mode` edit, exactly as across
            // `hotl resume`. A session that never set one has nothing to
            // inherit and takes the reloaded default — which the ack and the
            // broadcast both report, so no client has to infer it.
            let spec = SessionSpec::Load {
                session_id: store_id,
                name: None,
            };
            match factory(spec) {
                Ok(open) => {
                    let mode = open.mode.clone();
                    let plan = open.plan;
                    let sid = install_session(
                        open,
                        writer,
                        session,
                        pending,
                        pending_questions,
                        pending_egress,
                        pending_prompt,
                        next_id,
                        &info.model,
                    );
                    let skills: Vec<Value> = info
                        .skills
                        .iter()
                        .map(|s| json!({"name": s.name, "description": s.description}))
                        .collect();
                    reply_ok(
                        writer,
                        id,
                        json!({
                            "ok": true,
                            "sessionId": sid,
                            "model": info.model,
                            "mode": mode,
                            "plan": plan,
                            "contextWindow": info.context_window,
                            "warnings": warnings,
                        }),
                    )
                    .await;
                    // Broadcast, not just ack — same reasoning as
                    // `mode_changed`: every attached surface needs the new
                    // roster, and a client's loop needs no id-plumbing to get
                    // it. Additive, so `UPDATE_SCHEMA_VERSION` stands.
                    notify(
                        writer,
                        &sid,
                        json!({
                            "type": "config_reloaded",
                            "model": info.model,
                            "mode": mode,
                            "plan": plan,
                            "context_window": info.context_window,
                            "skills": skills,
                            "warnings": warnings,
                        }),
                    )
                    .await;
                }
                // The factory swapped but re-opening failed: say so loudly.
                // The connection has no session now, so `session/new` is the
                // way back — and it will use the reloaded engine.
                Err(e) => {
                    reply_err(
                        writer,
                        id,
                        &format!("config reloaded but session re-open failed: {e}"),
                    )
                    .await
                }
            }
        }
        // What currently fills the context window, by source. A read: it
        // appends nothing and publishes nothing, so unlike `reload_config`
        // there is no idle guard and no session rebuild.
        "session/context" => {
            let Some(state) = session.as_ref() else {
                return reply_err(writer, id, "session/context requires an open session").await;
            };
            let sid = state.id.clone();
            match state.handle.context_breakdown().await {
                Some(b) => {
                    // Thin ack, payload broadcast — the `config_reloaded`
                    // shape, for the same reason: a client's loop needs no
                    // id-plumbing to get it. Additive, so
                    // `UPDATE_SCHEMA_VERSION` stands.
                    reply_ok(writer, id, json!({"ok": true})).await;
                    notify(
                        writer,
                        &sid,
                        json!({
                            "type": "context_report",
                            "window": b.window,
                            "rows": b.rows,
                        }),
                    )
                    .await;
                }
                None => reply_err(writer, id, "session/context: the session is gone").await,
            }
        }
        other => reply_err(writer, id, &format!("unknown method `{other}`")).await,
    }
}

/// `keepItems` / `keepTurns` off a `session/load` request. Mutually exclusive:
/// they are two coordinate systems onto the same cut, and a request naming both
/// has no single meaning worth guessing at.
fn keep_spec(msg: &Value) -> Result<KeepSpec, String> {
    let read = |key: &str| -> Result<Option<usize>, String> {
        match msg.pointer(&format!("/params/{key}")) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .map(|n| Some(n as usize))
                .ok_or_else(|| format!("params.{key} must be a non-negative integer")),
        }
    };
    match (read("keepItems")?, read("keepTurns")?) {
        (Some(_), Some(_)) => {
            Err("params.keepItems and params.keepTurns are mutually exclusive".to_string())
        }
        (Some(n), None) => Ok(KeepSpec::Items(n)),
        (None, Some(t)) => Ok(KeepSpec::Turns(t)),
        (None, None) => Ok(KeepSpec::All),
    }
}

/// Make `open` the connection's session, retiring whatever was there, and
/// return the new session id.
///
/// Replacing a session: interrupt its in-flight turn (its events are about to
/// stop rendering anywhere — it must not keep running tools invisibly in the
/// shared cwd), stop its drain task, and drop its parked state — a dropped ask
/// sender reads as a deny to the old engine, and a stale prompt id must never
/// be answered with the new session's first TurnDone.
///
/// Shared by `session/new`, `session/load` and `session/reload_config` so the
/// three can never drift on what "retire the old session" means.
#[allow(clippy::too_many_arguments)]
fn install_session(
    open: SessionOpen,
    writer: &Writer,
    session: &mut Option<SessionState>,
    pending: &Pending,
    pending_questions: &PendingQuestions,
    pending_egress: &PendingEgress,
    pending_prompt: &PendingPrompt,
    next_id: &mut u64,
    model: &str,
) -> String {
    if let Some(old) = session.take() {
        old.handle.interrupt();
        old.drain.abort();
        pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        pending_questions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        pending_egress
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
        open.session_id,
        writer.clone(),
        pending.clone(),
        pending_questions.clone(),
        pending_egress.clone(),
        pending_prompt.clone(),
        next_id,
        model.to_string(),
    );
    let sid = state.id.clone();
    *session = Some(state);
    sid
}

#[allow(clippy::too_many_arguments)]
fn start_session(
    mut handle: SessionHandle,
    store_id: String,
    writer: Writer,
    pending: Pending,
    pending_questions: PendingQuestions,
    pending_egress: PendingEgress,
    pending_prompt: PendingPrompt,
    next_id: &mut u64,
    model: String,
) -> SessionState {
    let id = format!("acp-{}", *next_id);
    // Permission/question request ids for this session are disjoint from
    // every other id.
    let req_id_seed = *next_id * 1_000_000;
    *next_id += 1;
    let events = std::mem::replace(&mut handle.events, mpsc::channel(1).1);
    let sid = id.clone();
    let drain = tokio::spawn(drain_events(
        events,
        writer,
        pending,
        pending_questions,
        pending_egress,
        pending_prompt,
        sid,
        req_id_seed,
        model,
    ));
    SessionState {
        id,
        store_id,
        handle,
        drain,
    }
}

/// Map engine events to `session/update` notifications, turn permission asks
/// into `session/request_permission` requests, questions into
/// `session/request_question` requests, and answer the pending prompt on
/// TurnDone.
#[allow(clippy::too_many_arguments)]
async fn drain_events(
    mut events: mpsc::Receiver<EngineEvent>,
    writer: Writer,
    pending: Pending,
    pending_questions: PendingQuestions,
    pending_egress: PendingEgress,
    pending_prompt: PendingPrompt,
    session_id: String,
    mut req_id: u64,
    model: String,
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
                // INVARIANT (unimplemented — see
                // specs/exec-plans/active/0020-remediation-surface.md RQ-2):
                // an `edit`/`write` ask also carries `"diff"`, the proposed
                // change, so the human approving a write can see it. The
                // generator (`crate::diffgen::for_tool`) and the client's
                // renderer are both built and tested; this call site cannot
                // use them because `EngineEvent::Ask` carries no `tool`/
                // `input`. Once it does, this becomes:
                //     let diff = crate::diffgen::for_tool(tool, input);
                // plus `"diff": diff.map(|d| d.iter().map(DiffLine::to_json)…)`
                // in the params below.
                send(&writer, &json!({
                    "jsonrpc": "2.0", "id": req_id, "method": "session/request_permission",
                    "params": {"sessionId": session_id, "summary": summary, "protectedWhy": protected_why},
                }))
                .await;
            }
            EngineEvent::Question {
                question, reply, ..
            } => {
                // NOT a permission gate: the reply is plain text the model
                // reads, never an authorization (SECURITY invariant).
                req_id += 1;
                pending_questions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(req_id, reply);
                send(
                    &writer,
                    &json!({
                        "jsonrpc": "2.0", "id": req_id, "method": "session/request_question",
                        "params": {
                            "sessionId": session_id,
                            "header": question.header,
                            "prompt": question.prompt,
                            "options": question.options,
                            "multi": question.multi,
                        },
                    }),
                )
                .await;
            }
            // Plan 0026. Both surfaces already carried a two-way allow/deny
            // reply and already defaulted to deny, so excluding ACP would only
            // mean the eventual egress default flip silently breaks every
            // editor integration with no in-protocol recourse.
            EngineEvent::EgressAsk { host, reply } => {
                req_id += 1;
                pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(req_id, reply);
                send(
                    &writer,
                    &json!({
                        "jsonrpc": "2.0", "id": req_id, "method": "session/request_egress",
                        "params": {"sessionId": session_id, "host": host},
                    }),
                )
                .await;
            }
            EngineEvent::TurnDone { outcome, usage } => {
                // A turn that ended without its asks/questions being answered
                // left dead reply channels behind — drop them so they can't
                // leak.
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, tx| !tx.is_closed());
                pending_questions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, tx| !tx.is_closed());
                pending_egress
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
                        json!({
                            "schemaVersion": UPDATE_SCHEMA_VERSION,
                            "outcome": outcome_tag(&outcome),
                            "usage": crate::wire::usage_frame(&model, &usage),
                        }),
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

/// The `session/update` payload for an event, or `None` when the event is not
/// a stream frame. Thin alias over [`crate::wire::update_frame`] — there used
/// to be three copies of this mapping and they had already drifted (§7); one
/// renderer means a new `EngineEvent` variant cannot reach one surface and
/// silently miss another.
pub(crate) fn update_payload(event: &EngineEvent) -> Option<Value> {
    crate::wire::update_frame(event)
}

pub(crate) fn outcome_tag(outcome: &Outcome) -> Value {
    crate::wire::outcome_frame(outcome)
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

#[cfg(test)]
mod ask_reply_tests {
    use super::*;

    /// Plan 0022: the credential-read grant rides the same permission result
    /// the TUI already sends. A client that never heard of it gets `Allow`,
    /// which is the fail-closed direction.
    /// Plan 0026: two answers, and everything else refuses. An absent result
    /// — a client that hung up, or one that does not know this method — is
    /// `NoAnswer`: it denies the connection, but is not recorded as a human's
    /// decision, so the next connection asks again.
    #[test]
    fn acp_egress_maps_two_answers_and_denies_everything_else() {
        use hotl_tools::net::EgressDecision;
        assert_eq!(
            egress_decision_from_result(Some(&json!({"allow": true}))),
            EgressDecision::Allow
        );
        assert_eq!(
            egress_decision_from_result(Some(&json!({"allow": false}))),
            EgressDecision::Deny
        );
        assert_eq!(
            egress_decision_from_result(None),
            EgressDecision::NoAnswer,
            "a client that never answered is not a client that said no"
        );
        assert_eq!(
            egress_decision_from_result(Some(&json!({}))),
            EgressDecision::NoAnswer
        );
        assert_eq!(
            egress_decision_from_result(Some(&json!({"allow": "yes"}))),
            EgressDecision::NoAnswer,
            "a non-boolean is malformed, never a grant"
        );
    }

    #[test]
    fn secret_reads_is_opt_in_and_only_on_an_allow() {
        let reply = |v: Value| ask_reply_from_result(Some(&v));
        assert_eq!(
            reply(serde_json::json!({"allow": true, "secretReads": true})),
            AskReply::AllowWithSecretReads
        );
        for result in [
            serde_json::json!({"allow": true}),
            serde_json::json!({"allow": true, "secretReads": false}),
            // A non-boolean is not a grant.
            serde_json::json!({"allow": true, "secretReads": "yes"}),
        ] {
            assert_eq!(reply(result.clone()), AskReply::Allow, "{result}");
        }
        // The flag cannot smuggle an approval past a denial.
        assert!(matches!(
            reply(serde_json::json!({"allow": false, "secretReads": true})),
            AskReply::Deny { .. }
        ));
        assert!(matches!(ask_reply_from_result(None), AskReply::Deny { .. }));
    }
}
