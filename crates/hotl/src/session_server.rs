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

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hotl_engine::SessionHandle;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, BufReader};

use crate::acp::{ClientCaps, Connection, Hosted};
use crate::acp_ext::codes;
use hotl_platform::{Ipc as _, IpcListener as _};

/// The session transport, bound in one place: a unix socket on Unix, a named
/// pipe on Windows. `tokio::io::split` rather than a transport-specific
/// `into_split`, because it is the one spelling both have.
type Stream = <hotl_platform::ActiveIpc as hotl_platform::Ipc>::Stream;
type Listener = <hotl_platform::ActiveIpc as hotl_platform::Ipc>::Listener;
type ReadHalf = tokio::io::ReadHalf<Stream>;
type WriteHalf = tokio::io::WriteHalf<Stream>;
type Lines = tokio::io::Lines<BufReader<ReadHalf>>;

pub const META_SOCKET_TOKEN: &str = crate::acp_ext::META_SOCKET_TOKEN;

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

pub struct Served {
    pub session_id: String,
    pub info: crate::acp::ServerInfo,
    pub cwd: PathBuf,
    pub effort: Option<String>,
    pub handle: SessionHandle,
    pub prompt: Option<String>,
    pub ask_expiry: std::time::Duration,
}

pub fn detached_caps() -> ClientCaps {
    ClientCaps {
        ext: true,
        notices: true,
        compaction: true,
        elicitation_form: true,
        auth_terminal: false,
    }
}

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
/// socket) and open with an `initialize` carrying the session token. Returns
/// the reader, the writer and that `initialize`, or None — the connection is
/// closed without a reply and the incumbent is left alone.
async fn authenticate(stream: Stream, token: &str) -> Option<(Lines, WriteHalf, Value)> {
    if let Err(why) = hotl_platform::IPC.authenticate_peer(&stream) {
        // Defence in depth behind the endpoint's own access control, which is
        // the actual boundary — a `0600` socket mode or the pipe's DACL.
        let _ = why;
        return None;
    }
    let (read, write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read).lines();
    // Bound the handshake so a silent connect cannot stall the accept loop.
    let line =
        match tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            _ => return None,
        };
    let msg = serde_json::from_str::<Value>(&line).ok()?;
    initialize_carries(&msg, token).then_some((lines, write, msg))
}

fn initialize_carries(msg: &Value, token: &str) -> bool {
    msg.get("method").and_then(Value::as_str) == Some("initialize")
        && msg.get("id").is_some()
        && msg
            .pointer("/params/_meta")
            .and_then(|m| m.get(META_SOCKET_TOKEN))
            .and_then(Value::as_str)
            .is_some_and(|t| tokens_match(t, token))
}

/// Live backgrounded sessions, by id.
pub fn list_live() -> Vec<String> {
    let mut ids = hotl_platform::IPC.list_live();
    ids.sort();
    ids
}

/// Run a detached session bound to `run_dir/<session_id>.sock`.
pub async fn serve(served: Served) -> i32 {
    let session_id = served.session_id.clone();
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
    serve_on(listener, served, token).await;
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

#[derive(Clone, Default)]
struct ClientSlot(Arc<std::sync::Mutex<Option<WriteHalf>>>);

impl ClientSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<WriteHalf>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn poll_with(
        &self,
        cx: &mut Context<'_>,
        f: impl FnOnce(Pin<&mut WriteHalf>, &mut Context<'_>) -> Poll<std::io::Result<()>>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = self.lock();
        let Some(w) = guard.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match f(Pin::new(w), cx) {
            Poll::Ready(Err(_)) => {
                *guard = None;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl AsyncWrite for ClientSlot {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut guard = self.lock();
        let Some(w) = guard.as_mut() else {
            return Poll::Ready(Ok(buf.len()));
        };
        match Pin::new(w).poll_write(cx, buf) {
            Poll::Ready(Err(_)) => {
                *guard = None;
                Poll::Ready(Ok(buf.len()))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_with(cx, |w, cx| w.poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_with(cx, |w, cx| w.poll_shutdown(cx))
    }
}

/// The socket-server core over a pre-bound listener (testable without the
/// filesystem run-dir): host the session on one ACP connection whose writer
/// follows the attached client, submit the opening prompt, then serve
/// clients one at a time — the session lives across attach/detach.
pub async fn serve_on(listener: Listener, served: Served, token: String) {
    let slot = ClientSlot::default();
    let writer = crate::acp::Writer::new(slot.clone());
    let connection = Connection::hosting(
        writer,
        served.info,
        Hosted {
            handle: served.handle,
            session_id: served.session_id,
            cwd: served.cwd,
            effort: served.effort,
            caps: detached_caps(),
        },
    );
    if let (Some(p), Some(handle)) = (served.prompt, connection.handle()) {
        handle.prompt(p).await;
    }
    accept_loop(listener, connection, slot, token, served.ask_expiry).await;
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

/// What one client message asks of the server.
enum ClientAction {
    Continue,
    Initialize(Value),
    Detach,
    Shutdown,
}

async fn switch_client(
    connection: &mut Connection,
    slot: &ClientSlot,
    write: Option<Option<WriteHalf>>,
    caps: ClientCaps,
) {
    connection.set_client_caps(caps);
    connection.forget_prompts();
    let adopt = connection.caps_adopter();
    let writer = connection.writer().clone();
    writer
        .next_client(|| {
            adopt();
            if let Some(write) = write {
                *slot.lock() = write;
            }
        })
        .await;
}

/// Accepting stays live while a client is attached — a second `hotl attach`
/// takes over (the previous client's connection is closed) instead of
/// hanging unread in the listener backlog.
async fn accept_loop(
    mut listener: Listener,
    mut connection: Connection,
    slot: ClientSlot,
    token: String,
    ask_expiry: std::time::Duration,
) {
    let mut reader: Option<Lines> = None;
    let mut sweep = tokio::time::interval(sweep_period(ask_expiry));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // A parked ask nobody ever answers holds a turn open forever
            // (0059 T5). Denying it is the fail-closed answer, and the engine
            // commits the `AskResolved` when its await returns.
            _ = sweep.tick() => {
                let n = connection.expire_parked(ask_expiry);
                if n > 0 {
                    connection
                        .notice(format!(
                            "{n} parked ask(s) expired after {} s with no human and were denied",
                            ask_expiry.as_secs()
                        ))
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
                let Some((new_reader, write, init)) = authenticate(stream, &token).await else {
                    continue;
                };
                let caps = crate::acp::caps_of_initialize(&init);
                switch_client(&mut connection, &slot, Some(Some(write)), caps).await;
                reader = Some(new_reader);
                connection.on_message(&init).await;
            }
            // Lines::next_line is cancel-safe: a frame half-read when the
            // accept arm wins stays buffered.
            line = next_line(&mut reader), if reader.is_some() => {
                let action = match line {
                    Some(line) => on_line(&line, &mut connection).await,
                    None => ClientAction::Detach,
                };
                match action {
                    ClientAction::Continue => {}
                    ClientAction::Initialize(init) => {
                        let caps = crate::acp::caps_of_initialize(&init);
                        switch_client(&mut connection, &slot, None, caps).await;
                        connection.on_message(&init).await;
                    }
                    ClientAction::Detach => {
                        reader = None;
                        switch_client(&mut connection, &slot, Some(None), detached_caps()).await;
                    }
                    ClientAction::Shutdown => break,
                }
            }
        }
    }
}

async fn next_line(reader: &mut Option<Lines>) -> Option<String> {
    match reader {
        Some(lines) => lines.next_line().await.ok().flatten(),
        // Unreachable behind the select guard; never resolve regardless.
        None => std::future::pending().await,
    }
}

async fn on_line(line: &str, connection: &mut Connection) -> ClientAction {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return ClientAction::Continue;
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let writer = connection.writer().clone();
    let session_id = connection.session_id().unwrap_or_default().to_string();
    match msg.get("method").and_then(Value::as_str) {
        Some("initialize") => return ClientAction::Initialize(msg),
        Some(method @ ("session/new" | "session/load" | "_hotl/session/fork")) => {
            crate::acp::reply_err(
                &writer,
                id,
                codes::INVALID_PARAMS,
                &format!(
                    "{method}: this socket hosts the live session `{session_id}` — attach with session/resume"
                ),
            )
            .await;
        }
        Some("session/resume") => {
            let request = msg
                .get("params")
                .cloned()
                .map(serde_json::from_value::<crate::acp_types::ResumeSessionRequest>);
            match request {
                Some(Ok(r)) if r.session_id == session_id => {
                    crate::acp::reply_ok(&writer, id, connection.resume_result()).await;
                    for request in connection.parked_requests() {
                        crate::acp::send(&writer, &request).await;
                    }
                }
                Some(Ok(r)) => {
                    crate::acp::reply_err(
                        &writer,
                        id,
                        codes::NOT_FOUND,
                        &format!(
                            "unknown session `{}` — this socket hosts `{session_id}`",
                            r.session_id
                        ),
                    )
                    .await;
                }
                _ => {
                    crate::acp::reply_err(
                        &writer,
                        id,
                        codes::INVALID_PARAMS,
                        "session/resume requires params.sessionId and params.cwd",
                    )
                    .await;
                }
            }
        }
        Some("session/close") => {
            crate::acp::reply_ok(&writer, id, json!({})).await;
            return ClientAction::Detach;
        }
        Some("_hotl/session/stop") => {
            connection.on_message(&msg).await;
            return ClientAction::Shutdown;
        }
        _ => connection.on_message(&msg).await,
    }
    ClientAction::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_engine::{spawn_session, EngineConfig, EngineEvent, SessionDeps};
    use hotl_platform::SystemClock;
    use hotl_provider::ScriptedProvider;
    use hotl_store::{Masker, SessionLog};
    use hotl_tools::{rules::Rules, Registry};
    use tokio::io::AsyncWriteExt;

    fn endpoint_id(tag: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        format!(
            "t-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn scripted_session() -> SessionHandle {
        scripted_session_logged().0
    }

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

    fn info() -> crate::acp::ServerInfo {
        crate::acp::ServerInfo {
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

    fn served(handle: SessionHandle, ask_expiry: std::time::Duration) -> Served {
        Served {
            session_id: "test".into(),
            info: info(),
            cwd: std::env::temp_dir(),
            effort: None,
            handle,
            prompt: None,
            ask_expiry,
        }
    }

    fn start(
        handle: SessionHandle,
        ask_expiry: std::time::Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let id = endpoint_id("serve");
        let listener = hotl_platform::IPC.bind_private(&id).unwrap();
        let task = tokio::spawn(serve_on(listener, served(handle, ask_expiry), "tok".into()));
        (id, task)
    }

    struct Client {
        lines: tokio::io::Lines<BufReader<ReadHalf>>,
        write: WriteHalf,
        next_id: u64,
    }

    impl Client {
        async fn connect(id: &str) -> Client {
            let (read, write) = tokio::io::split(hotl_platform::IPC.connect(id).await.unwrap());
            Client {
                lines: BufReader::new(read).lines(),
                write,
                next_id: 0,
            }
        }

        async fn send(&mut self, msg: Value) {
            let mut line = msg.to_string();
            line.push('\n');
            self.write.write_all(line.as_bytes()).await.unwrap();
            self.write.flush().await.unwrap();
        }

        async fn request(&mut self, method: &str, params: Value) -> u64 {
            self.next_id += 1;
            let id = self.next_id;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            id
        }

        async fn next(&mut self) -> Option<Value> {
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(5), self.lines.next_line())
                    .await
                    .expect("message timeout")
                    .ok()
                    .flatten()?;
            Some(serde_json::from_str(&line).expect("json line"))
        }

        async fn until(&mut self, mut found: impl FnMut(&Value) -> bool) -> Value {
            loop {
                let msg = self.next().await.expect("the server hung up");
                if found(&msg) {
                    return msg;
                }
            }
        }

        async fn reply_to(&mut self, id: u64) -> Value {
            self.until(|m| m.get("method").is_none() && m["id"] == json!(id))
                .await
        }

        async fn request_of(&mut self, method: &str) -> Value {
            self.until(|m| m["method"] == method && m.get("id").is_some())
                .await
        }

        async fn initialize(&mut self, token: Option<&str>, caps: Value) -> u64 {
            let mut params = json!({"protocolVersion": 1, "clientCapabilities": caps});
            if let Some(t) = token {
                params["_meta"] = json!({META_SOCKET_TOKEN: t});
            }
            self.request("initialize", params).await
        }

        async fn attach(id: &str, caps: Value) -> Client {
            let mut client = Client::connect(id).await;
            let init = client.initialize(Some("tok"), caps).await;
            let reply = client.reply_to(init).await;
            assert_eq!(reply["result"]["protocolVersion"], 1, "{reply}");
            let resume = client
                .request(
                    "session/resume",
                    json!({"sessionId": "test", "cwd": std::env::temp_dir()}),
                )
                .await;
            let reply = client.next().await.expect("the resume reply");
            assert_eq!(
                reply["id"],
                json!(resume),
                "the resume reply comes first: {reply}"
            );
            assert!(reply.get("result").is_some(), "{reply}");
            client
        }

        async fn prompt(&mut self, text: &str) -> u64 {
            self.request(
                "session/prompt",
                json!({"sessionId": "test", "prompt": [{"type": "text", "text": text}]}),
            )
            .await
        }

        async fn allow(&mut self, id: &Value) {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}}))
                .await;
        }

        async fn hung_up(&mut self) -> bool {
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), self.lines.next_line())
                    .await,
                Ok(Ok(None)) | Ok(Err(_))
            )
        }
    }

    fn hotl_caps() -> Value {
        json!({"session": {"notices": {}}, "_meta": {"hotl.dev/extensions": 1}})
    }

    fn log_has(path: &std::path::Path, text: &str) -> bool {
        std::fs::read_to_string(path).is_ok_and(|s| s.contains(text))
    }

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

    #[tokio::test]
    async fn prompt_round_trip_answers_with_a_stop_reason() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut client = Client::attach(&id, hotl_caps()).await;
        let prompt = client.prompt("go").await;
        let ask = client.request_of("session/request_permission").await;
        client.allow(&ask["id"]).await;
        let reply = client.reply_to(prompt).await;
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        assert_eq!(
            reply["result"]["_meta"]["hotl.dev/turn"]["outcome"]["text"],
            "done in the background"
        );
    }

    #[tokio::test]
    async fn attach_receives_a_parked_ask_as_request_permission() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut first = Client::attach(&id, hotl_caps()).await;
        first.prompt("go").await;
        let asked = first.request_of("session/request_permission").await;
        drop(first);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut second = Client::attach(&id, hotl_caps()).await;
        let reissued = second.next().await.expect("the parked ask");
        assert_eq!(reissued["method"], "session/request_permission");
        assert_eq!(reissued["id"], asked["id"], "the same parked ask, same id");
        assert_eq!(reissued["params"], asked["params"]);

        second.allow(&reissued["id"]).await;
        let done = second
            .until(|m| m["params"]["update"]["sessionUpdate"] == "usage_update")
            .await;
        assert_eq!(done["params"]["sessionId"], "test");
    }

    #[tokio::test]
    async fn detached_render_events_are_dropped() {
        let (handle, log_path) = scripted_session_logged();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        let mut first = Client::attach(&id, hotl_caps()).await;
        first.prompt("go").await;
        let ask = first.request_of("session/request_permission").await;
        first.allow(&ask["id"]).await;
        drop(first);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !log_has(&log_path, "done in the background") {
            assert!(
                std::time::Instant::now() < deadline,
                "the turn never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut second = Client::attach(&id, hotl_caps()).await;
        let quiet = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            second.lines.next_line(),
        )
        .await;
        assert!(
            quiet.is_err(),
            "nothing rendered while detached reaches the next client: {quiet:?}"
        );
    }

    #[tokio::test]
    async fn a_second_client_takes_over_and_the_first_is_closed() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut first = Client::attach(&id, hotl_caps()).await;
        let mut second = Client::attach(&id, hotl_caps()).await;
        assert!(
            first.hung_up().await,
            "the first client is closed on takeover"
        );
        let prompt = second.prompt("go").await;
        let ask = second.request_of("session/request_permission").await;
        second.allow(&ask["id"]).await;
        assert_eq!(
            second.reply_to(prompt).await["result"]["stopReason"],
            "end_turn"
        );
    }

    #[tokio::test]
    async fn a_wrong_or_missing_token_is_refused_and_keeps_the_incumbent() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut incumbent = Client::attach(&id, hotl_caps()).await;
        let prompt = incumbent.prompt("go").await;
        let ask = incumbent.request_of("session/request_permission").await;

        for token in [Some("WRONG"), None] {
            let mut intruder = Client::connect(&id).await;
            intruder.initialize(token, json!({})).await;
            assert!(
                intruder.hung_up().await,
                "an unauthenticated initialize is closed without a reply ({token:?})"
            );
        }
        let mut wrong_first = Client::connect(&id).await;
        wrong_first.prompt("hijack").await;
        assert!(
            wrong_first.hung_up().await,
            "anything before initialize is refused"
        );

        incumbent.allow(&ask["id"]).await;
        assert_eq!(
            incumbent.reply_to(prompt).await["result"]["stopReason"],
            "end_turn"
        );
    }

    #[tokio::test]
    async fn the_socket_hosts_one_live_session() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut client = Client::connect(&id).await;
        let init = client.initialize(Some("tok"), hotl_caps()).await;
        client.reply_to(init).await;
        let other = client
            .request(
                "session/resume",
                json!({"sessionId": "elsewhere", "cwd": std::env::temp_dir()}),
            )
            .await;
        assert_eq!(client.reply_to(other).await["error"]["code"], -32002);
        let new = client
            .request(
                "session/new",
                json!({"cwd": std::env::temp_dir(), "mcpServers": []}),
            )
            .await;
        assert_eq!(client.reply_to(new).await["error"]["code"], -32602);
        let resume = client
            .request(
                "session/resume",
                json!({"sessionId": "test", "cwd": std::env::temp_dir()}),
            )
            .await;
        let reply = client.reply_to(resume).await;
        assert_eq!(reply["result"]["modes"]["currentModeId"], "ask", "{reply}");
    }

    #[tokio::test]
    async fn session_close_detaches_and_the_session_lives_on() {
        let (id, task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut client = Client::attach(&id, hotl_caps()).await;
        let close = client
            .request("session/close", json!({"sessionId": "test"}))
            .await;
        assert!(client.reply_to(close).await.get("result").is_some());
        assert!(client.hung_up().await);
        assert!(!task.is_finished());
        let mut again = Client::attach(&id, hotl_caps()).await;
        let prompt = again.prompt("go").await;
        let ask = again.request_of("session/request_permission").await;
        again.allow(&ask["id"]).await;
        assert_eq!(
            again.reply_to(prompt).await["result"]["stopReason"],
            "end_turn"
        );
    }

    #[tokio::test]
    async fn stop_ends_the_session() {
        let (id, task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut client = Client::attach(&id, hotl_caps()).await;
        let stop = client.request("_hotl/session/stop", json!({})).await;
        assert_eq!(client.reply_to(stop).await["result"]["ok"], true);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("the server exits on stop")
            .unwrap();
        assert!(client.hung_up().await);
    }

    #[tokio::test]
    async fn a_parked_ask_expires_and_is_denied() {
        let (handle, log_path) = scripted_session_logged();
        let (id, _task) = start(handle, std::time::Duration::from_millis(200));
        let mut client = Client::attach(&id, hotl_caps()).await;
        let prompt = client.prompt("go").await;
        let notice = client
            .until(|m| m["params"]["update"]["sessionUpdate"] == "notice")
            .await;
        assert!(
            notice["params"]["update"]["title"]
                .as_str()
                .unwrap_or("")
                .contains("expired"),
            "{notice}"
        );
        assert_eq!(
            client.reply_to(prompt).await["result"]["stopReason"],
            "end_turn"
        );
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

    fn injected() -> (SessionHandle, tokio::sync::mpsc::Sender<EngineEvent>) {
        let mut handle = scripted_session();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        handle.events = rx;
        (handle, tx)
    }

    #[tokio::test]
    async fn a_question_parks_and_is_re_issued_on_attach() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::from_secs(3600));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        events
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
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = Client::attach(
            &id,
            json!({"elicitation": {"form": {}}, "_meta": {"hotl.dev/extensions": 1}}),
        )
        .await;
        let asked = client.next().await.expect("the parked question");
        assert_eq!(asked["method"], "elicitation/create", "{asked}");
        assert_eq!(asked["params"]["message"], "Approve: Ship it?");
        client
            .send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"action": "accept", "content": {"answer": "ship"}}}))
            .await;
        assert_eq!(
            reply_rx.await.unwrap(),
            hotl_types::QuestionAnswer::Selected(vec!["ship".into()])
        );
    }

    #[tokio::test]
    async fn an_egress_ask_parks_and_a_malformed_reply_denies() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        let (allow_tx, allow_rx) = tokio::sync::oneshot::channel();
        events
            .send(EngineEvent::EgressAsk {
                host: "registry.npmjs.org".into(),
                reply: allow_tx,
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut client = Client::attach(&id, hotl_caps()).await;
        let asked = client.next().await.expect("the parked egress ask");
        assert_eq!(asked["method"], "session/request_permission");
        assert_eq!(
            asked["params"]["toolCall"]["_meta"]["hotl.dev/egress"]["host"],
            "registry.npmjs.org"
        );
        client.allow(&asked["id"]).await;
        assert_eq!(
            allow_rx.await.unwrap(),
            hotl_tools::net::EgressDecision::Allow
        );

        let (deny_tx, deny_rx) = tokio::sync::oneshot::channel();
        events
            .send(EngineEvent::EgressAsk {
                host: "evil.example".into(),
                reply: deny_tx,
            })
            .await
            .unwrap();
        let asked = client.request_of("session/request_permission").await;
        client
            .send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"allow": true}}))
            .await;
        assert_eq!(
            deny_rx.await.unwrap(),
            hotl_tools::net::EgressDecision::Deny
        );
    }

    fn plain_caps() -> Value {
        json!({})
    }

    async fn inject_ask(
        events: &tokio::sync::mpsc::Sender<EngineEvent>,
    ) -> tokio::sync::oneshot::Receiver<hotl_engine::AskReply> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        events
            .send(EngineEvent::Ask {
                summary: "bash: rm -rf build".into(),
                protected_why: Some("touches the build dir".into()),
                call_id: "c1".into(),
                tool: "bash".into(),
                input: json!({"command": "rm -rf build"}),
                reply,
            })
            .await
            .unwrap();
        rx
    }

    async fn inject_question(
        events: &tokio::sync::mpsc::Sender<EngineEvent>,
    ) -> tokio::sync::oneshot::Receiver<hotl_types::QuestionAnswer> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        events
            .send(EngineEvent::Question {
                id: "q".into(),
                question: hotl_types::Question {
                    header: "Ship".into(),
                    prompt: "now?".into(),
                    options: vec![hotl_types::QuestionOption {
                        label: "yes".into(),
                        description: None,
                    }],
                    multi: false,
                },
                reply,
            })
            .await
            .unwrap();
        rx
    }

    async fn inject_egress(
        events: &tokio::sync::mpsc::Sender<EngineEvent>,
    ) -> tokio::sync::oneshot::Receiver<hotl_tools::net::EgressDecision> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        events
            .send(EngineEvent::EgressAsk {
                host: "example.com".into(),
                reply,
            })
            .await
            .unwrap();
        rx
    }

    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    async fn drained(client: &mut Client) -> Vec<Value> {
        let mut got = Vec::new();
        while let Ok(Ok(Some(line))) = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            client.lines.next_line(),
        )
        .await
        {
            got.push(serde_json::from_str(&line).unwrap());
        }
        got
    }

    #[tokio::test]
    async fn a_plain_client_resumes_to_standard_requests_only() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        let _ask = inject_ask(&events).await;
        let _question = inject_question(&events).await;
        let _egress = inject_egress(&events).await;
        settle().await;

        let mut plain = Client::attach(&id, plain_caps()).await;
        let got = drained(&mut plain).await;
        let text = serde_json::to_string(&got).unwrap();
        assert!(!text.contains("hotl.dev/"), "{text}");
        assert!(
            got.iter()
                .all(|m| m["method"] == "session/request_permission"),
            "only the two permission requests, nothing the client did not offer: {text}"
        );
        assert_eq!(got.len(), 2, "{text}");
        drop(plain);
        settle().await;

        let mut hotl = Client::attach(
            &id,
            json!({"elicitation": {"form": {}}, "_meta": {"hotl.dev/extensions": 1}}),
        )
        .await;
        let got = drained(&mut hotl).await;
        let methods: Vec<&str> = got.iter().filter_map(|m| m["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "session/request_permission",
                "elicitation/create",
                "session/request_permission"
            ],
            "the question stayed parked for a client that can answer it"
        );
        assert_eq!(
            got[0]["params"]["toolCall"]["_meta"]["hotl.dev/ask"]["summary"],
            "bash: rm -rf build"
        );
        assert_eq!(
            got[2]["params"]["toolCall"]["_meta"]["hotl.dev/egress"]["host"],
            "example.com"
        );
    }

    #[tokio::test]
    async fn a_question_raised_after_a_plain_client_left_still_parks() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        drop(Client::attach(&id, plain_caps()).await);
        settle().await;
        let mut answer = inject_question(&events).await;
        settle().await;
        assert!(answer.try_recv().is_err(), "parked, not answered by nobody");
        let mut hotl = Client::attach(
            &id,
            json!({"elicitation": {"form": {}}, "_meta": {"hotl.dev/extensions": 1}}),
        )
        .await;
        let asked = hotl.next().await.expect("the parked question");
        assert_eq!(asked["method"], "elicitation/create", "{asked}");
    }

    #[tokio::test]
    async fn an_egress_raised_after_a_plain_client_left_is_still_an_egress() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        drop(Client::attach(&id, plain_caps()).await);
        settle().await;
        let _egress = inject_egress(&events).await;
        settle().await;
        let mut hotl = Client::attach(&id, hotl_caps()).await;
        let asked = hotl.next().await.expect("the parked egress ask");
        assert!(
            matches!(
                hotl_tui::client::decode(&asked),
                Some(hotl_tui::client::ServerMsg::EgressRequest { .. })
            ),
            "{asked}"
        );
    }

    #[tokio::test]
    async fn an_ask_raised_before_resume_is_sent_once() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        let mut client = Client::connect(&id).await;
        let init = client.initialize(Some("tok"), hotl_caps()).await;
        client.reply_to(init).await;
        let _ask = inject_ask(&events).await;
        settle().await;
        client
            .request(
                "session/resume",
                json!({"sessionId": "test", "cwd": std::env::temp_dir()}),
            )
            .await;
        let got = drained(&mut client).await;
        let asks = got
            .iter()
            .filter(|m| m["method"] == "session/request_permission")
            .count();
        assert_eq!(asks, 1, "{got:?}");
    }

    #[tokio::test]
    async fn a_second_resume_resends_nothing_and_a_new_initialize_starts_over() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        let _ask = inject_ask(&events).await;
        settle().await;
        let mut client = Client::attach(&id, hotl_caps()).await;
        assert_eq!(drained(&mut client).await.len(), 1, "the parked ask, once");
        let resume = json!({"sessionId": "test", "cwd": std::env::temp_dir()});
        client.request("session/resume", resume.clone()).await;
        let again = drained(&mut client).await;
        assert_eq!(again.len(), 1, "only the resume reply: {again:?}");

        let init = client.initialize(Some("tok"), plain_caps()).await;
        client.reply_to(init).await;
        client.request("session/resume", resume).await;
        let fresh = drained(&mut client).await;
        let asks: Vec<&Value> = fresh
            .iter()
            .filter(|m| m["method"] == "session/request_permission")
            .collect();
        assert_eq!(asks.len(), 1, "{fresh:?}");
        assert!(
            !serde_json::to_string(&asks).unwrap().contains("hotl.dev/"),
            "rebuilt for the caps of the new initialize"
        );
    }

    #[tokio::test]
    async fn the_next_client_never_gets_replies_owed_to_the_last_one() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        let mut first = Client::attach(&id, hotl_caps()).await;
        let prompt = first.prompt("go").await;
        first.request_of("session/request_permission").await;
        drop(first);
        settle().await;
        let mut second = Client::attach(&id, hotl_caps()).await;
        let ask = second.request_of("session/request_permission").await;
        second.allow(&ask["id"]).await;
        let got = drained(&mut second).await;
        assert!(
            got.iter()
                .all(|m| m.get("method").is_some() || m["id"] != json!(prompt)),
            "{got:?}"
        );
        assert!(got
            .iter()
            .any(|m| m["params"]["update"]["sessionUpdate"] == "usage_update"));
    }

    #[tokio::test]
    async fn approve_sees_a_question_first_sent_as_a_hotl_request() {
        let (handle, events) = injected();
        let (id, _task) = start(handle, std::time::Duration::ZERO);
        let mut ext_only = Client::attach(&id, json!({"_meta": {"hotl.dev/extensions": 1}})).await;
        let _answer = inject_question(&events).await;
        let first = ext_only.next().await.expect("the question");
        assert_eq!(first["method"], "_hotl/session/request_question");
        drop(ext_only);
        settle().await;

        let (read, write) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
        let mut client = hotl_tui::client::AcpClient::new(write);
        let mut lines = BufReader::new(read).lines();
        assert!(matches!(
            crate::attach::open(&mut client, &mut lines, "test", "tok").await,
            crate::attach::Opened::Attached
        ));
        let line = lines.next_line().await.unwrap().unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        assert!(
            matches!(
                hotl_tui::client::decode(&msg),
                Some(hotl_tui::client::ServerMsg::QuestionRequest { .. })
            ),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn the_attach_client_opens_with_the_token_and_is_closed_without_it() {
        let (id, _task) = start(scripted_session(), std::time::Duration::ZERO);
        for (token, attached) in [("tok", true), ("WRONG", false)] {
            let (read, write) = tokio::io::split(hotl_platform::IPC.connect(&id).await.unwrap());
            let mut client = hotl_tui::client::AcpClient::new(write);
            let mut lines = BufReader::new(read).lines();
            let opened = crate::attach::open(&mut client, &mut lines, "test", token).await;
            match opened {
                crate::attach::Opened::Attached => assert!(attached, "{token}"),
                crate::attach::Opened::Closed => assert!(!attached, "{token}"),
                crate::attach::Opened::Refused(e) => panic!("{token}: {e}"),
            }
        }
    }

    #[test]
    fn only_an_initialize_with_the_token_authenticates() {
        let init = |meta: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"_meta": meta}});
        assert!(initialize_carries(
            &init(json!({META_SOCKET_TOKEN: "tok"})),
            "tok"
        ));
        assert!(!initialize_carries(
            &init(json!({META_SOCKET_TOKEN: "nope"})),
            "tok"
        ));
        assert!(!initialize_carries(&init(json!({})), "tok"));
        assert!(!initialize_carries(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": {"_meta": {META_SOCKET_TOKEN: "tok"}}}),
            "tok"
        ));
    }
}
