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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hotl_engine::{EngineEvent, SessionHandle};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

use hotl_platform::{Ipc as _, IpcListener as _};

/// The session transport, bound in one place: a unix socket on Unix, a named
/// pipe on Windows. `tokio::io::split` rather than a transport-specific
/// `into_split`, because it is the one spelling both have.
type Stream = <hotl_platform::ActiveIpc as hotl_platform::Ipc>::Stream;
type Listener = <hotl_platform::ActiveIpc as hotl_platform::Ipc>::Listener;
type ReadHalf = tokio::io::ReadHalf<Stream>;
type WriteHalf = tokio::io::WriteHalf<Stream>;

use crate::acp::{outcome_tag, update_payload, UPDATE_SCHEMA_VERSION};

type ClientWriter = AsyncMutex<Option<WriteHalf>>;

/// What a surface prints after a human allows a host: the grant is
/// session-scoped, and hotl does not write `config.toml` for you.
///
/// A permanent, global, cross-session egress grant reachable by one keypress
/// would invert the threat model — the durable decision would get *less*
/// friction than the temporary one. So the prompt prints the line to paste
/// instead (plan 0026 decision 9).
pub fn paste_hint(host: &str) -> String {
    format!(
        "  allowed \"{host}\" for this session\n  \
         to make it permanent, add to your config.toml:\n      \
         [network]\n      allow = [\"{host}\"]"
    )
}

struct Shared {
    handle: SessionHandle,
    client: ClientWriter,
    /// Parked permission asks: id → (reply channel, the request frame to
    /// re-send, when it was parked). The instant is what `ask_expiry` reads.
    pending: Mutex<HashMap<u64, Parked<hotl_engine::AskReply>>>,
    /// Parked `ask_user` questions (0058 T9), same shape as `pending`. These
    /// used to fall through the catch-all and resolve `NoHuman` the moment
    /// the sender dropped — a question asked while nobody was attached was
    /// answered by nobody and never re-issued.
    ///
    /// **Deliberately not swept by `ask_expiry`** (0059 T5), which denies a
    /// parked *permission* ask after an hour. A question authorizes nothing,
    /// so leaving one parked risks nothing — and the case that would break is
    /// exactly the one worth protecting: a `human_input` workflow step is a
    /// recipe deliberately waiting for a person, with its own `timeout_secs`
    /// chosen by whoever wrote it. A global hour would silently override that.
    pending_question: Mutex<HashMap<u64, Parked<hotl_types::QuestionAnswer>>>,
    /// Parked egress asks (plan 0026), same shape as `pending`.
    ///
    /// A separate map, sharing `next_ask`: the two carry different reply types
    /// (`AskReply` vs `EgressDecision`), so one map could not hold both, and
    /// the shared counter is what keeps an id from ever meaning two different
    /// things — the same arrangement `acp.rs`'s `PendingQuestions` documents.
    pending_egress: Mutex<HashMap<u64, Parked<hotl_tools::net::EgressDecision>>>,
    next_ask: AtomicU64,
    session_id: String,
    /// Per-session secret a client must present before it can drive the session
    /// or evict the attached human (Vuln 1).
    token: String,
    /// The session's primary model — prices `turn_done.usage.cost_usd`
    /// (Task 5). See `wire::usage_frame` for the fallback-model imprecision
    /// this accepts.
    model: String,
    /// How long a parked ask waits for a human before it is denied (0059 T5).
    /// `Duration::ZERO` disables expiry.
    ask_expiry: std::time::Duration,
}

/// One parked ask: its reply channel, the frame to re-send on attach, and
/// when it was parked.
type Parked<T> = (tokio::sync::oneshot::Sender<T>, Value, std::time::Instant);

/// The coarsest the expiry sweep ever runs. An hour-scale deadline does not
/// need a fine-grained sweep, and the loop must stay cheap while a session
/// sits idle — but a deadline shorter than this gets a sweep to match, so an
/// expiry is never rounded up to five seconds.
const ASK_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The tick for `ask_expiry`, never longer than the deadline itself and never
/// zero (a zero-period interval would spin).
fn sweep_period(ask_expiry: std::time::Duration) -> std::time::Duration {
    if ask_expiry.is_zero() {
        return ASK_SWEEP_INTERVAL;
    }
    ask_expiry
        .min(ASK_SWEEP_INTERVAL)
        .max(std::time::Duration::from_millis(50))
}

/// Deny every parked ask older than `shared.ask_expiry`. Denying is what
/// closes the loop: the engine's `ask` is still awaiting this channel, so the
/// turn resumes and commits its own `AskResolved`. Returns how many expired,
/// for the test and for the notice.
fn sweep_expired_asks(shared: &Shared) -> usize {
    if shared.ask_expiry.is_zero() {
        return 0;
    }
    let secs = shared.ask_expiry.as_secs();
    let message = format!("expired after {secs} s with no human");
    let now = std::time::Instant::now();
    let expired = |at: &std::time::Instant| now.duration_since(*at) >= shared.ask_expiry;
    let mut n = 0;
    {
        let mut pending = shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<u64> = pending
            .iter()
            .filter(|(_, (_, _, at))| expired(at))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some((tx, _, _)) = pending.remove(&id) {
                let _ = tx.send(hotl_engine::AskReply::Deny {
                    message: Some(message.clone()),
                });
                n += 1;
            }
        }
    }
    let mut egress = shared
        .pending_egress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ids: Vec<u64> = egress
        .iter()
        .filter(|(_, (_, _, at))| expired(at))
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        if let Some((tx, _, _)) = egress.remove(&id) {
            let _ = tx.send(hotl_tools::net::EgressDecision::Deny);
            n += 1;
        }
    }
    n
}

/// Directory holding one `<id>.sock` per live backgrounded session.
pub fn run_dir() -> PathBuf {
    crate::agent::sessions_dir()
        .parent()
        .map(|p| p.join("run"))
        .unwrap_or_else(|| PathBuf::from("run"))
}

/// The per-session auth-token file, next to the socket. Written 0600 by the
/// server; read by `hotl attach` to authenticate.
pub fn token_path(id: &str) -> PathBuf {
    run_dir().join(format!("{id}.token"))
}

/// 256-bit hex secret from the OS CSPRNG. A failure fails the serve rather
/// than minting a weak token — `Entropy` forbids a PRNG fallback for exactly
/// this consumer.
fn mint_token() -> std::io::Result<String> {
    use hotl_platform::Entropy as _;
    let buf: [u8; 32] = hotl_platform::ENTROPY.token_bytes()?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Length-checked constant-time compare (token length is fixed and public).
fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A connecting peer must be the same uid (defence in depth behind the 0600
/// socket) and present the session token as its first frame. Returns the reader
/// half to promote, or None (rejected — the caller leaves the incumbent alone).
async fn authenticate(
    stream: Stream,
    token: &str,
) -> Option<(tokio::io::Lines<BufReader<ReadHalf>>, WriteHalf)> {
    if let Err(why) = hotl_platform::IPC.authenticate_peer(&stream) {
        // Defence in depth behind the endpoint's own access control, which is
        // the actual boundary — a `0600` socket mode or the pipe's DACL.
        let _ = why;
        return None;
    }
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    // Bound the handshake so a silent connect cannot stall the accept loop.
    let ok = matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line()).await,
        Ok(Ok(Some(line))) if auth_frame_ok(&line, token)
    );
    if !ok {
        let _ = write
            .write_all(b"{\"t\":\"error\",\"message\":\"unauthorized\"}\n")
            .await;
        return None;
    }
    Some((lines, write))
}

fn auth_frame_ok(line: &str, token: &str) -> bool {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    msg.get("t").and_then(Value::as_str) == Some("auth")
        && msg
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|t| tokens_match(t, token))
}

/// Live backgrounded sessions, by id.
pub fn list_live() -> Vec<String> {
    let mut ids = hotl_platform::IPC.list_live();
    ids.sort();
    ids
}

/// Run a detached session bound to `run_dir/<session_id>.sock`. `handle` is a
/// freshly spawned engine session; `prompt` is an optional opening prompt;
/// `model` is the session's primary model (Task 5 cost telemetry).
pub async fn serve(
    session_id: String,
    model: String,
    handle: SessionHandle,
    prompt: Option<String>,
    ask_expiry: std::time::Duration,
) -> i32 {
    // A *live* endpoint means this id collides with a running session (pid
    // reuse, a repeated --id) — refuse rather than silently steal it. A dead
    // one is cleared by `bind_private`.
    if hotl_platform::IPC.liveness(&session_id) == hotl_platform::Liveness::Live {
        eprintln!("hotl serve: session `{session_id}` is already running");
        return 1;
    }
    let listener = match hotl_platform::IPC.bind_private(&session_id) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("hotl serve: cannot bind session `{session_id}`: {e}");
            return 1;
        }
    };
    let token = match mint_token().and_then(|t| write_token(&session_id, &t).map(|_| t)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("hotl serve: cannot write the session token: {e}");
            return 1;
        }
    };
    let _guard = EndpointGuard::new(&session_id);
    let _token_guard = TokenGuard(token_path(&session_id));
    serve_on(
        listener, session_id, model, handle, prompt, token, ask_expiry,
    )
    .await;
    0
}

/// Removes the token file when the server exits.
struct TokenGuard(PathBuf);
impl Drop for TokenGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_token(id: &str, token: &str) -> std::io::Result<()> {
    use hotl_platform::PrivateFs as _;
    use std::io::Write;
    let path = token_path(id);
    if let Some(dir) = path.parent() {
        hotl_platform::PRIVATE_FS.create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&path); // clear any stale token first
    hotl_platform::PRIVATE_FS
        .create_file_new(&path, hotl_platform::Writes::FromStart)?
        .write_all(token.as_bytes())
}

/// The socket-server core over a pre-bound listener (testable without the
/// filesystem run-dir): spawn the drain, submit the opening prompt, then serve
/// clients one at a time — the session lives across attach/detach.
pub async fn serve_on(
    listener: Listener,
    session_id: String,
    model: String,
    mut handle: SessionHandle,
    prompt: Option<String>,
    token: String,
    ask_expiry: std::time::Duration,
) {
    let events = std::mem::replace(&mut handle.events, tokio::sync::mpsc::channel(1).1);
    let shared = Arc::new(Shared {
        handle,
        client: AsyncMutex::new(None),
        pending: Mutex::new(HashMap::new()),
        pending_question: Mutex::new(HashMap::new()),
        pending_egress: Mutex::new(HashMap::new()),
        next_ask: AtomicU64::new(1),
        session_id,
        token,
        model,
        ask_expiry,
    });
    tokio::spawn(drain_events(events, shared.clone()));
    if let Some(p) = prompt {
        shared.handle.prompt(p).await;
    }
    accept_loop(listener, shared).await;
}

/// Removes the endpoint's on-disk artifact when the server exits — but only
/// if the path still refers to the object *this* server bound (matched by
/// identity), so a stale guard can never delete a successor's live socket.
///
/// A **documented no-op** where the platform leaves no artifact: a named pipe
/// is a kernel object that vanishes with its last handle, which is strictly
/// better than the Unix behavior. `Ipc::LEAVES_STALE_ARTIFACT` is what says so,
/// rather than dead machinery ported for symmetry.
struct EndpointGuard {
    path: Option<PathBuf>,
    id: Option<hotl_platform::NodeId>,
}

impl EndpointGuard {
    fn new(session_id: &str) -> Self {
        let path = hotl_platform::IPC.artifact_path(session_id);
        let id = path
            .as_deref()
            .and_then(|p| hotl_platform::openat::identity_at(p).ok());
        Self { path, id }
    }
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        let Some(path) = &self.path else { return };
        let current = hotl_platform::openat::identity_at(path).ok();
        if self.id.is_none() || current == self.id {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// What one client frame asks of the server.
enum ClientAction {
    Continue,
    Detach,
    Shutdown,
}

/// Accepting stays live while a client is attached — a second `hotl attach`
/// takes over (the previous client is told and dropped) instead of hanging
/// unread in the listener backlog.
async fn accept_loop(mut listener: Listener, shared: Arc<Shared>) {
    let mut reader: Option<tokio::io::Lines<BufReader<ReadHalf>>> = None;
    let mut sweep = tokio::time::interval(sweep_period(shared.ask_expiry));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // A parked ask nobody ever answers holds a turn open forever
            // (0059 T5). Denying it is the fail-closed answer, and the engine
            // commits the `AskResolved` when its await returns.
            _ = sweep.tick() => {
                let n = sweep_expired_asks(&shared);
                if n > 0 {
                    send(
                        &shared,
                        &json!({"t": "error", "message": format!(
                            "{n} parked ask(s) expired after {} s with no human and were denied",
                            shared.ask_expiry.as_secs()
                        )}),
                    )
                    .await;
                }
            }
            accepted = listener.accept() => {
                let Ok(stream) = accepted else {
                    // A persistent accept failure (fd exhaustion) must not
                    // busy-spin the core; back off briefly and retry.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                };
                // Authenticate before touching the incumbent (Vuln 1): an
                // unauthenticated connect can neither drive the session nor
                // evict the attached human.
                let Some((new_reader, write)) = authenticate(stream, &shared.token).await else {
                    continue;
                };
                if reader.is_some() {
                    send(
                        &shared,
                        &json!({"t": "detached", "reason": "another client attached"}),
                    )
                    .await;
                }
                *shared.client.lock().await = Some(write);
                reader = Some(new_reader);
                resend_pending(&shared).await;
            }
            // Lines::next_line is cancel-safe: a frame half-read when the
            // accept arm wins stays buffered.
            line = next_line(&mut reader), if reader.is_some() => {
                match line {
                    Some(line) => match handle_frame(&line, &shared).await {
                        ClientAction::Continue => {}
                        ClientAction::Detach => {
                            reader = None;
                            *shared.client.lock().await = None;
                        }
                        ClientAction::Shutdown => break,
                    },
                    None => { // EOF or read error = detach
                        reader = None;
                        *shared.client.lock().await = None;
                    }
                }
            }
        }
    }
}

async fn next_line(reader: &mut Option<tokio::io::Lines<BufReader<ReadHalf>>>) -> Option<String> {
    match reader {
        Some(lines) => lines.next_line().await.ok().flatten(),
        // Unreachable behind the select guard; never resolve regardless.
        None => std::future::pending().await,
    }
}

/// Apply one client frame to the session.
async fn handle_frame(line: &str, shared: &Arc<Shared>) -> ClientAction {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return ClientAction::Continue;
    };
    match msg.get("t").and_then(Value::as_str).unwrap_or("") {
        // Attach-protocol parity: images ride at the frame's top level, same
        // {media_type, data} objects and the same validation as ACP. A frame
        // that fails validation is dropped whole — committing its text while
        // discarding its attachments would silently change what the human
        // said — and the client is told why.
        "prompt" => match crate::images::parse_images(&msg) {
            Ok(images) => {
                shared
                    .handle
                    .prompt_with(str_field(&msg, "text"), images)
                    .await
            }
            Err(e) => {
                send(
                    shared,
                    &json!({"t": "error", "message": format!("prompt: {e}")}),
                )
                .await
            }
        },
        "steer" => match crate::images::parse_images(&msg) {
            Ok(images) => {
                shared
                    .handle
                    .steer_with(str_field(&msg, "text"), images)
                    .await
            }
            Err(e) => {
                send(
                    shared,
                    &json!({"t": "error", "message": format!("steer: {e}")}),
                )
                .await
            }
        },
        "continue" => shared.handle.continue_turn().await,
        "cancel" => shared.handle.interrupt(),
        "ask_reply" => {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some((reply, _, _)) = shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
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
        "question_reply" => {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some((reply, ..)) = shared
                    .pending_question
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    // An option label, free text, or neither — a malformed
                    // answer is `NoHuman`, never a made-up selection.
                    let ans = match (
                        msg.get("option").and_then(Value::as_str),
                        msg.get("text").and_then(Value::as_str),
                    ) {
                        (Some(o), _) => hotl_types::QuestionAnswer::Selected(vec![o.to_string()]),
                        (None, Some(t)) => hotl_types::QuestionAnswer::FreeText(t.to_string()),
                        (None, None) => hotl_types::QuestionAnswer::NoHuman,
                    };
                    let _ = reply.send(ans);
                }
            }
        }
        "egress_reply" => {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some((reply, _, _)) = shared
                    .pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    // An absent or non-boolean `allow` denies, exactly as
                    // `ask_reply` above: a malformed answer is not a grant.
                    let allow = msg.get("allow").and_then(Value::as_bool).unwrap_or(false);
                    let _ = reply.send(if allow {
                        hotl_tools::net::EgressDecision::Allow
                    } else {
                        hotl_tools::net::EgressDecision::Deny
                    });
                }
            }
        }
        "detach" => return ClientAction::Detach,
        "shutdown" => return ClientAction::Shutdown,
        _ => {}
    }
    ClientAction::Continue
}

/// Re-issue every parked ask to the newly-attached client (the whole point).
async fn resend_pending(shared: &Arc<Shared>) {
    let frames: Vec<Value> = {
        // Prune asks whose reply channel died (turn cancelled/ended) so a
        // reattach never sees an ask that can no longer be answered.
        let mut pending = shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, (tx, _, _)| !tx.is_closed());
        let mut questions = shared
            .pending_question
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        questions.retain(|_, (tx, _, _)| !tx.is_closed());
        let mut egress = shared
            .pending_egress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        egress.retain(|_, (tx, _, _)| !tx.is_closed());
        // An egress ask raised while the TUI was detached must still be
        // answerable on reattach, or the blocked connection just waits out the
        // proxy's deadline.
        pending
            .values()
            .map(|(_, f, _)| f.clone())
            .chain(questions.values().map(|(_, f, _)| f.clone()))
            .chain(egress.values().map(|(_, f, _)| f.clone()))
            .collect()
    };
    // Tell the client the current session id first (a lightweight hello).
    send(
        shared,
        &json!({"t": "hello", "sessionId": shared.session_id}),
    )
    .await;
    for frame in frames {
        send(shared, &frame).await;
    }
}

async fn drain_events(mut events: tokio::sync::mpsc::Receiver<EngineEvent>, shared: Arc<Shared>) {
    while let Some(event) = events.recv().await {
        match event {
            EngineEvent::Ask {
                summary,
                protected_why,
                reply,
            } => {
                let id = shared.next_ask.fetch_add(1, Ordering::Relaxed);
                let frame = json!({
                    "t": "ask", "id": id, "summary": summary, "protectedWhy": protected_why,
                });
                shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, (reply, frame.clone(), std::time::Instant::now()));
                send(&shared, &frame).await; // no-op if detached; re-sent on attach
            }
            EngineEvent::Question {
                id: qid,
                question,
                reply,
            } => {
                let id = shared.next_ask.fetch_add(1, Ordering::Relaxed);
                let frame = json!({
                    "t": "question",
                    "id": id,
                    "questionId": qid,
                    "header": question.header,
                    "prompt": question.prompt,
                    "options": question.options,
                    "multi": question.multi,
                });
                shared
                    .pending_question
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, (reply, frame.clone(), std::time::Instant::now()));
                send(&shared, &frame).await; // no-op if detached; re-sent on attach
            }
            EngineEvent::EgressAsk { host, reply } => {
                let id = shared.next_ask.fetch_add(1, Ordering::Relaxed);
                let frame = json!({"t": "egress", "id": id, "host": host});
                shared
                    .pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, (reply, frame.clone(), std::time::Instant::now()));
                send(&shared, &frame).await; // no-op if detached; re-sent on attach
            }
            EngineEvent::TurnDone {
                outcome,
                usage,
                mispredictions,
            } => {
                // A turn that ended without its asks being answered left dead
                // reply channels behind — drop them so they never re-issue.
                shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, (tx, _, _)| !tx.is_closed());
                shared
                    .pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, (tx, _, _)| !tx.is_closed());
                let mut done = json!({
                    "t": "turn_done",
                    "schemaVersion": UPDATE_SCHEMA_VERSION,
                    "outcome": outcome_tag(&outcome),
                    "usage": crate::wire::usage_frame(&shared.model, &usage),
                });
                if mispredictions > 0 {
                    done["mispredictions"] = json!(mispredictions);
                }
                send(&shared, &done).await;
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
    v.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
    use hotl_platform::SystemClock;
    use hotl_provider::ScriptedProvider;
    use hotl_store::{Masker, SessionLog};
    use hotl_tools::{rules::Rules, Registry};
    use tokio::io::AsyncRead;

    /// A unique endpoint id per test, so two tests never collide on the
    /// machine-wide pipe namespace Windows has and Unix does not.
    ///
    /// Short on purpose, and it has to stay that way: a unix socket path is
    /// capped at 103 bytes on macOS, and under `nix flake check` the run dir
    /// alone eats 78 of them, leaving 20 for the whole id. `tag` is a label,
    /// not a sentence — the test's own name carries the meaning. Overrun it and
    /// hotl-platform's socket-path check names the path and the overage.
    fn endpoint_id(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        format!(
            "t-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A connected `(client, server)` pair through the platform seam, so the
    /// same test body exercises a unix socket or a named pipe.
    ///
    /// The listener is dropped once both ends exist — the pair is the point,
    /// not the accept loop. Tests that want a live server hand the listener to
    /// `serve_on` instead.
    async fn connected_pair(tag: &str) -> (Stream, Stream) {
        let id = endpoint_id(tag);
        let mut listener = hotl_platform::IPC.bind_private(&id).unwrap();
        let connecting = {
            let id = id.clone();
            tokio::spawn(async move { hotl_platform::IPC.connect(&id).await.unwrap() })
        };
        let server = listener.accept().await.unwrap();
        let client = connecting.await.unwrap();
        if let Some(p) = hotl_platform::IPC.artifact_path(&id) {
            let _ = std::fs::remove_file(p);
        }
        (client, server)
    }

    fn scripted_session() -> SessionHandle {
        scripted_session_logged().0
    }

    /// The same session, plus the path of the log it writes — the expiry test
    /// asserts on the `AskResolved` the engine commits when its ask is denied.
    fn scripted_session_logged() -> (SessionHandle, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).unwrap();
        let log_path = log.path().to_path_buf();
        std::mem::forget(dir);
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
            ScriptedProvider::text_reply("done in the background"),
        ]));
        let handle = spawn_session(SessionDeps {
            concurrency: Default::default(),
            provider,
            registry: Arc::new(Registry::builtin()),
            rules: Arc::new(Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(SystemClock),
            log,
            system: "sys".into(),
            cwd: std::env::temp_dir(),
            hooks: None,
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            initial_decisions: Vec::new(),
            plan_files: None,
            initial_goal: None,
            config: EngineConfig {
                max_turns: 6,
                ..Default::default()
            },
        });
        (handle, log_path)
    }

    async fn next(
        lines: &mut tokio::io::Lines<tokio::io::BufReader<impl AsyncRead + Unpin>>,
    ) -> Value {
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

    /// Guards the peer check on every supported transport: an endpoint we
    /// bound and connected to ourselves must authenticate as us.
    ///
    /// It is defence in depth — the boundary is the endpoint's own access
    /// control, a `0600` socket mode or the pipe's DACL — but a peer check that
    /// silently always passed would be worse than none, because it reads as a
    /// second layer while being none.
    #[tokio::test]
    async fn a_peer_we_opened_ourselves_authenticates() {
        let id = endpoint_id("peer");
        let mut listener = hotl_platform::IPC.bind_private(&id).unwrap();
        let connecting = {
            let id = id.clone();
            tokio::spawn(async move { hotl_platform::IPC.connect(&id).await.unwrap() })
        };
        let server_side = listener.accept().await.unwrap();
        let _client = connecting.await.unwrap();
        assert!(hotl_platform::IPC.authenticate_peer(&server_side).is_ok());
        if let Some(p) = hotl_platform::IPC.artifact_path(&id) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// 0058 T9: an `ask_user` question is parked like a permission ask,
    /// re-issued on attach, and answered by id — before this it fell through
    /// the catch-all and resolved `NoHuman` the moment the sender dropped,
    /// which is how a `human_input` phase would have been answered by nobody.
    #[tokio::test]
    async fn question_frames_park_and_are_re_issued_on_attach() {
        let (client_side, server_side) = connected_pair("ques").await;
        let (_cr, cw) = tokio::io::split(client_side);
        let (sr, _sw) = tokio::io::split(server_side);
        let mut lines = tokio::io::BufReader::new(sr).lines();
        let shared = Arc::new(Shared {
            handle: scripted_session(),
            client: AsyncMutex::new(Some(cw)),
            pending: Mutex::new(HashMap::new()),
            pending_question: Mutex::new(HashMap::new()),
            pending_egress: Mutex::new(HashMap::new()),
            next_ask: AtomicU64::new(1),
            session_id: "test".into(),
            token: "tok".into(),
            model: "m".into(),
            ask_expiry: std::time::Duration::from_secs(3600),
        });
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(drain_events(events_rx, shared.clone()));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        events_tx
            .send(EngineEvent::Question {
                id: "q1".into(),
                question: hotl_types::Question {
                    header: "Approve".into(),
                    prompt: "Ship it?".into(),
                    options: vec![
                        hotl_types::QuestionOption {
                            label: "ship".into(),
                            description: None,
                        },
                        hotl_types::QuestionOption {
                            label: "redo".into(),
                            description: None,
                        },
                    ],
                    multi: false,
                },
                reply: reply_tx,
            })
            .await
            .unwrap();
        let frame = loop {
            let f = next(&mut lines).await;
            if f["t"] == "question" {
                break f;
            }
        };
        assert_eq!(frame["header"], "Approve");
        assert_eq!(frame["questionId"], "q1");
        let id = frame["id"].as_u64().unwrap();
        assert!(shared.pending_question.lock().unwrap().contains_key(&id));

        // A reattach re-issues it: the whole point of parking.
        resend_pending(&shared).await;
        let re = loop {
            let f = next(&mut lines).await;
            if f["t"] == "question" {
                break f;
            }
        };
        assert_eq!(re["id"], json!(id), "the same question, same id");

        handle_frame(
            &json!({"t": "question_reply", "id": id, "option": "ship"}).to_string(),
            &shared,
        )
        .await;
        assert_eq!(
            reply_rx.await.unwrap(),
            hotl_types::QuestionAnswer::Selected(vec!["ship".into()])
        );
    }

    /// A reply naming no option and no text is nobody's answer — never a
    /// made-up selection.
    #[tokio::test]
    async fn a_malformed_question_reply_is_no_human() {
        let (client_side, server_side) = connected_pair("qbad").await;
        let (_cr, cw) = tokio::io::split(client_side);
        let (sr, _sw) = tokio::io::split(server_side);
        let mut lines = tokio::io::BufReader::new(sr).lines();
        let shared = Arc::new(Shared {
            handle: scripted_session(),
            client: AsyncMutex::new(Some(cw)),
            pending: Mutex::new(HashMap::new()),
            pending_question: Mutex::new(HashMap::new()),
            pending_egress: Mutex::new(HashMap::new()),
            next_ask: AtomicU64::new(1),
            session_id: "test".into(),
            token: "tok".into(),
            model: "m".into(),
            ask_expiry: std::time::Duration::from_secs(3600),
        });
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(drain_events(events_rx, shared.clone()));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        events_tx
            .send(EngineEvent::Question {
                id: "q2".into(),
                question: hotl_types::Question {
                    header: "Approve".into(),
                    prompt: "Ship it?".into(),
                    options: vec![hotl_types::QuestionOption {
                        label: "ship".into(),
                        description: None,
                    }],
                    multi: false,
                },
                reply: reply_tx,
            })
            .await
            .unwrap();
        let id = loop {
            let f = next(&mut lines).await;
            if f["t"] == "question" {
                break f["id"].as_u64().unwrap();
            }
        };
        handle_frame(
            &json!({"t": "question_reply", "id": id}).to_string(),
            &shared,
        )
        .await;
        assert_eq!(reply_rx.await.unwrap(), hotl_types::QuestionAnswer::NoHuman);
    }

    /// Plan 0026's egress prompt through the whole server path: framed on the
    /// wire, parked for a detached client, answered by id, and swept when the
    /// turn ends. Driven by feeding the event channel directly — the real
    /// producer is the proxy, which needs a socket and a blocked connection to
    /// exercise.
    #[tokio::test]
    async fn egress_frames_round_trip_and_a_malformed_reply_denies() {
        let (client_side, server_side) = connected_pair("egress").await;
        let (_cr, cw) = tokio::io::split(client_side);
        let (sr, _sw) = tokio::io::split(server_side);
        let mut lines = tokio::io::BufReader::new(sr).lines();
        let shared = Arc::new(Shared {
            handle: scripted_session(),
            client: AsyncMutex::new(Some(cw)),
            pending: Mutex::new(HashMap::new()),
            pending_question: Mutex::new(HashMap::new()),
            pending_egress: Mutex::new(HashMap::new()),
            next_ask: AtomicU64::new(1),
            session_id: "test".into(),
            token: "tok".into(),
            model: "m".into(),
            ask_expiry: std::time::Duration::ZERO,
        });
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(8);
        tokio::spawn(drain_events(events_rx, shared.clone()));

        // 1. An allow round-trips.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        events_tx
            .send(EngineEvent::EgressAsk {
                host: "registry.npmjs.org".into(),
                reply: reply_tx,
            })
            .await
            .unwrap();
        let frame = loop {
            let f = next(&mut lines).await;
            if f["t"] == "egress" {
                break f;
            }
        };
        assert_eq!(frame["host"], "registry.npmjs.org");
        let id = frame["id"].as_u64().unwrap();
        // Parked, so a client that reattaches later can still answer.
        assert!(shared.pending_egress.lock().unwrap().contains_key(&id));
        handle_frame(
            &json!({"t": "egress_reply", "id": id, "allow": true}).to_string(),
            &shared,
        )
        .await;
        assert_eq!(
            reply_rx.await.unwrap(),
            hotl_tools::net::EgressDecision::Allow
        );

        // 2. A reply with no `allow` field denies — asserted, not assumed.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        events_tx
            .send(EngineEvent::EgressAsk {
                host: "evil.example".into(),
                reply: reply_tx,
            })
            .await
            .unwrap();
        let frame = loop {
            let f = next(&mut lines).await;
            if f["t"] == "egress" {
                break f;
            }
        };
        let id = frame["id"].as_u64().unwrap();
        handle_frame(&json!({"t": "egress_reply", "id": id}).to_string(), &shared).await;
        assert_eq!(
            reply_rx.await.unwrap(),
            hotl_tools::net::EgressDecision::Deny
        );

        // 3. A dead reply channel is swept on TurnDone, so it never re-issues
        //    to a reattaching client.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        events_tx
            .send(EngineEvent::EgressAsk {
                host: "stale.example".into(),
                reply: reply_tx,
            })
            .await
            .unwrap();
        loop {
            if next(&mut lines).await["t"] == "egress" {
                break;
            }
        }
        drop(reply_rx);
        events_tx
            .send(EngineEvent::TurnDone {
                outcome: hotl_engine::Outcome::Done {
                    text: String::new(),
                },
                usage: Default::default(),
                mispredictions: 0,
            })
            .await
            .unwrap();
        loop {
            if next(&mut lines).await["t"] == "turn_done" {
                break;
            }
        }
        assert!(
            shared.pending_egress.lock().unwrap().is_empty(),
            "a dead egress reply channel must not survive the turn"
        );
    }

    /// 0059 T5: a parked ask nobody answers is denied once `ask_expiry` is up,
    /// and the engine records the resolution — the turn resumes instead of
    /// holding the session open forever.
    #[tokio::test]
    async fn a_parked_ask_expires_and_is_denied() {
        let id = endpoint_id("expiry");
        let listener = hotl_platform::IPC.bind_private(&id).unwrap();
        let (handle, log_path) = scripted_session_logged();
        tokio::spawn(serve_on(
            listener,
            "test".into(),
            "m".into(),
            handle,
            None,
            "tok".into(),
            std::time::Duration::from_millis(200),
        ));

        let (r, mut w) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
        let mut lines = tokio::io::BufReader::new(r).lines();
        send(&mut w, json!({"t":"auth","token":"tok"})).await;
        send(&mut w, json!({"t":"prompt","text":"go"})).await;

        // Never answer the ask. The sweep denies it, the turn runs on, and the
        // client is told why.
        let (mut expired, mut done) = (false, None);
        while done.is_none() {
            let f = next(&mut lines).await;
            match f["t"].as_str().unwrap_or("") {
                "error" if f["message"].as_str().unwrap_or("").contains("expired") => {
                    expired = true
                }
                "turn_done" => done = Some(f),
                _ => {}
            }
        }
        assert!(expired, "the client is told the ask expired");
        assert_eq!(done.unwrap()["outcome"]["kind"], "done");

        let resolutions: Vec<bool> = std::fs::read_to_string(&log_path)
            .expect("read log")
            .lines()
            .filter_map(|l| serde_json::from_str::<hotl_types::Entry>(l).ok())
            .filter_map(|e| match e.payload {
                hotl_types::EntryPayload::AskResolved { allowed, .. } => Some(allowed),
                _ => None,
            })
            .collect();
        assert_eq!(
            resolutions,
            vec![false],
            "the expiry is recorded as a denial"
        );
    }

    /// 0058 T9 × 0059 T5: the ask expiry deliberately does **not** reach
    /// parked questions. A question authorizes nothing, so leaving one parked
    /// risks nothing — and a `human_input` workflow step is a recipe
    /// deliberately waiting for a person, with its own `timeout_secs` chosen
    /// by whoever wrote it. A global hour would silently override that and
    /// route `on_timeout` behind the author's back.
    #[tokio::test]
    async fn the_ask_expiry_never_touches_a_parked_question() {
        let shared = Arc::new(Shared {
            handle: scripted_session(),
            client: AsyncMutex::new(None),
            pending: Mutex::new(HashMap::new()),
            pending_question: Mutex::new(HashMap::new()),
            pending_egress: Mutex::new(HashMap::new()),
            next_ask: AtomicU64::new(1),
            session_id: "test".into(),
            token: "tok".into(),
            model: "m".into(),
            ask_expiry: std::time::Duration::from_secs(3600),
        });
        let (ask_tx, ask_rx) = tokio::sync::oneshot::channel();
        let (q_tx, q_rx) = tokio::sync::oneshot::channel();
        let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(7200);
        shared
            .pending
            .lock()
            .unwrap()
            .insert(1, (ask_tx, json!({}), long_ago));
        shared
            .pending_question
            .lock()
            .unwrap()
            .insert(2, (q_tx, json!({}), long_ago));

        // Both are two hours old; only the permission ask expires.
        assert_eq!(sweep_expired_asks(&shared), 1);
        assert!(matches!(
            ask_rx.await,
            Ok(hotl_engine::AskReply::Deny { .. })
        ));
        assert_eq!(
            shared.pending_question.lock().unwrap().len(),
            1,
            "a question outlives the ask expiry — its deadline is the recipe's"
        );
        // Still answerable, which is the whole point.
        handle_frame(
            &json!({"t": "question_reply", "id": 2, "option": "ship"}).to_string(),
            &shared,
        )
        .await;
        assert_eq!(
            q_rx.await.unwrap(),
            hotl_types::QuestionAnswer::Selected(vec!["ship".into()])
        );
    }

    /// The sweep's own contract, without a socket behind it: only asks past
    /// the deadline are denied, and the denial says why.
    #[tokio::test]
    async fn the_sweep_denies_only_what_is_past_the_deadline() {
        let shared = Arc::new(Shared {
            handle: scripted_session(),
            client: AsyncMutex::new(None),
            pending: Mutex::new(HashMap::new()),
            pending_question: Mutex::new(HashMap::new()),
            pending_egress: Mutex::new(HashMap::new()),
            next_ask: AtomicU64::new(1),
            session_id: "test".into(),
            token: "tok".into(),
            model: "m".into(),
            ask_expiry: std::time::Duration::from_secs(3600),
        });
        let (old_tx, old_rx) = tokio::sync::oneshot::channel();
        let (fresh_tx, fresh_rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = shared.pending.lock().unwrap();
            let long_ago = std::time::Instant::now() - std::time::Duration::from_secs(7200);
            pending.insert(1, (old_tx, json!({}), long_ago));
            pending.insert(2, (fresh_tx, json!({}), std::time::Instant::now()));
        }
        assert_eq!(sweep_expired_asks(&shared), 1);
        match old_rx.await.expect("the expired ask was answered") {
            hotl_engine::AskReply::Deny { message } => assert_eq!(
                message.as_deref(),
                Some("expired after 3600 s with no human")
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            shared.pending.lock().unwrap().len(),
            1,
            "the fresh ask stays"
        );
        drop(fresh_rx);
    }

    #[tokio::test]
    async fn detach_while_asking_then_reattach_reissues_the_ask() {
        let id = endpoint_id("serve");
        let listener = hotl_platform::IPC.bind_private(&id).unwrap();
        tokio::spawn(serve_on(
            listener,
            "test".into(),
            "m".into(),
            scripted_session(),
            None,
            "tok".into(),
            std::time::Duration::ZERO,
        ));

        // Attach (authenticate first), prompt; the scripted bash call is gated.
        let (r, mut w) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
        let mut lines = tokio::io::BufReader::new(r).lines();
        send(&mut w, json!({"t":"auth","token":"tok"})).await;
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
        let (r2, mut w2) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
        let mut lines2 = tokio::io::BufReader::new(r2).lines();
        send(&mut w2, json!({"t":"auth","token":"tok"})).await;
        let reissued = loop {
            let f = next(&mut lines2).await;
            if f["t"] == "ask" {
                break f["id"].as_u64().unwrap();
            }
        };
        assert_eq!(
            reissued, ask_id,
            "the same parked ask must re-issue on reattach"
        );

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

    #[tokio::test]
    async fn unauthenticated_connect_is_rejected_and_keeps_the_incumbent() {
        // Vuln 1: a connect that fails auth is refused and must not evict the
        // attached human or drive the session.
        let id = endpoint_id("serve");
        let listener = hotl_platform::IPC.bind_private(&id).unwrap();
        tokio::spawn(serve_on(
            listener,
            "test".into(),
            "m".into(),
            scripted_session(),
            None,
            "tok".into(),
            std::time::Duration::ZERO,
        ));

        // Authenticated client A drives to a parked ask.
        let (r, mut w) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
        let mut a = tokio::io::BufReader::new(r).lines();
        send(&mut w, json!({"t":"auth","token":"tok"})).await;
        send(&mut w, json!({"t":"prompt","text":"go"})).await;
        let ask_id = loop {
            let f = next(&mut a).await;
            if f["t"] == "ask" {
                break f["id"].as_u64().unwrap();
            }
        };

        // Client B connects with the WRONG token: rejected outright.
        let (rb, mut wb) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
        let mut b = tokio::io::BufReader::new(rb).lines();
        send(&mut wb, json!({"t":"auth","token":"WRONG"})).await;
        assert_eq!(
            next(&mut b).await["t"],
            "error",
            "an unauthenticated connect must be rejected"
        );

        // A was never evicted — it answers its ask and the turn completes.
        send(&mut w, json!({"t":"ask_reply","id":ask_id,"allow":true})).await;
        let done = loop {
            let f = next(&mut a).await;
            assert_ne!(
                f["t"], "detached",
                "A must not be evicted by an unauthed connect"
            );
            if f["t"] == "turn_done" {
                break f;
            }
        };
        assert_eq!(done["outcome"]["kind"], "done");
    }
}
