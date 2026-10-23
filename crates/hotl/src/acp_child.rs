//! An external agent as a sub-agent, over standard ACP v1 (0077).
//!
//! hotl is the ACP *client* here: it starts the agent binary under the bash
//! sandbox floor, in the child's root, and translates what the agent says
//! into the same `EngineEvent`s a native child emits — so `spawn.rs` drives
//! it unchanged. Not hotl's own `hotl acp` dialect (that one is a server and
//! speaks `"0.1"`); nothing here is shared with it or with the TUI client.
//!
//! Every `session/request_permission` is answered by hotl's rules, never a
//! human ([`gate`]). What the agent does without asking is bounded by the
//! kernel floor, not by this module.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use hotl_engine::{EngineEvent, Outcome};
use hotl_tools::net::EgressState;
use hotl_tools::rules::Rules;
use hotl_tools::sandbox::SandboxStatus;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// The ACP major version this client speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// Each handshake step's leash.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// After `session/cancel`, how long the agent gets to answer the prompt
/// before its process group is killed (D8).
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// One JSON-RPC line, bounded like `hotl-mcp`'s.
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const STDERR_RING_LINES: usize = 32;
const STDERR_RING_BYTES: usize = 8 * 1024;
/// The engine's own event channel width.
const EVENTS_CAP: usize = 256;

/// The re-prompt an external agent gets when it ended without a valid block.
pub const ACP_REPROMPT: &str = "End your final message now with the one fenced ```json result \
     block your brief describes (outcome, summary, files_touched, commits, citations), with \
     what you have.";

/// Everything needed to start one external agent.
pub struct AcpSpawn {
    pub program: String,
    pub args: Vec<String>,
    /// The child's worktree, or the parent's cwd: the agent's `cwd` and the
    /// root hotl's gate classifies paths against.
    pub root: PathBuf,
    /// The agent's own home state, the only widening of its write floor.
    pub state_dirs: Vec<PathBuf>,
    /// Wall clock per prompt (D7).
    pub timeout: Duration,
    /// `session/set_mode` after `session/new`, when set.
    pub mode: Option<String>,
    /// The child's rules (forced `dontask`).
    pub rules: Arc<Rules>,
    /// The parent's hooks under this child's actor; deny-only here.
    pub hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
    pub sandbox_enforced: bool,
    pub sandbox: SandboxStatus,
    /// `None` resolves the session's egress at start, as `bash` does.
    pub egress: Option<EgressState>,
    /// Where the fenced report lands; `None` (the workflow runner) skips it.
    pub report_path: Option<PathBuf>,
    /// [`CANCEL_GRACE`], shortened by tests.
    pub grace: Duration,
}

impl AcpSpawn {
    /// The write roots this agent gets beyond the floor: its root and its
    /// state dirs. A state dir under a credential path is refused.
    fn extra_write(&self) -> Result<Vec<PathBuf>, String> {
        use hotl_platform::KnownPaths as _;
        let home = hotl_platform::KNOWN_PATHS.home().unwrap_or_default();
        let creds = hotl_tools::sandbox::credential_read_paths(&home);
        let mut out = vec![self.root.clone()];
        for dir in &self.state_dirs {
            let canon = dunce::canonicalize(dir).unwrap_or_else(|_| dir.clone());
            let refused = creds.iter().any(|c| {
                let c = dunce::canonicalize(c).unwrap_or_else(|_| c.clone());
                canon.starts_with(&c) || dir.starts_with(&c)
            });
            if refused {
                return Err(format!(
                    "`state_dirs: {}` is under a credential path hotl never opens to a \
                     sub-process",
                    dir.display()
                ));
            }
            out.push(canon);
        }
        Ok(out)
    }

    /// The command that starts the agent: the bash floor (`build_argv_with`,
    /// so the provider-key scrub applies), detached, cwd = the child's root.
    pub fn command(&self, egress: &EgressState) -> Result<tokio::process::Command, String> {
        let extra = self.extra_write()?;
        let mut cmd = hotl_tools::sandbox::build_argv_with(
            &self.program,
            &self.args,
            &self.sandbox,
            egress,
            &extra,
        );
        cmd.current_dir(&self.root);
        hotl_tools::sandbox::detach_session(&mut cmd);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        Ok(cmd)
    }
}

/// `program` as something that will exec: a path as given (`~/` expanded),
/// or the first executable match on `PATH`.
pub fn resolve_program(program: &str) -> Result<PathBuf, String> {
    use hotl_platform::KnownPaths as _;
    let given = match program.strip_prefix("~/") {
        Some(rest) => hotl_platform::KNOWN_PATHS
            .home()
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(program),
    };
    if given.components().count() > 1 {
        return match given.is_file() {
            true => Ok(given),
            false => Err(format!("`{program}` does not exist")),
        };
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| is_executable(p))
        .ok_or_else(|| format!("`{program}` is not on PATH"))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file() || p.with_extension("exe").is_file() || p.with_extension("cmd").is_file()
}

/// The tail of the agent's stderr, bounded in lines and bytes.
#[derive(Default)]
struct StderrRing {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrRing {
    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > STDERR_RING_LINES || self.bytes > STDERR_RING_BYTES {
            match self.lines.pop_front() {
                Some(d) => self.bytes -= d.len(),
                None => break,
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

type Writer = tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>;
type Streams = (
    Box<dyn AsyncRead + Send + Unpin>,
    Box<dyn AsyncWrite + Send + Unpin>,
);

/// What the reader, the prompt task and the handle share.
struct Shared {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    writer: Mutex<Option<Arc<Writer>>>,
    stderr: Mutex<StderrRing>,
    /// Why the connection died, once it has.
    dead: Mutex<Option<String>>,
    events: mpsc::Sender<EngineEvent>,
    session_id: Mutex<Option<String>>,
    /// The current prompt's `agent_message_chunk` text.
    text: Mutex<String>,
    /// Why the last report block was refused; prefixes the next re-prompt.
    report_error: Mutex<Option<String>>,
    /// Tool ids with a start and no settle yet.
    open_tools: Mutex<HashMap<String, String>>,
    /// Tool ids hotl denied: the agent's later update for one is dropped.
    denied: Mutex<HashSet<String>>,
    /// The live prompt's cancel, set by `interrupt`.
    turn: Mutex<Option<CancellationToken>>,
    reaper: Mutex<Option<hotl_platform::process::ActiveReaper>>,
    last_cost: Mutex<Option<String>>,
    reports: AtomicU64,
    root: PathBuf,
    rules: Arc<Rules>,
    hooks: Option<Arc<dyn hotl_engine::hooks::Hooks>>,
    sandbox_enforced: bool,
}

impl Shared {
    fn stderr_tail(&self) -> String {
        lock(&self.stderr)
            .lines
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn die(&self, reason: String) {
        lock(&self.dead).get_or_insert(reason);
        lock(&self.pending).clear();
    }

    /// The death reason with the agent's own last words, after one short
    /// wait for the stderr drain to catch up.
    async fn death_note(&self, fallback: &str) -> String {
        if self.stderr_tail().is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let reason = lock(&self.dead)
            .clone()
            .unwrap_or_else(|| fallback.to_string());
        self.with_tail(reason)
    }

    fn with_tail(&self, reason: String) -> String {
        match self.stderr_tail() {
            t if t.is_empty() => reason,
            t => format!("{reason}\nagent stderr:\n{t}"),
        }
    }

    async fn send(&self, msg: &Value) -> Result<(), String> {
        let Some(writer) = lock(&self.writer).clone() else {
            return Err("the agent is not running".into());
        };
        let mut line = msg.to_string();
        line.push('\n');
        let write = async {
            let mut w = writer.lock().await;
            w.write_all(line.as_bytes())
                .await
                .map_err(|e| format!("agent pipe closed: {e}"))?;
            w.flush()
                .await
                .map_err(|e| format!("agent pipe closed: {e}"))
        };
        match tokio::time::timeout(WRITE_TIMEOUT, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.die(e.clone());
                Err(self.death_note(&e).await)
            }
            Err(_) => {
                let e = "the agent stopped reading its stdin".to_string();
                self.die(e.clone());
                Err(e)
            }
        }
    }

    /// Issue a request; the receiver resolves with the whole reply message.
    async fn issue(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(u64, oneshot::Receiver<Value>), String> {
        if lock(&self.dead).is_some() {
            return Err(self.death_note("the agent is gone").await);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if let Err(e) = self
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await
        {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    /// A request answered within `leash`, its `result` or its error as text.
    async fn request(&self, method: &str, params: Value, leash: Duration) -> Result<Value, String> {
        let (id, rx) = self.issue(method, params).await?;
        match tokio::time::timeout(leash, rx).await {
            Ok(Ok(reply)) => reply_result(method, reply).map_err(|e| self.with_tail(e)),
            Ok(Err(_)) => Err(self.death_note("the agent closed its output").await),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(self.with_tail(format!(
                    "`{method}` got no answer within {}s",
                    leash.as_secs()
                )))
            }
        }
    }

    async fn emit(&self, event: EngineEvent) {
        let _ = self.events.send(event).await;
    }

    fn kill(&self) {
        use hotl_platform::TreeReaper as _;
        if let Some(r) = lock(&self.reaper).as_ref() {
            let _ = r.kill_tree();
        }
        self.die("the agent was stopped".into());
    }
}

fn reply_result(method: &str, reply: Value) -> Result<Value, String> {
    if let Some(err) = reply.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(format!("the agent refused `{method}`: {msg}"));
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

/// One external agent, driven like a native child. Created unstarted;
/// [`AcpChild::start`] runs the process and the handshake.
pub struct AcpChild {
    spec: AcpSpawn,
    shared: Arc<Shared>,
    events: mpsc::Receiver<EngineEvent>,
    /// Pre-supplied streams (tests): no process is started.
    streams: Mutex<Option<Streams>>,
    process: Option<tokio::process::Child>,
}

impl AcpChild {
    /// Unstarted: nothing runs until [`Self::start`].
    pub fn new(spec: AcpSpawn) -> Self {
        let (tx, rx) = mpsc::channel(EVENTS_CAP);
        let shared = Arc::new(Shared {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            writer: Mutex::new(None),
            stderr: Mutex::new(StderrRing::default()),
            dead: Mutex::new(None),
            events: tx,
            session_id: Mutex::new(None),
            text: Mutex::new(String::new()),
            report_error: Mutex::new(None),
            open_tools: Mutex::new(HashMap::new()),
            denied: Mutex::new(HashSet::new()),
            turn: Mutex::new(None),
            reaper: Mutex::new(None),
            last_cost: Mutex::new(None),
            reports: AtomicU64::new(0),
            root: spec.root.clone(),
            rules: spec.rules.clone(),
            hooks: spec.hooks.clone(),
            sandbox_enforced: spec.sandbox_enforced,
        });
        Self {
            spec,
            shared,
            events: rx,
            streams: Mutex::new(None),
            process: None,
        }
    }

    /// Over arbitrary streams (tests use duplex pipes): no process.
    #[cfg_attr(not(test), allow(dead_code))] // test and process-test seam
    pub fn from_streams(
        read: impl AsyncRead + Send + Unpin + 'static,
        write: impl AsyncWrite + Send + Unpin + 'static,
        spec: AcpSpawn,
    ) -> Self {
        let child = Self::new(spec);
        *lock(&child.streams) = Some((Box::new(read), Box::new(write)));
        child
    }

    /// Start and handshake in one call.
    #[allow(dead_code)] // the process test's seam (tests/acp_subagent.rs)
    pub async fn spawn(spec: AcpSpawn) -> Result<Self, String> {
        let mut child = Self::new(spec);
        child.start().await?;
        Ok(child)
    }

    pub fn events(&mut self) -> &mut mpsc::Receiver<EngineEvent> {
        &mut self.events
    }

    /// The agent's process id, while it runs.
    #[allow(dead_code)] // the process test's seam (tests/acp_subagent.rs)
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().and_then(tokio::process::Child::id)
    }

    /// Run the process (unless streams were supplied), then `initialize` →
    /// `session/new` → optional `session/set_mode`.
    pub async fn start(&mut self) -> Result<(), String> {
        let supplied = lock(&self.streams).take();
        let (read, write): Streams = match supplied {
            Some(s) => s,
            None => {
                let egress = match &self.spec.egress {
                    Some(e) => e.clone(),
                    None => hotl_tools::net::egress_state().await,
                };
                let mut cmd = self.spec.command(&egress)?;
                let mut process = cmd
                    .spawn()
                    .map_err(|e| format!("could not start `{}`: {e}", self.spec.program))?;
                {
                    use hotl_platform::{ProcessControl as _, TreeReaper as _};
                    let reaper = hotl_platform::PROCESS_CONTROL.new_reaper().ok();
                    if let Some(r) = &reaper {
                        let _ = r.adopt(&process);
                    }
                    *lock(&self.shared.reaper) = reaper;
                }
                let stdin = process.stdin.take().ok_or("the agent has no stdin")?;
                let stdout = process.stdout.take().ok_or("the agent has no stdout")?;
                if let Some(stderr) = process.stderr.take() {
                    tokio::spawn(stderr_task(stderr, Arc::downgrade(&self.shared)));
                }
                self.process = Some(process);
                (Box::new(stdout), Box::new(stdin))
            }
        };
        *lock(&self.shared.writer) = Some(Arc::new(tokio::sync::Mutex::new(write)));
        tokio::spawn(reader_task(read, Arc::downgrade(&self.shared)));

        let init = self
            .shared
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    // D3: the agent uses its own file and shell access, inside
                    // the floor; hotl serves neither.
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false,
                    },
                    "clientInfo": {"name": "hotl", "version": env!("CARGO_PKG_VERSION")},
                }),
                HANDSHAKE_TIMEOUT,
            )
            .await?;
        match init.get("protocolVersion").and_then(Value::as_u64) {
            Some(PROTOCOL_VERSION) => {}
            other => {
                let n = other.map_or_else(|| "?".to_string(), |n| n.to_string());
                return Err(self.shared.with_tail(format!(
                    "the agent speaks ACP v{n}, hotl speaks v{PROTOCOL_VERSION}"
                )));
            }
        }
        let new = self
            .shared
            .request(
                "session/new",
                json!({"cwd": self.spec.root, "mcpServers": []}),
                HANDSHAKE_TIMEOUT,
            )
            .await?;
        let Some(sid) = new.get("sessionId").and_then(Value::as_str) else {
            return Err(self
                .shared
                .with_tail("the agent's `session/new` reply has no sessionId".into()));
        };
        *lock(&self.shared.session_id) = Some(sid.to_string());
        if let Some(mode) = &self.spec.mode {
            let offered: Vec<&str> = new
                .pointer("/modes/availableModes")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|m| m.get("id")?.as_str()).collect())
                .unwrap_or_default();
            if !offered.contains(&mode.as_str()) {
                return Err(format!(
                    "the agent does not offer mode `{mode}` (it offers: {})",
                    match offered.is_empty() {
                        true => "none".to_string(),
                        false => offered.join(", "),
                    }
                ));
            }
            self.shared
                .request(
                    "session/set_mode",
                    json!({"sessionId": sid, "modeId": mode}),
                    HANDSHAKE_TIMEOUT,
                )
                .await?;
        }
        Ok(())
    }

    /// Send one prompt; its turn settles as a `TurnDone` on the events.
    /// spawn.rs's native re-prompt and wrap-up texts name `report_result`,
    /// which this agent does not have, so they are rewritten here.
    pub fn prompt(&self, text: String) {
        let text = if text == hotl_tools::report_tool::REPROMPT {
            match lock(&self.shared.report_error).take() {
                Some(e) => format!("Your result block was refused: {e}\n{ACP_REPROMPT}"),
                None => ACP_REPROMPT.to_string(),
            }
        } else if text.starts_with("Turn budget exhausted.") {
            format!("Turn budget exhausted. {ACP_REPROMPT} Do not start new work.")
        } else {
            text
        };
        let cancel = CancellationToken::new();
        *lock(&self.shared.turn) = Some(cancel.clone());
        tokio::spawn(run_prompt(
            self.shared.clone(),
            text,
            cancel,
            self.spec.timeout,
            self.spec.grace,
            self.spec.report_path.clone(),
        ));
    }

    /// An external child has nothing to continue (D7 refuses `fork`); the
    /// turn ends at once rather than leaving a drain waiting.
    pub fn continue_turn(&self) {
        let shared = self.shared.clone();
        tokio::spawn(async move {
            shared
                .emit(EngineEvent::TurnDone {
                    outcome: Outcome::Error {
                        message: "an external agent has no turn to continue".into(),
                    },
                    usage: Default::default(),
                    mispredictions: 0,
                })
                .await;
        });
    }

    /// `session/cancel` for the live prompt, then the grace, then the kill.
    pub fn interrupt(&self) {
        if let Some(t) = lock(&self.shared.turn).as_ref() {
            t.cancel();
        }
    }

    /// Stop the agent: cancel any prompt, give it `grace` to exit, then
    /// kill its process group.
    pub async fn finish(mut self, grace: Duration) {
        self.interrupt();
        let sid = lock(&self.shared.session_id).clone();
        if let Some(sid) = sid {
            let _ = self
                .shared
                .send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}))
                .await;
        }
        *lock(&self.shared.writer) = None;
        if let Some(p) = self.process.as_mut() {
            let _ = tokio::time::timeout(grace, p.wait()).await;
        }
        self.shared.kill();
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        if let Some(t) = lock(&self.shared.turn).as_ref() {
            t.cancel();
        }
        self.shared.kill();
    }
}

/// One `session/prompt`, to its `TurnDone`.
async fn run_prompt(
    shared: Arc<Shared>,
    text: String,
    cancel: CancellationToken,
    timeout: Duration,
    grace: Duration,
    report_path: Option<PathBuf>,
) {
    lock(&shared.text).clear();
    let outcome = prompt_outcome(
        &shared,
        text,
        &cancel,
        timeout,
        grace,
        report_path.as_deref(),
    )
    .await;
    // A tool the agent never settled settles failed, so the band row closes.
    let open: Vec<(String, String)> = lock(&shared.open_tools).drain().collect();
    for (id, name) in open {
        shared
            .emit(EngineEvent::ToolDone {
                id,
                name,
                ok: false,
                lines: 0,
                bytes: 0,
            })
            .await;
    }
    shared
        .emit(EngineEvent::TurnDone {
            outcome,
            // D6: an external agent's spend is not hotl's to count.
            usage: Default::default(),
            mispredictions: 0,
        })
        .await;
}

async fn prompt_outcome(
    shared: &Arc<Shared>,
    text: String,
    cancel: &CancellationToken,
    timeout: Duration,
    grace: Duration,
    report_path: Option<&Path>,
) -> Outcome {
    let Some(sid) = lock(&shared.session_id).clone() else {
        return Outcome::Error {
            message: shared.with_tail("the agent never completed its handshake".into()),
        };
    };
    let (_, mut rx) = match shared
        .issue(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Outcome::Error { message: e },
    };
    enum Stop {
        Cancelled,
        TimedOut,
    }
    let stop = tokio::select! {
        biased;
        _ = cancel.cancelled() => Stop::Cancelled,
        _ = tokio::time::sleep(timeout) => Stop::TimedOut,
        r = &mut rx => match r {
            Ok(reply) => return settle(shared, reply, report_path).await,
            Err(_) => {
                return Outcome::Error {
                    message: shared.death_note("the agent closed its output").await,
                }
            }
        },
    };
    // D8: ask, wait the grace for the prompt's answer, then kill.
    let _ = shared
        .send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}))
        .await;
    if tokio::time::timeout(grace, &mut rx).await.is_err() {
        shared.kill();
    }
    match stop {
        Stop::Cancelled => Outcome::Cancelled,
        Stop::TimedOut => Outcome::Error {
            message: format!("timed out after {}s", timeout.as_secs()),
        },
    }
}

/// A `session/prompt` reply as the turn's outcome.
async fn settle(shared: &Arc<Shared>, reply: Value, report_path: Option<&Path>) -> Outcome {
    let result = match reply_result("session/prompt", reply) {
        Ok(r) => r,
        Err(e) => {
            return Outcome::Error {
                message: shared.with_tail(e),
            }
        }
    };
    let text = lock(&shared.text).clone();
    match result.get("stopReason").and_then(Value::as_str) {
        Some("end_turn") => {
            if let Some(path) = report_path {
                record_report(shared, &text, path).await;
            }
            Outcome::Done { text }
        }
        Some("cancelled") => Outcome::Cancelled,
        Some("refusal") => Outcome::Refused,
        Some("max_tokens") | Some("max_turn_requests") => Outcome::TurnLimit,
        other => Outcome::Error {
            message: format!("the agent ended its turn with stopReason {other:?}"),
        },
    }
}

/// D4: the last fenced block becomes `RESPONSE.json`, announced as the same
/// `report_result` pair a native child's tool call makes.
async fn record_report(shared: &Arc<Shared>, text: &str, path: &Path) {
    let n = shared.reports.fetch_add(1, Ordering::Relaxed);
    let id = format!("acp-report-{n}");
    let (ok, summary) = match hotl_tools::report_tool::extract_fenced_report(text) {
        None => {
            *lock(&shared.report_error) =
                Some("your final message had no fenced ```json block.".into());
            return;
        }
        Some(Err(e)) => {
            // The validator speaks to a tool caller; this agent has a block.
            *lock(&shared.report_error) =
                Some(e.replace("call report_result again", "send the block again"));
            (false, "report_result (invalid)".to_string())
        }
        Some(Ok(v)) => match write_response(path, &v) {
            Ok(()) => {
                *lock(&shared.report_error) = None;
                let outcome = v.get("outcome").and_then(Value::as_str).unwrap_or("?");
                (true, format!("report_result ({outcome})"))
            }
            Err(e) => {
                *lock(&shared.report_error) = Some(format!("hotl could not record it: {e}"));
                (false, "report_result (unrecorded)".to_string())
            }
        },
    };
    shared
        .emit(EngineEvent::ToolStart {
            id: id.clone(),
            name: "report_result".into(),
            summary,
        })
        .await;
    shared
        .emit(EngineEvent::ToolDone {
            id,
            name: "report_result".into(),
            ok,
            lines: 0,
            bytes: 0,
        })
        .await;
}

fn write_response(path: &Path, v: &Value) -> std::io::Result<()> {
    use hotl_platform::PrivateFs as _;
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        hotl_platform::PRIVATE_FS.create_dir_all(parent)?;
    }
    let mut f = hotl_platform::PRIVATE_FS.create_file_truncate(path)?;
    f.write_all(
        serde_json::to_string_pretty(v)
            .unwrap_or_default()
            .as_bytes(),
    )
}

async fn stderr_task(read: impl AsyncRead + Send + Unpin, shared: Weak<Shared>) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let line = hotl_types::sanitize::safe_summary(&line);
        if !line.is_empty() {
            lock(&shared.stderr).push(line);
        }
    }
}

async fn reader_task(read: impl AsyncRead + Send + Unpin, shared: Weak<Shared>) {
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let mut limited = (&mut reader).take(MAX_LINE_BYTES);
        let read = limited.read_until(b'\n', &mut buf).await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match read {
            Ok(0) => {
                shared.die("the agent closed its output".into());
                return;
            }
            Ok(_) if buf.last() != Some(&b'\n') && buf.len() as u64 >= MAX_LINE_BYTES => {
                shared.die(format!(
                    "the agent sent a message over {MAX_LINE_BYTES} bytes with no newline"
                ));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                shared.die(format!("read error from the agent: {e}"));
                return;
            }
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        dispatch(&shared, msg).await;
    }
}

async fn dispatch(shared: &Arc<Shared>, msg: Value) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned();
    match (method, id) {
        // A reply to one of ours.
        (None, Some(id)) => {
            if let Some(n) = id.as_u64() {
                if let Some(tx) = lock(&shared.pending).remove(&n) {
                    let _ = tx.send(msg);
                }
            }
        }
        (Some("session/update"), _) => {
            if let Some(update) = msg.pointer("/params/update") {
                on_update(shared, update).await;
            }
        }
        (Some("session/request_permission"), Some(id)) => {
            // Answered off the reader so a slow hook never stalls the pipe.
            let shared = shared.clone();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            tokio::spawn(async move {
                let d = gate::decide(
                    &shared.rules,
                    shared.hooks.as_deref(),
                    &params,
                    &shared.root,
                    shared.sandbox_enforced,
                )
                .await;
                if !d.allowed {
                    let tool_id = params
                        .pointer("/toolCall/toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("acp-denied")
                        .to_string();
                    lock(&shared.open_tools).remove(&tool_id);
                    lock(&shared.denied).insert(tool_id.clone());
                    shared
                        .emit(EngineEvent::ToolDenied {
                            id: tool_id,
                            name: d.tool.clone(),
                        })
                        .await;
                }
                let _ = shared
                    .send(&json!({"jsonrpc": "2.0", "id": id, "result": d.reply()}))
                    .await;
            });
        }
        // fs/*, terminal/*, elicitation: hotl advertised none of them.
        (Some(m), Some(id)) => {
            let _ = shared
                .send(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("hotl does not serve `{m}`")},
                }))
                .await;
        }
        _ => {}
    }
}

fn text_of(content: Option<&Value>) -> Option<String> {
    let c = content?;
    match c.get("type").and_then(Value::as_str) {
        Some("text") => c.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

async fn on_update(shared: &Arc<Shared>, u: &Value) {
    let s = |k: &str| u.get(k).and_then(Value::as_str);
    match s("sessionUpdate") {
        Some("agent_message_chunk") => {
            if let Some(t) = text_of(u.get("content")) {
                lock(&shared.text).push_str(&t);
                shared.emit(EngineEvent::TextDelta(t)).await;
            }
        }
        Some("agent_thought_chunk") => {
            if let Some(t) = text_of(u.get("content")) {
                shared.emit(EngineEvent::ThinkingDelta(t)).await;
            }
        }
        Some("tool_call") => {
            let Some(id) = s("toolCallId").map(str::to_string) else {
                return;
            };
            let name = tool_name(u);
            let summary = s("title")
                .map(str::to_string)
                .unwrap_or_else(|| name.clone());
            lock(&shared.open_tools).insert(id.clone(), name.clone());
            shared
                .emit(EngineEvent::ToolStart {
                    id: id.clone(),
                    name: name.clone(),
                    summary: hotl_types::sanitize::safe_summary(&summary),
                })
                .await;
            if let Some(ok) = settled(s("status")) {
                lock(&shared.open_tools).remove(&id);
                shared.emit(tool_done(id, name, ok)).await;
            }
        }
        Some("tool_call_update") => {
            let Some(id) = s("toolCallId").map(str::to_string) else {
                return;
            };
            let Some(ok) = settled(s("status")) else {
                return;
            };
            if lock(&shared.denied).contains(&id) {
                return;
            }
            let opened = lock(&shared.open_tools).remove(&id);
            let name = opened.clone().unwrap_or_else(|| tool_name(u));
            // An update for a call never announced is a start and a settle in one.
            if opened.is_none() {
                let summary = s("title")
                    .map(str::to_string)
                    .unwrap_or_else(|| name.clone());
                shared
                    .emit(EngineEvent::ToolStart {
                        id: id.clone(),
                        name: name.clone(),
                        summary: hotl_types::sanitize::safe_summary(&summary),
                    })
                    .await;
            }
            shared.emit(tool_done(id, name, ok)).await;
        }
        Some("usage_update") => {
            // D6: informational only, never folded into the cap.
            let Some(cost) = u.get("cost") else { return };
            let amount = cost
                .get("amount")
                .map(|a| a.to_string())
                .unwrap_or_default();
            let currency = cost.get("currency").and_then(Value::as_str).unwrap_or("");
            let line = format!("spend reported by the agent: {amount} {currency}");
            let fresh = {
                let mut last = lock(&shared.last_cost);
                let fresh = last.as_deref() != Some(line.as_str());
                *last = Some(line.clone());
                fresh
            };
            if fresh {
                shared
                    .emit(EngineEvent::TextDelta(format!("\n[{}]\n", line.trim())))
                    .await;
            }
        }
        _ => {}
    }
}

fn tool_name(u: &Value) -> String {
    u.get("name")
        .or_else(|| u.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string()
}

fn settled(status: Option<&str>) -> Option<bool> {
    match status {
        Some("completed") => Some(true),
        Some("failed") => Some(false),
        _ => None,
    }
}

fn tool_done(id: String, name: String, ok: bool) -> EngineEvent {
    EngineEvent::ToolDone {
        id,
        name,
        ok,
        lines: 0,
        bytes: 0,
    }
}

/// D3: an agent's permission request, decided by hotl's own rules.
pub mod gate {
    use std::path::Path;

    use hotl_engine::hooks::{Hooks, PreToolDecision};
    use hotl_tools::rules::{CallFacts, PermissionMode, Rules, Verdict};
    use hotl_tools::{Permission, Tool as _};
    use serde_json::{json, Value};

    /// The hotl tool and input an ACP tool call is judged as.
    #[derive(Debug, Clone, PartialEq)]
    pub struct GateInput {
        pub tool: &'static str,
        pub input: Value,
    }

    /// What hotl answers.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Decision {
        /// `None` is the `cancelled` outcome: hotl wanted to reject and no
        /// `reject_once` option was offered.
        pub option_id: Option<String>,
        pub allowed: bool,
        /// The hotl tool it was judged as (`unknown` when untranslatable).
        pub tool: String,
    }

    impl Decision {
        pub fn reply(&self) -> Value {
            match &self.option_id {
                Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
                None => json!({"outcome": {"outcome": "cancelled"}}),
            }
        }
    }

    fn path_of(tc: &Value) -> Option<String> {
        tc.pointer("/locations/0/path")
            .and_then(Value::as_str)
            .or_else(|| {
                tc.get("content")?
                    .as_array()?
                    .iter()
                    .find(|c| c.get("type").and_then(Value::as_str) == Some("diff"))?
                    .get("path")?
                    .as_str()
            })
            .or_else(|| tc.pointer("/rawInput/path").and_then(Value::as_str))
            .or_else(|| tc.pointer("/rawInput/file_path").and_then(Value::as_str))
            .map(str::to_string)
    }

    fn command_of(tc: &Value) -> Option<String> {
        let raw = tc.get("rawInput");
        let from = |v: Option<&Value>| match v? {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => {
                let parts: Option<Vec<&str>> = a.iter().map(Value::as_str).collect();
                parts.map(|p| p.join(" "))
            }
            _ => None,
        };
        from(raw.and_then(|r| r.get("command")))
            .or_else(|| from(raw.and_then(|r| r.get("cmd"))))
            .or_else(|| tc.get("title").and_then(Value::as_str).map(str::to_string))
            .filter(|c| !c.trim().is_empty())
    }

    /// An ACP `toolCall` as a hotl call, or `None` when it cannot be judged
    /// (an unknown kind, or one whose input is not where D3 looks).
    pub fn translate(tc: &Value) -> Option<GateInput> {
        let kind = tc.get("kind").and_then(Value::as_str)?;
        let (tool, input) = match kind {
            "execute" => ("bash", json!({"command": command_of(tc)?})),
            "edit" => ("edit", json!({"path": path_of(tc)?})),
            "delete" | "move" => ("write", json!({"path": path_of(tc)?})),
            "read" => ("read", json!({"path": path_of(tc)?})),
            "search" => ("grep", json!({"path": path_of(tc)?})),
            "fetch" => {
                let url = tc
                    .pointer("/rawInput/url")
                    .and_then(Value::as_str)
                    .or_else(|| tc.get("title").and_then(Value::as_str))
                    .filter(|u| u.starts_with("http://") || u.starts_with("https://"))?;
                ("web_fetch", json!({"urls": [url]}))
            }
            _ => return None,
        };
        Some(GateInput { tool, input })
    }

    /// The permission hotl's own tool would ask for this call, rooted at the
    /// child's root — so "outside the root" means what it means natively.
    fn permission(root: &Path, g: &GateInput) -> (Permission, bool, bool) {
        let tools = hotl_tools::Registry::builtin_with_root(
            Default::default(),
            Default::default(),
            hotl_tools::MinifyConfig::default(),
            std::sync::Arc::from(root),
        );
        match g.tool {
            "web_fetch" => {
                let t = hotl_tools::web::WebFetchTool::new(Default::default());
                (t.permission(&g.input), t.read_only(), t.edits_files())
            }
            // A search is placed like a read of its path: native `grep` asks
            // nothing, but an agent's search outside the root is a read there.
            "grep" => {
                let t = tools.get("read").expect("read is a builtin");
                (t.permission(&g.input), true, false)
            }
            name => {
                let t = tools.get(name).expect("translate names builtins only");
                (t.permission(&g.input), t.read_only(), t.edits_files())
            }
        }
    }

    /// Whether hotl's rules (and the deny-only hooks) let this call through.
    pub async fn allowed(
        rules: &Rules,
        hooks: Option<&dyn Hooks>,
        g: &GateInput,
        root: &Path,
        sandbox_enforced: bool,
    ) -> bool {
        if rules.denied(g.tool, &g.input).is_some() {
            return false;
        }
        let (perm, read_only, edits_files) = permission(root, g);
        let by_rules = match perm {
            Permission::None => true,
            Permission::Ask { .. } => matches!(
                rules.evaluate(
                    PermissionMode::DontAsk,
                    false,
                    g.tool,
                    &g.input,
                    CallFacts {
                        sandbox_enforced,
                        protected: None,
                        read_only,
                        edits_files
                    }
                ),
                Verdict::Auto { .. }
            ),
            Permission::AskProtected { class, .. } => matches!(
                rules.evaluate(
                    PermissionMode::DontAsk,
                    false,
                    g.tool,
                    &g.input,
                    CallFacts {
                        sandbox_enforced,
                        protected: Some(class),
                        read_only,
                        edits_files
                    }
                ),
                Verdict::Auto { .. }
            ),
        };
        if !by_rules {
            return false;
        }
        // After the rules, deny-only: a rewrite cannot be applied to a call
        // the agent makes itself, so it rejects too.
        match hooks {
            Some(h) => matches!(
                h.pre_tool("", g.tool, &g.input).await,
                PreToolDecision::Continue
            ),
            None => true,
        }
    }

    fn option(req: &Value, kind: &str) -> Option<String> {
        req.get("options")?
            .as_array()?
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))?
            .get("optionId")?
            .as_str()
            .map(str::to_string)
    }

    /// Decide one `session/request_permission`. Never `allow_always` or
    /// `reject_always`: hotl persists no grant (the rules.rs rule).
    pub async fn decide(
        rules: &Rules,
        hooks: Option<&dyn Hooks>,
        req: &Value,
        root: &Path,
        sandbox_enforced: bool,
    ) -> Decision {
        let tc = req.get("toolCall").cloned().unwrap_or(Value::Null);
        let g = translate(&tc);
        let tool = g.as_ref().map_or("unknown", |g| g.tool).to_string();
        let allow = match &g {
            Some(g) => allowed(rules, hooks, g, root, sandbox_enforced).await,
            None => false,
        };
        if allow {
            if let Some(id) = option(req, "allow_once") {
                return Decision {
                    option_id: Some(id),
                    allowed: true,
                    tool,
                };
            }
        }
        Decision {
            option_id: option(req, "reject_once"),
            allowed: false,
            tool,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    pub(crate) type Handler = Box<dyn FnMut(&Value) -> Vec<Value> + Send>;

    /// A scripted ACP agent on the far end of a duplex pipe: every message
    /// it receives is recorded, and `handler` answers it. `None` from the
    /// handler's caller-side `close` ends the pipe.
    pub(crate) struct Fake {
        seen: Arc<Mutex<Vec<Value>>>,
        close: Option<oneshot::Sender<()>>,
    }

    impl Fake {
        pub(crate) fn seen(&self) -> Vec<Value> {
            lock(&self.seen).clone()
        }
        pub(crate) fn methods(&self) -> Vec<String> {
            self.seen()
                .iter()
                .filter_map(|m| m.get("method").and_then(Value::as_str).map(str::to_string))
                .collect()
        }
        fn hang_up(&mut self) {
            if let Some(c) = self.close.take() {
                let _ = c.send(());
            }
        }
    }

    /// Put a line in the agent's stderr tail, as a dying agent would.
    pub(crate) fn note_stderr(child: &AcpChild, line: &str) {
        lock(&child.shared.stderr).push(line.into());
    }

    pub(crate) fn reply(msg: &Value, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": msg["id"], "result": result})
    }

    pub(crate) fn update(u: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}})
    }

    pub(crate) fn chunk(text: &str) -> Value {
        update(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        )
    }

    /// `initialize` and `session/new` answered the conformant way.
    pub(crate) fn handshake(msg: &Value) -> Option<Vec<Value>> {
        match msg.get("method").and_then(Value::as_str)? {
            "initialize" => Some(vec![reply(
                msg,
                json!({"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}),
            )]),
            "session/new" => Some(vec![reply(msg, json!({"sessionId": "s1"}))]),
            _ => None,
        }
    }

    pub(crate) fn fake(mut handler: Handler) -> (Fake, tokio::io::DuplexStream) {
        let (client, agent) = tokio::io::duplex(1 << 20);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (close, mut closed) = oneshot::channel::<()>();
        let rec = seen.clone();
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(agent);
            let mut lines = BufReader::new(r).lines();
            loop {
                tokio::select! {
                    _ = &mut closed => return,
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { return };
                        let msg: Value = serde_json::from_str(&line).unwrap();
                        lock(&rec).push(msg.clone());
                        for out in handler(&msg) {
                            let mut l = out.to_string();
                            l.push('\n');
                            if w.write_all(l.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        (
            Fake {
                seen,
                close: Some(close),
            },
            client,
        )
    }

    pub(crate) fn spec(root: &Path, report: Option<PathBuf>) -> AcpSpawn {
        AcpSpawn {
            program: "fake-agent".into(),
            args: Vec::new(),
            root: root.to_path_buf(),
            state_dirs: Vec::new(),
            timeout: Duration::from_secs(30),
            mode: None,
            rules: Arc::new(Rules::default()),
            hooks: None,
            sandbox_enforced: false,
            sandbox: SandboxStatus::Disabled,
            egress: Some(EgressState::Open),
            report_path: report,
            grace: Duration::from_millis(200),
        }
    }

    async fn connect(handler: Handler, spec: AcpSpawn) -> (AcpChild, Fake, Result<(), String>) {
        let (fake, client) = fake(handler);
        let (r, w) = tokio::io::split(client);
        let mut child = AcpChild::from_streams(r, w, spec);
        let started = child.start().await;
        (child, fake, started)
    }

    /// Every event up to and including the turn's `TurnDone`.
    async fn turn(child: &mut AcpChild) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(10), child.events().recv())
                .await
                .expect("turn timed out")
                .expect("events closed");
            let done = matches!(ev, EngineEvent::TurnDone { .. });
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    fn outcome(events: &[EngineEvent]) -> Outcome {
        match events.last() {
            Some(EngineEvent::TurnDone { outcome, usage, .. }) => {
                assert_eq!(*usage, Default::default(), "D6: never a usage");
                outcome.clone()
            }
            other => panic!("no TurnDone: {other:?}"),
        }
    }

    pub(crate) fn method(msg: &Value) -> &str {
        msg.get("method").and_then(Value::as_str).unwrap_or("")
    }

    /// A prompt answered with `chunks` then `stopReason`.
    fn answering(chunks: Vec<Value>, stop: &'static str) -> Handler {
        Box::new(move |m| {
            handshake(m).unwrap_or_else(|| match method(m) {
                "session/prompt" => {
                    let mut out = chunks.clone();
                    out.push(reply(m, json!({"stopReason": stop})));
                    out
                }
                _ => Vec::new(),
            })
        })
    }

    #[tokio::test]
    async fn initialize_sends_v1_and_no_fs_or_terminal_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let (_c, fake, started) =
            connect(answering(vec![], "end_turn"), spec(dir.path(), None)).await;
        started.unwrap();
        let init = &fake.seen()[0];
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], 1);
        assert_eq!(
            init["params"]["clientCapabilities"],
            json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
        );
        assert_eq!(init["params"]["clientInfo"]["name"], "hotl");
    }

    #[tokio::test]
    async fn a_v2_only_agent_is_refused_before_session_new() {
        let dir = tempfile::tempdir().unwrap();
        let handler: Handler = Box::new(|m| match method(m) {
            "initialize" => vec![reply(m, json!({"protocolVersion": 2}))],
            _ => handshake(m).unwrap_or_default(),
        });
        let (_c, fake, started) = connect(handler, spec(dir.path(), None)).await;
        let err = started.unwrap_err();
        assert!(err.contains("ACP v2, hotl speaks v1"), "{err}");
        assert_eq!(fake.methods(), vec!["initialize"]);
    }

    #[tokio::test]
    async fn session_new_carries_the_child_root_as_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let (_c, fake, started) =
            connect(answering(vec![], "end_turn"), spec(dir.path(), None)).await;
        started.unwrap();
        let new = fake
            .seen()
            .into_iter()
            .find(|m| method(m) == "session/new")
            .unwrap();
        assert_eq!(new["params"]["cwd"], json!(dir.path()));
        assert_eq!(new["params"]["mcpServers"], json!([]));
    }

    #[tokio::test]
    async fn a_mode_the_agent_does_not_offer_fails_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let handler: Handler = Box::new(|m| match method(m) {
            "session/new" => vec![reply(
                m,
                json!({"sessionId": "s1", "modes": {"currentModeId": "auto", "availableModes": [{"id": "auto"}, {"id": "ask"}]}}),
            )],
            "session/set_mode" => vec![reply(m, json!({}))],
            _ => handshake(m).unwrap_or_default(),
        });
        let mut s = spec(dir.path(), None);
        s.mode = Some("ask".into());
        let (_c, fake, started) = connect(handler, s).await;
        started.unwrap();
        let set = fake
            .seen()
            .into_iter()
            .find(|m| method(m) == "session/set_mode")
            .unwrap();
        assert_eq!(set["params"], json!({"sessionId": "s1", "modeId": "ask"}));

        let handler: Handler = Box::new(|m| handshake(m).unwrap_or_default());
        let mut s = spec(dir.path(), None);
        s.mode = Some("ask".into());
        let (_c, _f, started) = connect(handler, s).await;
        assert!(started.unwrap_err().contains("does not offer mode `ask`"));
    }

    #[tokio::test]
    async fn message_and_thought_chunks_become_text_deltas_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let chunks = vec![
            update(
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
            ),
            chunk("one "),
            update(json!({"sessionUpdate": "plan", "entries": []})),
            chunk("two"),
        ];
        let (mut c, _f, started) =
            connect(answering(chunks, "end_turn"), spec(dir.path(), None)).await;
        started.unwrap();
        c.prompt("go".into());
        let events = turn(&mut c).await;
        let texts: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::TextDelta(t) => Some(format!("t:{t}")),
                EngineEvent::ThinkingDelta(t) => Some(format!("th:{t}")),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["th:hmm", "t:one ", "t:two"]);
        assert_eq!(
            outcome(&events),
            Outcome::Done {
                text: "one two".into()
            }
        );
    }

    #[tokio::test]
    async fn tool_call_and_update_become_start_and_done_with_the_title_as_summary() {
        let dir = tempfile::tempdir().unwrap();
        let chunks = vec![
            update(
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Reading lib.rs", "kind": "read", "status": "pending"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "in_progress"}),
            ),
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}),
            ),
            // Never announced: a start and a settle in one.
            update(
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t2", "kind": "execute", "title": "ls", "status": "failed"}),
            ),
        ];
        let (mut c, _f, started) =
            connect(answering(chunks, "end_turn"), spec(dir.path(), None)).await;
        started.unwrap();
        c.prompt("go".into());
        let events = turn(&mut c).await;
        let tools: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::ToolStart { id, name, summary } => {
                    Some(format!("start {id} {name} {summary}"))
                }
                EngineEvent::ToolDone { id, name, ok, .. } => {
                    Some(format!("done {id} {name} {ok}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            tools,
            vec![
                "start t1 read Reading lib.rs",
                "done t1 read true",
                "start t2 execute ls",
                "done t2 execute false",
            ]
        );
    }

    pub(crate) const VALID: &str = "done.\n```json\n{\"outcome\": \"completed\", \"summary\": \"listed them\", \"citations\": [\"a.rs:1\"]}\n```\n";

    #[tokio::test]
    async fn an_end_turn_with_a_valid_fenced_block_writes_response_json_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("scratch").join("RESPONSE.json");
        let (mut c, _f, started) = connect(
            answering(vec![chunk(VALID)], "end_turn"),
            spec(dir.path(), Some(report.clone())),
        )
        .await;
        started.unwrap();
        c.prompt("go".into());
        let events = turn(&mut c).await;
        let v = hotl_tools::report_tool::read_response(&report).expect("RESPONSE.json");
        assert_eq!(v["outcome"], "completed");
        assert_eq!(v["files_touched"], json!([]));
        let pair: Vec<(bool, String)> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::ToolStart { name, .. } if name == "report_result" => {
                    Some((true, name.clone()))
                }
                EngineEvent::ToolDone { name, ok, .. } if name == "report_result" => {
                    assert!(ok);
                    Some((false, name.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(pair.len(), 2, "a start and an ok done: {pair:?}");
        assert!(matches!(outcome(&events), Outcome::Done { .. }));
    }

    #[tokio::test]
    async fn an_invalid_block_is_reprompted_with_the_validation_error_then_unverifiable() {
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("RESPONSE.json");
        let bad = "```json\n{\"outcome\": \"vibes\", \"summary\": \"s\"}\n```";
        let (mut c, fake, started) = connect(
            answering(vec![chunk(bad)], "end_turn"),
            spec(dir.path(), Some(report.clone())),
        )
        .await;
        started.unwrap();
        c.prompt("go".into());
        let events = turn(&mut c).await;
        assert!(events.iter().any(|e| matches!(e, EngineEvent::ToolDone { name, ok: false, .. } if name == "report_result")));
        assert!(!report.exists(), "an invalid block records nothing");
        // spawn.rs re-prompts with the native text; the agent gets its own,
        // led by why the block was refused.
        c.prompt(hotl_tools::report_tool::REPROMPT.into());
        turn(&mut c).await;
        let prompts: Vec<String> = fake
            .seen()
            .iter()
            .filter(|m| method(m) == "session/prompt")
            .map(|m| {
                m["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts[1].contains("`outcome` was `vibes`"),
            "{}",
            prompts[1]
        );
        assert!(prompts[1].contains("```json") && !prompts[1].contains("report_result"));
    }

    #[tokio::test]
    async fn cancelled_refusal_and_max_turn_requests_map_to_the_engine_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        for (stop, want) in [
            ("cancelled", Outcome::Cancelled),
            ("refusal", Outcome::Refused),
            ("max_turn_requests", Outcome::TurnLimit),
            ("max_tokens", Outcome::TurnLimit),
        ] {
            let (mut c, _f, started) =
                connect(answering(vec![], stop), spec(dir.path(), None)).await;
            started.unwrap();
            c.prompt("go".into());
            assert_eq!(outcome(&turn(&mut c).await), want, "{stop}");
        }
    }

    #[tokio::test]
    async fn eof_mid_prompt_is_an_error_carrying_the_stderr_tail() {
        let dir = tempfile::tempdir().unwrap();
        let handler: Handler = Box::new(|m| handshake(m).unwrap_or_default());
        let (mut c, mut fake, started) = connect(handler, spec(dir.path(), None)).await;
        started.unwrap();
        lock(&c.shared.stderr).push("codex-acp: not logged in".into());
        c.prompt("go".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.hang_up();
        match outcome(&turn(&mut c).await) {
            Outcome::Error { message } => {
                assert!(message.contains("closed its output"), "{message}");
                assert!(message.contains("not logged in"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn interrupt_sends_cancel_then_kills_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        // Never answers the prompt, even after a cancel.
        let handler: Handler = Box::new(|m| handshake(m).unwrap_or_default());
        let (mut c, fake, started) = connect(handler, spec(dir.path(), None)).await;
        started.unwrap();
        c.prompt("go".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let t0 = std::time::Instant::now();
        c.interrupt();
        assert_eq!(outcome(&turn(&mut c).await), Outcome::Cancelled);
        assert!(
            t0.elapsed() >= Duration::from_millis(200),
            "waited the grace"
        );
        let cancel = fake
            .seen()
            .into_iter()
            .find(|m| method(m) == "session/cancel")
            .unwrap();
        assert_eq!(cancel["params"]["sessionId"], "s1");
        assert!(cancel.get("id").is_none(), "a notification");
        assert!(lock(&c.shared.dead).is_some(), "killed after the grace");
    }

    #[tokio::test]
    async fn the_timeout_cancels_and_errors_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let handler: Handler = Box::new(|m| handshake(m).unwrap_or_default());
        let mut s = spec(dir.path(), None);
        s.timeout = Duration::from_millis(100);
        let (mut c, fake, started) = connect(handler, s).await;
        started.unwrap();
        c.prompt("go".into());
        match outcome(&turn(&mut c).await) {
            Outcome::Error { message } => assert!(message.contains("timed out"), "{message}"),
            other => panic!("{other:?}"),
        }
        assert!(fake.methods().contains(&"session/cancel".to_string()));
    }

    #[tokio::test]
    async fn usage_update_cost_is_forwarded_as_text_and_never_as_usage() {
        let dir = tempfile::tempdir().unwrap();
        let chunks = vec![
            update(
                json!({"sessionUpdate": "usage_update", "used": 1000, "size": 200000, "cost": {"amount": 0.42, "currency": "USD"}}),
            ),
            update(
                json!({"sessionUpdate": "usage_update", "used": 1200, "size": 200000, "cost": {"amount": 0.42, "currency": "USD"}}),
            ),
            chunk("hi"),
        ];
        let (mut c, _f, started) =
            connect(answering(chunks, "end_turn"), spec(dir.path(), None)).await;
        started.unwrap();
        c.prompt("go".into());
        let events = turn(&mut c).await;
        let texts: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::TextDelta(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 2, "the same spend once: {texts:?}");
        assert!(texts[0].contains("spend reported by the agent: 0.42 USD"));
        // Informational only: not part of the turn's text.
        assert_eq!(outcome(&events), Outcome::Done { text: "hi".into() });
    }

    #[tokio::test]
    async fn fs_and_terminal_requests_are_refused_with_method_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let handler: Handler = Box::new(|m| {
            handshake(m).unwrap_or_else(|| match method(m) {
                "session/prompt" => vec![
                    json!({"jsonrpc": "2.0", "id": 900, "method": "fs/read_text_file", "params": {"sessionId": "s1", "path": "/etc/hosts"}}),
                    json!({"jsonrpc": "2.0", "id": 901, "method": "terminal/create", "params": {"sessionId": "s1", "command": "ls"}}),
                    reply(m, json!({"stopReason": "end_turn"})),
                ],
                _ => Vec::new(),
            })
        });
        let (mut c, fake, started) = connect(handler, spec(dir.path(), None)).await;
        started.unwrap();
        c.prompt("go".into());
        turn(&mut c).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let errs: Vec<Value> = fake
            .seen()
            .into_iter()
            .filter(|m| m.get("error").is_some())
            .collect();
        assert_eq!(errs.len(), 2, "{errs:?}");
        for e in errs {
            assert_eq!(e["error"]["code"], -32601);
        }
    }

    #[test]
    fn provider_credentials_never_reach_the_acp_child() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = spec(dir.path(), None).command(&EgressState::Open).unwrap();
        let envs: Vec<(String, Option<String>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
        ] {
            assert!(
                envs.iter().any(|(k, v)| k == key && v.is_none()),
                "{key} must be removed from the agent's env: {envs:?}"
            );
        }
        assert_eq!(cmd.as_std().get_current_dir(), Some(dir.path()));
    }

    #[test]
    fn state_dirs_widen_the_write_roots_and_never_a_credential_dir() {
        use hotl_platform::KnownPaths as _;
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), None);
        s.state_dirs = vec![state.path().to_path_buf()];
        let extra = s.extra_write().unwrap();
        assert!(extra.contains(&dir.path().to_path_buf()), "the root");
        assert!(
            extra.contains(&dunce::canonicalize(state.path()).unwrap()),
            "the state dir: {extra:?}"
        );
        #[cfg(target_os = "macos")]
        {
            s.sandbox = SandboxStatus::Enforced("seatbelt");
            let cmd = s.command(&EgressState::Open).unwrap();
            let args: Vec<String> = cmd
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let canon = dunce::canonicalize(state.path()).unwrap();
            assert!(
                args.iter()
                    .any(|a| a.starts_with("EXTRA_") && a.ends_with(&*canon.to_string_lossy())),
                "{args:?}"
            );
        }
        let home = hotl_platform::KNOWN_PATHS.home().unwrap_or_default();
        s.state_dirs = vec![home.join(".ssh")];
        let err = s.command(&EgressState::Open).unwrap_err();
        assert!(err.contains("credential"), "{err}");
    }

    #[test]
    fn a_program_not_on_path_is_named() {
        let err = resolve_program("hotl-no-such-agent-binary").unwrap_err();
        assert!(err.contains("not on PATH"), "{err}");
        assert!(resolve_program("sh").is_ok());
    }

    // ---- T4: the gate -------------------------------------------------

    fn options() -> Value {
        json!([
            {"optionId": "aa", "name": "Always", "kind": "allow_always"},
            {"optionId": "a1", "name": "Once", "kind": "allow_once"},
            {"optionId": "r1", "name": "No", "kind": "reject_once"},
            {"optionId": "ra", "name": "Never", "kind": "reject_always"},
        ])
    }

    fn req(tool_call: Value) -> Value {
        json!({"sessionId": "s1", "toolCall": tool_call, "options": options()})
    }

    async fn decide_with(rules: &Rules, root: &Path, tc: Value, sandbox: bool) -> gate::Decision {
        gate::decide(rules, None, &req(tc), root, sandbox).await
    }

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dunce::canonicalize(dir.path()).unwrap();
        (dir, canon)
    }

    #[test]
    fn execute_requests_are_gated_as_bash_with_the_raw_command() {
        let g = gate::translate(&json!({"kind": "execute", "title": "Run tests", "rawInput": {"command": "cargo test"}})).unwrap();
        assert_eq!(
            g,
            gate::GateInput {
                tool: "bash",
                input: json!({"command": "cargo test"})
            }
        );
        let g =
            gate::translate(&json!({"kind": "execute", "rawInput": {"cmd": ["git", "status"]}}))
                .unwrap();
        assert_eq!(g.input, json!({"command": "git status"}));
        let g = gate::translate(&json!({"kind": "execute", "title": "ls -la"})).unwrap();
        assert_eq!(g.input, json!({"command": "ls -la"}));
    }

    #[tokio::test]
    async fn an_edit_outside_the_child_root_is_rejected_as_protected() {
        let (_d, root) = root();
        // Even an allow rule for every write cannot carry an outside path.
        let rules = Rules::from_toml("[[allow]]\ntool = \"edit\"\npath_prefix = \"\"\n").unwrap();
        let tc = json!({"toolCallId": "t1", "kind": "edit", "locations": [{"path": "/etc/hosts"}]});
        let d = decide_with(&rules, &root, tc, true).await;
        assert_eq!(
            (d.allowed, d.option_id.as_deref(), d.tool.as_str()),
            (false, Some("r1"), "edit")
        );
        // The same rule inside the root allows it once.
        let inside = root.join("src/lib.rs");
        let tc = json!({"toolCallId": "t2", "kind": "edit", "content": [{"type": "diff", "path": inside, "newText": "x"}]});
        let d = decide_with(&rules, &root, tc, true).await;
        assert_eq!((d.allowed, d.option_id.as_deref()), (true, Some("a1")));
    }

    #[tokio::test]
    async fn read_and_search_kinds_are_read_only_and_allowed_without_a_rule() {
        let (_d, root) = root();
        let rules = Rules::default();
        for kind in ["read", "search"] {
            let tc = json!({"toolCallId": "t", "kind": kind, "locations": [{"path": root.join("a.rs")}]});
            let d = decide_with(&rules, &root, tc, false).await;
            assert_eq!(
                (d.allowed, d.option_id.as_deref()),
                (true, Some("a1")),
                "{kind}"
            );
            // Outside the root is a protected read: rejected.
            let tc =
                json!({"toolCallId": "t", "kind": kind, "locations": [{"path": "/etc/hosts"}]});
            let d = decide_with(&rules, &root, tc, false).await;
            assert!(!d.allowed, "{kind} outside");
        }
        // A deny rule still reaches a read.
        let rules =
            Rules::from_toml("[[deny]]\ntool = \"read\"\npath_prefix = \"secret\"\n").unwrap();
        let tc = json!({"toolCallId": "t", "kind": "read", "locations": [{"path": "secret/k"}]});
        assert!(!decide_with(&rules, &root, tc, false).await.allowed);
    }

    #[tokio::test]
    async fn an_allow_rule_for_cargo_test_selects_allow_once() {
        let (_d, root) = root();
        let rules =
            Rules::from_toml("[[allow]]\ntool = \"bash\"\nprefix = \"cargo test\"\n").unwrap();
        let tc = json!({"toolCallId": "t", "kind": "execute", "rawInput": {"command": "cargo test -p x"}});
        let d = decide_with(&rules, &root, tc.clone(), true).await;
        assert_eq!(
            (d.allowed, d.option_id.as_deref(), d.tool.as_str()),
            (true, Some("a1"), "bash")
        );
        // No rule: dontask rejects.
        let d = decide_with(&Rules::default(), &root, tc.clone(), true).await;
        assert_eq!((d.allowed, d.option_id.as_deref()), (false, Some("r1")));
        // Bash allow rules need the floor, natively and here.
        let d = decide_with(&rules, &root, tc, false).await;
        assert!(!d.allowed);
    }

    #[tokio::test]
    async fn an_unknown_kind_is_rejected() {
        let (_d, root) = root();
        let rules = Rules::from_toml("[[allow]]\ntool = \"bash\"\nprefix = \"\"\n").unwrap();
        for kind in [
            json!("think"),
            json!("switch_mode"),
            json!("other"),
            Value::Null,
        ] {
            let tc = json!({"toolCallId": "t", "kind": kind, "title": "x"});
            let d = decide_with(&rules, &root, tc, true).await;
            assert_eq!((d.allowed, d.tool.as_str()), (false, "unknown"), "{kind}");
        }
    }

    #[tokio::test]
    async fn a_kind_with_no_findable_input_is_rejected() {
        let (_d, root) = root();
        let rules = Rules::from_toml("[[allow]]\ntool = \"write\"\npath_prefix = \"\"\n").unwrap();
        for tc in [
            json!({"toolCallId": "t", "kind": "delete", "title": "remove it"}),
            json!({"toolCallId": "t", "kind": "fetch", "title": "the docs"}),
            json!({"toolCallId": "t", "kind": "execute", "title": "  "}),
        ] {
            assert!(gate::translate(&tc).is_none(), "{tc}");
            assert!(!decide_with(&rules, &root, tc, true).await.allowed);
        }
    }

    #[tokio::test]
    async fn allow_always_and_reject_always_are_never_selected() {
        let (_d, root) = root();
        let allow = Rules::from_toml("[[allow]]\ntool = \"bash\"\nprefix = \"\"\n").unwrap();
        for (rules, sandbox) in [(&allow, true), (&Rules::default(), true), (&allow, false)] {
            let tc = json!({"toolCallId": "t", "kind": "execute", "title": "ls"});
            let d = decide_with(rules, &root, tc, sandbox).await;
            let id = d.option_id.unwrap();
            assert!(id == "a1" || id == "r1", "{id}");
        }
        // Only the `always` kinds on offer: hotl cancels rather than persist.
        let tc = json!({"toolCallId": "t", "kind": "execute", "title": "ls"});
        let r = json!({"toolCall": tc, "options": [{"optionId": "aa", "kind": "allow_always"}, {"optionId": "ra", "kind": "reject_always"}]});
        let d = gate::decide(&allow, None, &r, &root, true).await;
        assert_eq!(d.option_id, None);
        assert_eq!(d.reply(), json!({"outcome": {"outcome": "cancelled"}}));
    }

    #[tokio::test]
    async fn a_pre_tool_hook_deny_rejects_after_the_rules_allowed() {
        let (_d, root) = root();
        let rules = Rules::from_toml("[[allow]]\ntool = \"bash\"\nprefix = \"\"\n").unwrap();
        let hooks = hotl_engine::hooks::InProcessHooks::new().on_pre_tool(|name, input| {
            match name == "bash" && input["command"] == "rm -rf build" {
                true => hotl_engine::hooks::PreToolDecision::Deny {
                    message: "no".into(),
                },
                false => hotl_engine::hooks::PreToolDecision::Continue,
            }
        });
        let tc = json!({"toolCallId": "t", "kind": "execute", "title": "rm -rf build"});
        let d = gate::decide(&rules, Some(&hooks), &req(tc), &root, true).await;
        assert_eq!((d.allowed, d.option_id.as_deref()), (false, Some("r1")));
        let tc = json!({"toolCallId": "t", "kind": "execute", "title": "ls"});
        let d = gate::decide(&rules, Some(&hooks), &req(tc), &root, true).await;
        assert!(d.allowed);
    }

    #[tokio::test]
    async fn a_rejection_emits_tool_denied_on_the_parent_stream() {
        let (_d, root) = root();
        let handler: Handler = Box::new(|m| {
            handshake(m).unwrap_or_else(|| match method(m) {
                "session/prompt" => vec![
                    update(json!({"sessionUpdate": "tool_call", "toolCallId": "t9", "title": "rm -rf /", "kind": "execute", "status": "pending"})),
                    json!({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": req(json!({"toolCallId": "t9", "kind": "execute", "title": "rm -rf /"}))}),
                ],
                // The permission answer: settle the call and end the turn.
                "" if m["id"] == 900 => vec![
                    update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t9", "status": "failed"})),
                    json!({"jsonrpc": "2.0", "id": 1000, "result": null}),
                ],
                _ => Vec::new(),
            })
        });
        // The prompt reply is sent once the answer is seen; its id is 3
        // (initialize 1, session/new 2).
        let handler: Handler = {
            let mut inner = handler;
            Box::new(move |m| {
                let mut out = inner(m);
                if m["id"] == 900 {
                    out.pop();
                    out.push(
                        json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}}),
                    );
                }
                out
            })
        };
        let (mut c, fake, started) = connect(handler, spec(&root, None)).await;
        started.unwrap();
        c.prompt("go".into());
        let events = turn(&mut c).await;
        let denied: Vec<&EngineEvent> = events
            .iter()
            .filter(|e| matches!(e, EngineEvent::ToolDenied { .. }))
            .collect();
        assert_eq!(denied.len(), 1, "{events:?}");
        assert!(
            matches!(denied[0], EngineEvent::ToolDenied { id, name } if id == "t9" && name == "bash")
        );
        // The agent's own `failed` for the denied call is not a second row.
        assert!(!events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolDone { id, .. } if id == "t9")));
        let answer = fake.seen().into_iter().find(|m| m["id"] == 900).unwrap();
        assert_eq!(
            answer["result"],
            json!({"outcome": {"outcome": "selected", "optionId": "r1"}})
        );
    }

    #[tokio::test]
    async fn no_reject_once_option_means_cancelled() {
        let (_d, root) = root();
        let tc = json!({"toolCallId": "t", "kind": "execute", "title": "ls"});
        let r = json!({"toolCall": tc, "options": [{"optionId": "a1", "kind": "allow_once"}]});
        let d = gate::decide(&Rules::default(), None, &r, &root, true).await;
        assert_eq!((d.allowed, d.option_id.clone()), (false, None));
        assert_eq!(d.reply(), json!({"outcome": {"outcome": "cancelled"}}));
    }
}
