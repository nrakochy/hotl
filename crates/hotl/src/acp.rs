//! `hotl acp` — the ACP-shaped protocol surface (M4).
//!
//! A JSON-RPC 2.0 line protocol over stdio that drives the *same* engine the
//! REPL does: an editor or orchestrator is just another client of
//! `SessionHandle`. One session per connection (process-per-session — the
//! orchestrator pattern). Model output and tool status
//! arrive as `session/update` notifications carrying a `schema_version`
//! (Tier-1 stable, not a side-channel); permission asks are
//! `session/request_permission` round-trips to the client.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use hotl_engine::{AskReply, EngineEvent, Outcome, SessionHandle};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::acp_ext::codes;
use crate::acp_types::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, CancelNotification, CloseSessionRequest,
    ConfigOptionUpdate, Cost, CurrentModeUpdate, ErrorObject, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, McpCapabilities, McpServer, NewSessionRequest, NewSessionResponse, Notice,
    NoticeSeverity, PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionRequest,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionInfo,
    SessionInfoUpdate, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, UsageUpdate,
};

pub const ACP_PROTOCOL_VERSION: u16 = 1;

pub const ACP_SCHEMA_CHECKED: &str = "agent-client-protocol-schema 1.18.0";

pub const EXTENSION_METHODS: &[&str] = &[
    "_hotl/session/fork",
    "_hotl/session/rename",
    "_hotl/session/set_color",
    "_hotl/session/set_goal",
    "_hotl/session/remember",
    "_hotl/session/steer",
    "_hotl/session/fold",
    "_hotl/session/reload_config",
    "_hotl/session/aside",
    "_hotl/session/context",
    "_hotl/session/workflows",
    "_hotl/session/jobs",
    "_hotl/session/jobs/stop",
    "_hotl/session/children",
    "_hotl/session/children/stop",
    "_hotl/session/stop",
    "_hotl/session/update",
    "_hotl/session/request_question",
];

pub const META_METHODS: &str = "hotl.dev/methods";

#[derive(Clone)]
pub(crate) struct Writer {
    sink: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    client: Arc<std::sync::atomic::AtomicU64>,
    seen: Option<u64>,
    connection: Arc<std::sync::atomic::AtomicU64>,
    seen_connection: Option<u64>,
}

impl Writer {
    pub(crate) fn new(sink: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Writer {
            sink: Arc::new(Mutex::new(Box::new(sink))),
            client: Arc::default(),
            seen: None,
            connection: Arc::default(),
            seen_connection: None,
        }
    }

    fn linked(&self) -> Writer {
        Writer {
            seen: None,
            seen_connection: Some(self.connection.load(std::sync::atomic::Ordering::SeqCst)),
            ..self.clone()
        }
    }

    fn stale(&self) -> bool {
        let moved = |seen: Option<u64>, now: &std::sync::atomic::AtomicU64| {
            seen.is_some_and(|seen| seen != now.load(std::sync::atomic::Ordering::SeqCst))
        };
        moved(self.seen, &self.client) || moved(self.seen_connection, &self.connection)
    }

    fn stamped(&self) -> Writer {
        self.stamped_for(self.client())
    }

    fn stamped_for(&self, client: u64) -> Writer {
        Writer {
            seen: Some(client),
            ..self.clone()
        }
    }

    fn client(&self) -> u64 {
        self.seen
            .unwrap_or_else(|| self.client.load(std::sync::atomic::Ordering::SeqCst))
    }

    pub(crate) async fn next_client(&self, new_connection: bool, change: impl FnOnce()) {
        let _held = self.sink.lock().await;
        change();
        self.reinitialized();
        if new_connection {
            self.connection
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn reinitialized(&self) {
        self.client
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) enum Request {
    Ask {
        session_id: String,
        ext: Box<crate::acp_types::ToolCallUpdate>,
        plain: Box<crate::acp_types::ToolCallUpdate>,
        options: Vec<crate::acp_types::PermissionOption>,
    },
    Question {
        session_id: String,
        question: hotl_types::Question,
    },
    Egress {
        session_id: String,
        host: String,
    },
}

impl Request {
    fn build(&self, id: u64, caps: &ClientCaps) -> Option<Value> {
        let (method, params) = match self {
            Request::Ask {
                session_id,
                ext,
                plain,
                options,
            } => (
                "session/request_permission",
                to_json(&RequestPermissionRequest {
                    session_id: session_id.clone(),
                    tool_call: if caps.ext {
                        (**ext).clone()
                    } else {
                        (**plain).clone()
                    },
                    options: options.clone(),
                    meta: None,
                }),
            ),
            Request::Egress { session_id, host } => (
                "session/request_permission",
                to_json(&RequestPermissionRequest {
                    session_id: session_id.clone(),
                    tool_call: crate::acp_map::egress_tool_call(id, host, caps),
                    options: crate::acp_map::egress_options(),
                    meta: None,
                }),
            ),
            Request::Question {
                session_id,
                question,
            } if caps.elicitation_form => (
                "elicitation/create",
                to_json(&crate::acp_map::question_form(
                    session_id, question, caps.ext,
                )),
            ),
            Request::Question {
                session_id,
                question,
            } if caps.ext => (
                "_hotl/session/request_question",
                to_json(&RequestQuestionParams {
                    session_id: session_id.clone(),
                    header: question.header.clone(),
                    prompt: question.prompt.clone(),
                    options: question.options.clone(),
                    multi: question.multi,
                }),
            ),
            Request::Question { .. } => return None,
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
    }
}

impl Request {
    fn announce(&self, id: u64, caps: &ClientCaps) -> Option<Value> {
        let (session_id, call) = match self {
            Request::Ask {
                session_id,
                ext,
                plain,
                ..
            } => (
                session_id,
                if caps.ext {
                    (**ext).clone()
                } else {
                    (**plain).clone()
                },
            ),
            Request::Egress { session_id, host } => {
                (session_id, crate::acp_map::egress_tool_call(id, host, caps))
            }
            Request::Question { .. } => return None,
        };
        let params = SessionNotification {
            session_id: session_id.clone(),
            update: crate::acp_map::announce(&call),
        };
        Some(json!({"jsonrpc": "2.0", "method": "session/update", "params": to_json(&params)}))
    }
}

fn form_of(question: &hotl_types::Question, caps: &ClientCaps) -> Option<FormAsk> {
    caps.elicitation_form.then(|| FormAsk {
        offered: question.options.iter().map(|o| o.label.clone()).collect(),
        multi: question.multi,
    })
}

#[derive(Clone)]
pub(crate) struct ShownTo(Arc<std::sync::atomic::AtomicU64>);

impl ShownTo {
    pub(crate) fn new(client: u64) -> Self {
        ShownTo(Arc::new(std::sync::atomic::AtomicU64::new(client)))
    }

    fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set(&self, client: u64) {
        self.0.store(client, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) struct Sent {
    pub(crate) what: Request,
    pub(crate) at: std::time::Instant,
    pub(crate) client: ShownTo,
}

impl Sent {
    fn to(what: Request, client: ShownTo) -> Self {
        Sent {
            what,
            at: std::time::Instant::now(),
            client,
        }
    }
}

pub(crate) struct AskSlot {
    pub(crate) reply: oneshot::Sender<AskReply>,
    offered: Vec<crate::acp_types::PermissionOption>,
    ext: bool,
    pub(crate) sent: Sent,
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, AskSlot>>>;
/// In-flight `session/request_question` round-trips, parallel to `Pending`.
/// Disjoint ids from `Pending`: both draw from the same per-session `req_id`
/// counter in `drain_events`, so an id never means two different things.
type PendingQuestions = Arc<std::sync::Mutex<HashMap<u64, QuestionSlot>>>;

pub(crate) struct QuestionSlot {
    pub(crate) reply: oneshot::Sender<hotl_types::QuestionAnswer>,
    form: Option<FormAsk>,
    pub(crate) sent: Sent,
}

struct FormAsk {
    offered: Vec<String>,
    multi: bool,
}

fn question_reply_of(msg: &Value, slot: &QuestionSlot) -> hotl_types::QuestionAnswer {
    match &slot.form {
        None => question_answer_from_result(msg.get("result")),
        Some(form) => msg
            .get("result")
            .and_then(|r| {
                serde_json::from_value::<crate::acp_types::CreateElicitationResponse>(r.clone())
                    .ok()
            })
            .map_or(hotl_types::QuestionAnswer::NoHuman, |r| {
                crate::acp_map::question_answer(&r, &form.offered, form.multi)
            }),
    }
}
/// In-flight network `session/request_permission` round-trips (plan 0026), parallel to
/// `Pending` and disjoint from it for the same reason: one `req_id` counter,
/// one meaning per id, but different reply types, so answering an egress
/// prompt through the tool-ask path could only mis-route.
type PendingEgress = Arc<std::sync::Mutex<HashMap<u64, EgressSlot>>>;

pub(crate) struct EgressSlot {
    pub(crate) reply: oneshot::Sender<hotl_tools::net::EgressDecision>,
    pub(crate) sent: Sent,
}

fn permission_response(msg: &Value) -> Option<crate::acp_types::RequestPermissionResponse> {
    serde_json::from_value(msg.get("result")?.clone()).ok()
}

fn ask_reply_of(msg: &Value, slot: &AskSlot) -> AskReply {
    match permission_response(msg) {
        Some(r) => crate::acp_map::ask_reply(&r.outcome, r.meta.as_ref(), slot.ext, &slot.offered),
        None => AskReply::Deny { message: None },
    }
}

fn egress_decision_of(msg: &Value) -> hotl_tools::net::EgressDecision {
    match permission_response(msg) {
        Some(r) => crate::acp_map::egress_decision(&r.outcome),
        None => hotl_tools::net::EgressDecision::NoAnswer,
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
type PendingPrompt = Arc<std::sync::Mutex<PromptBook>>;

#[derive(Default)]
struct PromptBook {
    next_tag: u64,
    ids: HashMap<u64, Value>,
    current: Vec<u64>,
    cancelled: std::collections::HashSet<u64>,
}

fn lock_prompts(book: &PendingPrompt) -> std::sync::MutexGuard<'_, PromptBook> {
    book.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn answer_dropped(
    writer: &Writer,
    book: PendingPrompt,
    dropped: oneshot::Receiver<Vec<u64>>,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    let writer = writer.linked();
    async move {
        let Ok(tags) = dropped.await else {
            return;
        };
        for tag in tags {
            let id = {
                let mut b = lock_prompts(&book);
                b.cancelled.remove(&tag);
                b.ids.remove(&tag)
            };
            if let Some(id) = id {
                reply_cancelled(&writer, id).await;
            }
        }
    }
}

async fn reply_cancelled(writer: &Writer, id: Value) {
    reply_ok(
        writer,
        id,
        to_json(&PromptResponse {
            stop_reason: crate::acp_types::StopReason::Cancelled,
            meta: None,
        }),
    )
    .await;
}

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

/// What `-w`/`--worktree` asked for (0065): a branch by name, or one derived
/// from the session id (`hotl/<id>`), which only exists once the log does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorktreeArg {
    Derived,
    Named(String),
}

impl WorktreeArg {
    /// How `hotl bg` spells it to `hotl serve`, and the ACP `worktree`
    /// param: a derived request and an absent flag must differ on the wire.
    pub(crate) fn wire(&self) -> &str {
        match self {
            Self::Derived => "@derived",
            Self::Named(n) => n,
        }
    }

    pub(crate) fn from_wire(raw: &str) -> Self {
        match raw {
            "@derived" => Self::Derived,
            n => Self::Named(n.to_string()),
        }
    }

    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Self::Derived => None,
            Self::Named(n) => Some(n),
        }
    }
}

/// What a client asked the factory to produce.
pub enum SessionSpec {
    New {
        name: Option<String>,
        /// `-w [name]` (0065): open a git worktree for this session.
        worktree: Option<WorktreeArg>,
        cwd: std::path::PathBuf,
        mcp: Vec<hotl_mcp::config::ServerConfig>,
    },
    Load {
        session_id: String,
        name: Option<String>,
        /// `--no-worktree` (0065): reopen in the main checkout, tombstone the
        /// chain's worktree.
        no_worktree: bool,
        cwd: std::path::PathBuf,
        replay: bool,
        mcp: Vec<hotl_mcp::config::ServerConfig>,
    },
    /// A **new** session seeded with another's projection — optionally only a
    /// prefix of it — pinned to that session's fork-time tip. Unlike `Load`,
    /// the parent may still be live: the pin plus the fork's own `BranchMove`
    /// freeze the inherited history whatever the parent does next.
    Fork {
        session_id: String,
        keep: KeepSpec,
        name: Option<String>,
        /// `-w [name]` on a fork opens a new worktree from `HEAD` and does not
        /// inherit the parent's (0065).
        worktree: Option<WorktreeArg>,
        cwd: std::path::PathBuf,
        mcp: Vec<hotl_mcp::config::ServerConfig>,
    },
}

/// A session the factory opened: the handle plus its display name (the one
/// just given, or inherited from the resumed chain).
pub struct SessionOpen {
    pub handle: SessionHandle,
    pub name: Option<String>,
    /// The session's git worktree as `(path, branch)` (0065); the open reply's
    /// `worktree`. Absent for a main-checkout session.
    pub worktree: Option<(String, String)>,
    /// The worktree's lock, held for the session's life: dropping it on a
    /// clean close is what unlocks (0065 T7). `None` without a worktree.
    pub lease: Option<hotl_store::worktree::Leased>,
    pub color: Option<String>,
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
    /// The per-phase effort schedule, pre-rendered (`plan xhigh \u{b7} implement
    /// high`), or `None` when one rung governs the session (0059 T1). The
    /// client shows it beside the effort so a rung that moves between turns
    /// is explained rather than mysterious.
    pub effort_schedule: Option<String>,
    /// The active goal at open (0034): resume restores it, a fork never
    /// does, a fresh session has none.
    pub goal: Option<String>,
    /// The inherited todo list at open (0040): resume restores it into the
    /// engine's Head, so the client renders the same list or it lies by
    /// omission (§5.7). Empty for a fresh session.
    pub todos: Vec<hotl_types::Todo>,
    /// The effective effort the engine will run (0040): `None` = never set
    /// (client falls back to `default_effort`), `Some(None)` =
    /// inherited-cleared, `Some(Some(l))` = inherited-set. Mirrors
    /// `effort_changed`'s encoding — collapsing the tri-state would make a
    /// cleared setting indistinguishable from one never made.
    pub effort: Option<Option<String>>,
    /// The memory dial (0069) as its config word, so a console can label it
    /// and refuse `/remember <note>` locally under `read-only`. `None` on a
    /// factory that has no dial (tests).
    pub memory_mode: Option<String>,
    /// The lineage log's model when it differs from the running model (0040) —
    /// resume re-derives the model from config, and a silent switch under an
    /// inherited transcript is a §5.7 lie.
    pub previous_model: Option<String>,
    /// The **store** session id — what `hotl_store::replay_chain` resolves and
    /// what `SessionSpec::Load` takes. `_hotl/session/reload_config` resumes through
    /// this.
    pub session_id: String,
    /// Per-session startup notices (an instruction file cut at the cap,
    /// 0070 T9). The open reply carries them as `warnings`; the client shows
    /// each as a notice.
    pub warnings: Vec<String>,
    pub mcp_warnings: Vec<String>,
    /// The generated title a resume restored (0071 T4); the open reply
    /// carries it as `title` (absent when none). A name outranks it.
    pub title: Option<String>,
    /// The `provider/model` this session runs when it is not the process
    /// default (a resumed `/model` switch; 0067). The open reply's `model`.
    pub model: Option<String>,
    pub history: Vec<hotl_types::Item>,
}

/// Builds a session per the client's request. The real binary wires engine
/// deps here; tests inject a scripted-provider session.
pub type SessionFactory = Box<dyn FnMut(SessionSpec) -> Result<SessionOpen, String> + Send>;

/// Rebuilds the factory (and what `initialize` advertises) from a freshly-read
/// `config.toml`, plus any warnings that read produced. This is the whole of
/// `_hotl/session/reload_config`'s engine knowledge: the protocol layer never learns
/// how a scaffold is built, only that one can be rebuilt.
///
/// `Send` because `hotl tui` `tokio::spawn`s `serve`.
pub type Reload = Box<
    dyn FnMut() -> BoxFuture<'static, Result<(SessionFactory, ServerInfo, Vec<String>), String>>
        + Send,
>;

/// `session/set_model`'s engine knowledge (0067): turn `provider/model` (or
/// `default`) into a switch the engine can install, validating the provider
/// half and the key the way startup does. `None` on connections that cannot
/// switch. Returns the switch and any warnings (an uncatalogued model, say).
pub type ModelResolver =
    Arc<dyn Fn(&str) -> Result<(hotl_engine::ModelSwitch, Vec<String>), String> + Send + Sync>;

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

/// One saved workflow recipe (0044) as `initialize` advertises it — the same
/// client-facing shape as [`SkillInfo`], so `/<name>` completes and dispatches
/// without the client walking `<config_dir>/workflows`.
#[derive(Debug, Clone)]
pub struct WorkflowInfo {
    pub name: String,
    pub description: String,
}

/// What `serve` advertises to a client before any session exists. Bundled
/// rather than passed positionally: `serve` is already at the argument count
/// where this workspace reaches for `#[allow(clippy::too_many_arguments)]`.
#[derive(Clone)]
pub struct ServerInfo {
    pub skills: Vec<SkillInfo>,
    pub workflows: Vec<WorkflowInfo>,
    /// The live `workflows_report` payload (`crate::workflow::report` in the
    /// binary). A fn pointer, not a module path: this file is `#[path]`-
    /// included by the protocol tests, which cannot reach the tool.
    pub workflows_report: fn() -> Value,
    /// The configured permission mode a new session starts in, already run
    /// through `enforced_mode`. Clients render it; they never infer it.
    pub default_mode: String,
    /// Whether a new session starts with the plan overlay on.
    pub default_plan: bool,
    /// Model context window in tokens — what a UI's fullness gauge divides by.
    pub context_window: u64,
    /// The stream idle watchdog in seconds (0071 T2) — what the strip's
    /// `stalled … · retry at Ns` names.
    pub stream_idle_secs: u64,
    /// How `session/set_model` resolves a spec (0067); `None` = unsupported.
    pub model_resolver: Option<ModelResolver>,
    pub key_available: bool,
    pub sessions_dir: Option<std::path::PathBuf>,
    pub fallback_models: Vec<String>,
    /// The session's primary model — prices `turn_done`'s `usage.cost_usd`
    /// (Task 5 cost telemetry). One process serves one model for its whole
    /// lifetime (process-per-session), so this is fixed for every
    /// `session/new` and `session/load` this connection sees; a fallback
    /// model used mid-turn is still priced as this one (see
    /// `wire::UsageFrame::of`).
    pub model: String,
}

impl std::fmt::Debug for ServerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerInfo")
            .field("model", &self.model)
            .field("default_mode", &self.default_mode)
            .field("default_plan", &self.default_plan)
            .field("context_window", &self.context_window)
            .field("stream_idle_secs", &self.stream_idle_secs)
            .field("skills", &self.skills.len())
            .field("workflows", &self.workflows.len())
            .field("model_resolver", &self.model_resolver.is_some())
            .finish()
    }
}

/// The `skills` array `initialize` and `config_reloaded` both carry.
fn skill_roster(info: &ServerInfo) -> Vec<RosterEntry> {
    info.skills
        .iter()
        .map(|s| RosterEntry {
            name: s.name.clone(),
            description: s.description.clone(),
        })
        .collect()
}

/// The `workflows` array beside it (0044) — additive, same shape.
fn workflow_roster(info: &ServerInfo) -> Vec<RosterEntry> {
    info.workflows
        .iter()
        .map(|w| RosterEntry {
            name: w.name.clone(),
            description: w.description.clone(),
        })
        .collect()
}

/// Drive the protocol over one connection until the client hangs up.
///
/// `reload` grants the connection the ability to rebuild its engine from
/// `config.toml` mid-flight (`_hotl/session/reload_config`). `None` is a connection
/// without that capability, and the method then answers with an explicit
/// error — never a silent no-op, which a client would read as a reload that
/// happened.
pub async fn serve(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
    factory: SessionFactory,
    info: ServerInfo,
    reload: Option<Reload>,
) {
    let writer = Writer::new(write);
    let mut connection = Connection::new(writer, factory, info, reload);
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // unparseable frame, no id to reply to
        };
        connection.on_message(&msg).await;
    }
}

pub(crate) struct Connection {
    writer: Writer,
    factory: SessionFactory,
    info: ServerInfo,
    reload: Option<Reload>,
    client_caps: ClientCaps,
    pending: Pending,
    pending_questions: PendingQuestions,
    pending_egress: PendingEgress,
    next_id: u64,
    session: Option<SessionState>,
    run_signal: Option<RunSignal>,
}

#[derive(Clone, Default)]
pub(crate) struct RunSignal {
    pub turn_active: Arc<std::sync::atomic::AtomicBool>,
    pub changed: Arc<tokio::sync::Notify>,
}

impl RunSignal {
    fn turn(&self, active: bool) {
        self.turn_active
            .store(active, std::sync::atomic::Ordering::SeqCst);
    }

    fn turn_is_active(&self) -> bool {
        self.turn_active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub(crate) struct Hosted {
    pub(crate) handle: SessionHandle,
    pub(crate) session_id: String,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) effort: Option<String>,
    pub(crate) caps: ClientCaps,
    pub(crate) signal: Option<RunSignal>,
    pub(crate) open: Option<HostedOpen>,
}

pub struct HostedOpen {
    pub mode: String,
    pub plan: bool,
    pub model: Option<String>,
    pub store_id: String,
    pub lease: Option<hotl_store::worktree::Leased>,
}

impl Connection {
    pub(crate) fn new(
        writer: Writer,
        factory: SessionFactory,
        info: ServerInfo,
        reload: Option<Reload>,
    ) -> Self {
        Connection {
            writer,
            factory,
            info,
            reload,
            client_caps: ClientCaps::default(),
            pending: Pending::default(),
            pending_questions: PendingQuestions::default(),
            pending_egress: PendingEgress::default(),
            next_id: 1,
            session: None,
            run_signal: None,
        }
    }

    pub(crate) fn hosting(writer: Writer, info: ServerInfo, hosted: Hosted) -> Self {
        let factory: SessionFactory =
            Box::new(|_| Err("this connection hosts one session that is already open".into()));
        let mut connection = Connection::new(writer, factory, info, None);
        connection.client_caps = hosted.caps;
        connection.run_signal = hosted.signal.clone();
        let (mode, plan, model, store_id, lease) = match hosted.open {
            Some(open) => (
                open.mode,
                open.plan,
                open.model.unwrap_or_else(|| connection.info.model.clone()),
                open.store_id,
                open.lease,
            ),
            None => (
                connection.info.default_mode.clone(),
                connection.info.default_plan,
                connection.info.model.clone(),
                hosted.session_id.clone(),
                None,
            ),
        };
        let config = Arc::new(std::sync::Mutex::new(crate::acp_map::ConfigState {
            model,
            plan,
            effort: hosted.effort.unwrap_or_else(|| "default".into()),
            models: std::iter::once(connection.info.model.clone())
                .chain(connection.info.fallback_models.iter().cloned())
                .collect(),
            ext: hosted.caps.ext,
            context_window: connection.info.context_window,
        }));
        let mut state = start_session(
            hosted.handle,
            config,
            hosted.caps,
            crate::acp_map::ToolCalls::rooted(&hosted.cwd),
            hosted.session_id,
            store_id,
            connection.writer.clone(),
            connection.pending.clone(),
            connection.pending_questions.clone(),
            connection.pending_egress.clone(),
            &mut connection.next_id,
            connection.info.model.clone(),
            hosted.signal,
        );
        state._lease = lease;
        state.mode = mode;
        state.cwd = hosted.cwd;
        state.release_drain();
        connection.session = Some(state);
        connection
    }

    pub(crate) fn writer(&self) -> &Writer {
        &self.writer
    }

    pub(crate) fn handle(&self) -> Option<&SessionHandle> {
        self.session.as_ref().map(|s| &s.handle)
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id.as_str())
    }

    pub(crate) fn run_state(&self) -> crate::run_status::RunState {
        fn any_open<V>(
            slots: &std::sync::Mutex<HashMap<u64, V>>,
            open: impl Fn(&V) -> bool,
        ) -> bool {
            slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .any(open)
        }
        let waiting = any_open(&self.pending, |slot| !slot.reply.is_closed())
            || any_open(&self.pending_questions, |slot| !slot.reply.is_closed())
            || any_open(&self.pending_egress, |slot| !slot.reply.is_closed());
        if waiting {
            crate::run_status::RunState::Waiting
        } else if self
            .run_signal
            .as_ref()
            .is_some_and(RunSignal::turn_is_active)
        {
            crate::run_status::RunState::Running
        } else {
            crate::run_status::RunState::Idle
        }
    }

    pub(crate) fn caps_adopter(&self) -> impl FnOnce() + Send + 'static {
        let caps = self.client_caps;
        let shared = self
            .session
            .as_ref()
            .map(|state| (Arc::clone(&state.caps), Arc::clone(&state.config)));
        move || {
            if let Some((shared_caps, config)) = shared {
                *lock_caps(&shared_caps) = caps;
                lock_config(&config).ext = caps.ext;
            }
        }
    }

    pub(crate) fn resume_result(&self) -> Value {
        let Some(state) = self.session.as_ref() else {
            return json!({});
        };
        to_json(&ResumeSessionResponse {
            modes: Some(crate::acp_map::session_modes(&state.mode)),
            config_options: Some(crate::acp_map::config_options(&lock_config(&state.config))),
            meta: None,
        })
    }

    pub(crate) fn parked_requests(&self) -> Vec<Value> {
        let caps = match self.session.as_ref() {
            Some(state) => *lock_caps(&state.caps),
            None => self.client_caps,
        };
        let client = self.writer.client();
        let fresh = |sent: &Sent| sent.client.get() != client;
        let mut parked: Vec<(u64, Value)> = Vec::new();
        {
            let mut asks = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            asks.retain(|_, slot| !slot.reply.is_closed());
            for (id, slot) in asks.iter_mut().filter(|(_, slot)| fresh(&slot.sent)) {
                if let Some(request) = slot.sent.what.build(*id, &caps) {
                    slot.ext = caps.ext;
                    slot.sent.client.set(client);
                    parked.extend(slot.sent.what.announce(*id, &caps).map(|a| (*id, a)));
                    parked.push((*id, request));
                }
            }
        }
        {
            let mut questions = self
                .pending_questions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            questions.retain(|_, slot| !slot.reply.is_closed());
            for (id, slot) in questions.iter_mut().filter(|(_, slot)| fresh(&slot.sent)) {
                if let Some(request) = slot.sent.what.build(*id, &caps) {
                    if let Request::Question { question, .. } = &slot.sent.what {
                        slot.form = form_of(question, &caps);
                    }
                    slot.sent.client.set(client);
                    parked.push((*id, request));
                }
            }
        }
        {
            let mut egress = self
                .pending_egress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            egress.retain(|_, slot| !slot.reply.is_closed());
            for (id, slot) in egress.iter_mut().filter(|(_, slot)| fresh(&slot.sent)) {
                if let Some(request) = slot.sent.what.build(*id, &caps) {
                    slot.sent.client.set(client);
                    parked.extend(slot.sent.what.announce(*id, &caps).map(|a| (*id, a)));
                    parked.push((*id, request));
                }
            }
        }
        parked.sort_by_key(|(id, _)| *id);
        parked.into_iter().map(|(_, request)| request).collect()
    }

    pub(crate) fn set_client_caps(&mut self, caps: ClientCaps) {
        self.client_caps = caps;
    }

    pub(crate) fn forget_prompts(&self) {
        let Some(state) = self.session.as_ref() else {
            return;
        };
        let mut book = lock_prompts(&state.book);
        book.ids.clear();
        book.cancelled.clear();
    }

    pub(crate) fn expire_parked(&self, after: std::time::Duration) -> usize {
        if after.is_zero() {
            return 0;
        }
        let message = format!("expired after {} s with no human", after.as_secs());
        let now = std::time::Instant::now();
        let expired = |sent: &Sent| now.duration_since(sent.at) >= after;
        let mut n = 0;
        {
            let mut asks = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids: Vec<u64> = asks
                .iter()
                .filter(|(_, slot)| expired(&slot.sent))
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                if let Some(slot) = asks.remove(&id) {
                    let _ = slot.reply.send(AskReply::Deny {
                        message: Some(message.clone()),
                    });
                    n += 1;
                }
            }
        }
        let mut egress = self
            .pending_egress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<u64> = egress
            .iter()
            .filter(|(_, slot)| expired(&slot.sent))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(slot) = egress.remove(&id) {
                let _ = slot.reply.send(hotl_tools::net::EgressDecision::NoAnswer);
                n += 1;
            }
        }
        n
    }

    pub(crate) async fn notice(&mut self, text: String) {
        let Some(session_id) = self.session_id() else {
            return;
        };
        if self.client_caps.notices {
            send_update(
                &self.writer,
                session_id,
                SessionUpdate::Notice(Notice {
                    severity: NoticeSeverity::Warning,
                    title: text,
                    description: None,
                    meta: None,
                }),
            )
            .await;
        }
    }

    pub(crate) async fn on_message(&mut self, msg: &Value) {
        if msg.get("method").is_none() {
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                if let Some(slot) = self
                    .pending_questions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let answer = question_reply_of(msg, &slot);
                    let _ = slot.reply.send(answer);
                } else if let Some(slot) = self
                    .pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = slot.reply.send(egress_decision_of(msg));
                } else if let Some(slot) = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let reply = ask_reply_of(msg, &slot);
                    let _ = slot.reply.send(reply);
                }
            }
            return;
        }
        if msg.get("id").is_none()
            && msg.get("method").and_then(Value::as_str) != Some("session/cancel")
        {
            return;
        }
        handle_request(
            msg,
            &self.writer,
            &mut self.factory,
            &mut self.session,
            &self.pending,
            &self.pending_questions,
            &self.pending_egress,
            &mut self.next_id,
            &mut self.info,
            &mut self.reload,
            &mut self.client_caps,
        )
        .await;
    }
}

type SharedConfig = Arc<std::sync::Mutex<crate::acp_map::ConfigState>>;
type SharedCaps = Arc<std::sync::Mutex<ClientCaps>>;

fn lock_caps(caps: &SharedCaps) -> std::sync::MutexGuard<'_, ClientCaps> {
    caps.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct SessionState {
    id: String,
    /// The durable id this session's log carries — what a resume (and so
    /// `_hotl/session/reload_config`) has to name.
    store_id: String,
    cwd: std::path::PathBuf,
    mcp: Vec<hotl_mcp::config::ServerConfig>,
    handle: SessionHandle,
    config: SharedConfig,
    caps: SharedCaps,
    mode: String,
    drain: tokio::task::JoinHandle<()>,
    go: Option<oneshot::Sender<()>>,
    book: PendingPrompt,
    /// The worktree lock (0065 T7): unlocked when this state drops — a
    /// replaced session, a closed connection, a clean quit.
    _lease: Option<hotl_store::worktree::Leased>,
}

impl SessionState {
    fn release_drain(&mut self) {
        if let Some(go) = self.go.take() {
            let _ = go.send(());
        }
    }
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
    next_id: &mut u64,
    info: &mut ServerInfo,
    reload: &mut Option<Reload>,
    client_caps: &mut ClientCaps,
) {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => {
            let request: InitializeRequest = msg
                .get("params")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            *client_caps = client_caps_of(&request);
            reply_ok(
                writer,
                id,
                to_json(&initialize_response(info, *client_caps)),
            )
            .await;
        }
        "authenticate" => {
            let request: AuthenticateRequest = msg
                .get("params")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            match request.method_id.as_deref() {
                Some(AUTH_METHOD_KEY) => {
                    if let Some(hook) = reload.as_mut() {
                        match hook().await {
                            Ok((new_factory, new_info, _)) => {
                                *factory = new_factory;
                                *info = new_info;
                            }
                            Err(e) => return reply_err(writer, id, codes::INTERNAL, &e).await,
                        }
                    }
                    if key_available(info) {
                        reply_ok(writer, id, json!({})).await;
                    } else {
                        reply_err(writer, id, codes::AUTH_REQUIRED, NO_KEY_MESSAGE).await;
                    }
                }
                Some(AUTH_METHOD_SETUP) => {
                    reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        "hotl-setup is a terminal method — run `hotl setup` instead of sending authenticate",
                    )
                    .await
                }
                Some(other) => {
                    reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        &format!("unknown authentication method `{other}`"),
                    )
                    .await
                }
                None => {
                    reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        "authenticate requires params.methodId",
                    )
                    .await
                }
            }
        }
        method @ ("session/new" | "session/load" | "session/resume" | "_hotl/session/fork") => {
            if !key_available(info) {
                return reply_err(writer, id, codes::AUTH_REQUIRED, NO_KEY_MESSAGE).await;
            }
            let Opening {
                mut spec,
                kind,
                client_id,
                cwd,
                mcp,
            } = match parse_open(method, msg) {
                Ok(o) => o,
                Err(e) => return reply_err(writer, id, codes::INVALID_PARAMS, &e).await,
            };
            if let Some(dir) = info.sessions_dir.clone() {
                let target = match &mut spec {
                    SessionSpec::Load { session_id, .. } | SessionSpec::Fork { session_id, .. } => {
                        Some(session_id)
                    }
                    SessionSpec::New { .. } => None,
                };
                if let Some(session_id) = target {
                    let asked = session_id.clone();
                    let resolved = tokio::task::spawn_blocking(move || {
                        stored_session(&dir, &asked)
                            .then(|| hotl_store::newest_continuation(&dir, &asked))
                    })
                    .await
                    .ok()
                    .flatten();
                    match resolved {
                        Some(newest) => *session_id = newest,
                        None => {
                            return reply_err(
                                writer,
                                id,
                                codes::NOT_FOUND,
                                &format!("no stored session `{session_id}`"),
                            )
                            .await
                        }
                    }
                }
            }
            let mut open = match factory(spec) {
                Ok(open) => open,
                Err(e) => return reply_err(writer, id, codes::INTERNAL, &e).await,
            };
            let history = std::mem::take(&mut open.history);
            let mode = open.mode.clone();
            let todos = open.todos.clone();
            let title = open.name.clone().or_else(|| open.title.clone());
            let mut extras = crate::acp_map::open_extras(&open, &info.model);
            let mcp_warnings = std::mem::take(&mut open.mcp_warnings);
            if !client_caps.notices {
                extras.warnings.extend(mcp_warnings.iter().cloned());
            }
            let sid = match (&kind, client_id) {
                (OpenKind::Load | OpenKind::Resume, Some(client_id)) => client_id,
                _ => open.session_id.clone(),
            };
            install_session(
                open,
                sid.clone(),
                cwd,
                true,
                writer,
                session,
                pending,
                pending_questions,
                pending_egress,
                next_id,
                &info.model,
                &info.fallback_models,
                *client_caps,
                info.context_window,
            )
            .await;
            if let Some(state) = session.as_mut() {
                state.mcp = mcp;
            }
            let context_tokens = match session.as_ref() {
                Some(state) => state
                    .handle
                    .context_breakdown()
                    .await
                    .map(|b| b.rows.iter().map(|r| r.tokens).sum::<u64>()),
                None => None,
            };
            extras.context_tokens = context_tokens;
            if kind == OpenKind::Load {
                for update in crate::acp_map::replay_updates(&history) {
                    send_update(writer, &sid, update).await;
                }
            }
            let meta = client_caps.ext.then(|| {
                let mut m = serde_json::Map::new();
                m.insert(META_OPEN.into(), to_json(&extras));
                m
            });
            let modes = Some(crate::acp_map::session_modes(&mode));
            let config_options = session
                .as_ref()
                .map(|state| crate::acp_map::config_options(&lock_config(&state.config)));
            let result = match kind {
                OpenKind::New | OpenKind::Fork => to_json(&NewSessionResponse {
                    session_id: sid.clone(),
                    modes,
                    config_options,
                    meta,
                }),
                OpenKind::Load => to_json(&LoadSessionResponse {
                    modes,
                    config_options,
                    meta,
                }),
                OpenKind::Resume => to_json(&ResumeSessionResponse {
                    modes,
                    config_options,
                    meta,
                }),
            };
            reply_ok(writer, id, result).await;
            if let Some(state) = session.as_mut() {
                state.release_drain();
            }
            if client_caps.notices {
                for warning in mcp_warnings {
                    send_update(
                        writer,
                        &sid,
                        SessionUpdate::Notice(Notice {
                            severity: NoticeSeverity::Warning,
                            title: warning,
                            description: None,
                            meta: None,
                        }),
                    )
                    .await;
                }
            }
            let commands = info
                .skills
                .iter()
                .map(|s| (s.name.as_str(), s.description.as_str()))
                .chain(
                    info.workflows
                        .iter()
                        .map(|w| (w.name.as_str(), w.description.as_str())),
                );
            send_update(
                writer,
                &sid,
                SessionUpdate::AvailableCommandsUpdate(crate::acp_map::available_commands(
                    commands,
                )),
            )
            .await;
            if !todos.is_empty() {
                send_update(
                    writer,
                    &sid,
                    SessionUpdate::Plan(crate::acp_map::plan_of(&todos)),
                )
                .await;
            }
            if let Some(used) = context_tokens {
                send_update(
                    writer,
                    &sid,
                    SessionUpdate::UsageUpdate(UsageUpdate {
                        used,
                        size: info.context_window,
                        cost: None,
                    }),
                )
                .await;
            }
            if let Some(title) = title {
                send_update(
                    writer,
                    &sid,
                    SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                        title: Some(title),
                        updated_at: None,
                        meta: None,
                    }),
                )
                .await;
            }
            if matches!(kind, OpenKind::Load | OpenKind::Resume) {
                if let Some(state) = session.as_ref() {
                    state.handle.continue_turn().await;
                }
            }
        }
        "session/list" => {
            let request: ListSessionsRequest = match msg.get("params") {
                None | Some(Value::Null) => ListSessionsRequest::default(),
                Some(p) => match serde_json::from_value(p.clone()) {
                    Ok(r) => r,
                    Err(e) => {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            &format!("invalid session/list params: {e}"),
                        )
                        .await
                    }
                },
            };
            let dir = info.sessions_dir.clone();
            let listed =
                tokio::task::spawn_blocking(move || list_sessions(dir.as_deref(), &request))
                    .await
                    .unwrap_or_else(|e| Err(format!("session/list failed: {e}")));
            match listed {
                Ok(list) => reply_ok(writer, id, to_json(&list)).await,
                Err(e) => reply_err(writer, id, codes::INVALID_PARAMS, &e).await,
            }
        }
        "session/close" => {
            let request: CloseSessionRequest =
                match msg.get("params").cloned().map(serde_json::from_value) {
                    Some(Ok(r)) => r,
                    _ => {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            "session/close requires params.sessionId",
                        )
                        .await
                    }
                };
            if session.as_ref().is_some_and(|s| s.id == request.session_id) {
                retire_session(writer, session, pending, pending_questions, pending_egress).await;
            }
            reply_ok(writer, id, json!({})).await;
        }
        "_hotl/session/stop" => {
            retire_session(writer, session, pending, pending_questions, pending_egress).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
        }
        "session/prompt" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let request: PromptRequest =
                match serde_json::from_value(msg.get("params").cloned().unwrap_or(Value::Null)) {
                    Ok(r) => r,
                    Err(e) => {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            &format!(
                            "session/prompt requires params.prompt, a list of content blocks: {e}"
                        ),
                        )
                        .await
                    }
                };
            if request.session_id.as_ref().is_some_and(|s| *s != state.id) {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    &format!(
                        "unknown session `{}`",
                        request.session_id.unwrap_or_default()
                    ),
                )
                .await;
            }
            let (text, images) = match crate::acp_map::prompt_from_blocks(&request.prompt) {
                Ok(v) => v,
                Err(e) => {
                    return reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        &format!("session/prompt: {e}"),
                    )
                    .await
                }
            };
            let tag = {
                let mut book = lock_prompts(&state.book);
                book.next_tag += 1;
                let tag = book.next_tag;
                book.ids.insert(tag, id);
                tag
            };
            state.handle.prompt_answering(tag, text, images).await;
        }
        "_hotl/session/rename" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let Some(name) = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .and_then(hotl_types::normalize_session_name)
            else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "_hotl/session/rename requires params.name (1–64 chars after trimming)",
                )
                .await;
            };
            state.handle.rename(name.clone()).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
            send_update(
                writer,
                &state.id,
                SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                    title: Some(name),
                    updated_at: None,
                    meta: None,
                }),
            )
            .await;
        }
        "_hotl/session/set_color" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let color = match msg.pointer("/params/color") {
                Some(Value::Null) => None,
                Some(Value::String(c)) => match hotl_theme::session_color(c) {
                    Some(c) => Some(c.to_string()),
                    None => {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            &format!("unknown color {c:?}"),
                        )
                        .await
                    }
                },
                _ => {
                    return reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        "_hotl/session/set_color requires params.color (a color name or null)",
                    )
                    .await
                }
            };
            state.handle.set_color(color).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
        }
        "session/set_mode" => {
            let Some(state) = session.as_mut() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let request: SetSessionModeRequest =
                serde_json::from_value(msg.get("params").cloned().unwrap_or(Value::Null))
                    .unwrap_or(SetSessionModeRequest { mode_id: None });
            let Some(raw) = request.mode_id else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "session/set_mode requires params.modeId (ask | dontask | bypass)",
                )
                .await;
            };
            let Some(mode) = hotl_tools::rules::PermissionMode::from_str(&raw) else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    &format!("unknown mode `{raw}` — use ask, dontask or bypass"),
                )
                .await;
            };
            let effective = hotl_tools::rules::enforced_mode(mode);
            state.handle.set_mode(mode).await;
            state.mode = effective.as_str().into();
            let session_id = state.id.clone();
            reply_ok(writer, id, json!({})).await;
            send_update(
                writer,
                &session_id,
                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate {
                    current_mode_id: effective.as_str().into(),
                }),
            )
            .await;
        }
        "session/set_config_option" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let request: SetSessionConfigOptionRequest =
                serde_json::from_value(msg.get("params").cloned().unwrap_or(Value::Null))
                    .unwrap_or(SetSessionConfigOptionRequest {
                        config_id: None,
                        value: None,
                    });
            let (Some(config_id), Some(value)) = (request.config_id, request.value) else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "session/set_config_option requires params.configId and params.value",
                )
                .await;
            };
            let mut model_meta = None;
            match config_id.as_str() {
                "model" => {
                    let Some(raw) = value.as_str() else {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            "the model option takes a string (provider/model, or default)",
                        )
                        .await;
                    };
                    let Some(resolver) = info.model_resolver.as_ref() else {
                        return reply_err(
                            writer,
                            id,
                            codes::INTERNAL,
                            "the model option cannot change on this connection",
                        )
                        .await;
                    };
                    let (switch, warnings) = match resolver(raw) {
                        Ok(r) => r,
                        Err(e) => {
                            return reply_err(writer, id, codes::INVALID_PARAMS, &e).await;
                        }
                    };
                    let spec = switch.spec.clone();
                    match state.handle.set_model(switch).await {
                        Ok(applied) => {
                            if applied {
                                lock_config(&state.config).model = spec.clone();
                            }
                            model_meta = Some(ModelAck {
                                ok: true,
                                model: spec,
                                applied,
                                warnings,
                            });
                        }
                        Err(e) => {
                            return reply_err(writer, id, codes::INVALID_PARAMS, &e.to_string())
                                .await;
                        }
                    }
                }
                "effort" => {
                    let effort = match value.as_str().map(str::trim) {
                        Some("" | "default" | "unset" | "none") => None,
                        Some(other) => match other.parse::<hotl_provider::Effort>() {
                            Ok(e) => Some(e),
                            Err(_) => {
                                return reply_err(
                                    writer,
                                    id,
                                    codes::INVALID_PARAMS,
                                    "the effort option takes low | medium | high | xhigh | max | default",
                                )
                                .await;
                            }
                        },
                        None => {
                            return reply_err(
                                writer,
                                id,
                                codes::INVALID_PARAMS,
                                "the effort option takes a string",
                            )
                            .await;
                        }
                    };
                    state.handle.set_effort(effort).await;
                    lock_config(&state.config).effort =
                        effort.map_or_else(|| "default".to_string(), |e| e.as_str().to_string());
                }
                "plan" => {
                    let Some(plan) = value.as_bool() else {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            "the plan option takes a boolean",
                        )
                        .await;
                    };
                    state.handle.set_plan(plan).await;
                    lock_config(&state.config).plan = plan;
                }
                other => {
                    return reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        &format!("unknown config option `{other}` — use model, effort or plan"),
                    )
                    .await;
                }
            }
            let session_id = state.id.clone();
            let config_options = crate::acp_map::config_options(&lock_config(&state.config));
            let model_change = model_meta.is_some();
            let meta = model_meta.filter(|_| client_caps.ext).map(|ack| {
                let mut m = serde_json::Map::new();
                m.insert(META_MODEL.into(), to_json(&ack));
                m
            });
            reply_ok(
                writer,
                id,
                to_json(&SetSessionConfigOptionResponse {
                    config_options: config_options.clone(),
                    meta,
                }),
            )
            .await;
            if !model_change {
                send_update(
                    writer,
                    &session_id,
                    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate {
                        config_options,
                        meta: None,
                    }),
                )
                .await;
            }
        }
        "_hotl/session/set_goal" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            // `null` clears; a string must survive `normalize_goal`. No
            // bespoke broadcast: the engine emits
            // `GoalChanged`, which the drain's catch-all relays to every
            // attached surface — and that path also covers the clears the
            // engine initiates itself (met/impossible verdicts), which a
            // handler-side notify never could.
            let raw = msg.pointer("/params/goal");
            let goal = match raw {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => match hotl_types::normalize_goal(s) {
                    Some(g) => Some(g),
                    None => {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            "_hotl/session/set_goal requires params.goal of 1–4000 chars \
                             (or null to clear)",
                        )
                        .await;
                    }
                },
                Some(_) => {
                    return reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        "_hotl/session/set_goal requires params.goal (a string, or null to clear)",
                    )
                    .await;
                }
            };
            state.handle.set_goal(goal.clone()).await;
            reply_ok(writer, id, to_json(&GoalAck { ok: true, goal })).await;
        }
        // `/remember` (0069). With `params.note`: the engine asks the owner
        // through the ordinary `session/request_permission` while this request
        // is in flight — a client that answers permission requests needs
        // nothing new — then replies `{"ok": true}` or the refusal text as an
        // error (read-only, declined, over the cap). Without a note: a
        // `memory_report` reply with the dial, the file and its
        // entries. The write itself reaches every surface as `memory_changed`.
        "_hotl/session/remember" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let raw = msg
                .pointer("/params/note")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(raw) = raw else {
                let Some(report) = state.handle.memory_report().await else {
                    return reply_err(
                        writer,
                        id,
                        codes::INTERNAL,
                        "_hotl/session/remember: memory is off",
                    )
                    .await;
                };
                reply_ok(
                    writer,
                    id,
                    to_json(&AcpOnlyUpdate::MemoryReport {
                        mode: report.mode.as_str().into(),
                        path: report.path.display().to_string(),
                        bytes: report.bytes,
                        cap: report.cap,
                        entries: report
                            .entries
                            .iter()
                            .map(|e| MemoryEntryRow {
                                date: e.date.clone(),
                                note: e.note.clone(),
                            })
                            .collect(),
                    }),
                )
                .await;
                return;
            };
            let Some(note) = hotl_types::normalize_memory_note(raw) else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    &format!(
                        "_hotl/session/remember requires params.note of 1–{} characters on one line",
                        hotl_types::MEMORY_NOTE_MAX_CHARS
                    ),
                )
                .await;
            };
            // Off the read loop: the owner's answer to the confirm arrives
            // through this very loop, so awaiting here would deadlock it.
            let pending = state.handle.remember(note);
            let writer = writer.linked();
            tokio::spawn(async move {
                match pending.await {
                    Ok(()) => reply_ok(&writer, id, to_json(&Ack { ok: true })).await,
                    Err(why) => {
                        reply_err(
                            &writer,
                            id,
                            codes::INTERNAL,
                            &format!("_hotl/session/remember: {why}"),
                        )
                        .await
                    }
                }
            });
        }
        "_hotl/session/steer" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let Some(text) = msg.pointer("/params/text").and_then(Value::as_str) else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "_hotl/session/steer requires params.text",
                )
                .await;
            };
            let images =
                match crate::images::parse_images(msg.pointer("/params").unwrap_or(&Value::Null)) {
                    Ok(v) => v,
                    Err(e) => {
                        return reply_err(
                            writer,
                            id,
                            codes::INTERNAL,
                            &format!("_hotl/session/steer: {e}"),
                        )
                        .await
                    }
                };
            state.handle.steer_with(text.to_string(), images).await;
            reply_ok(writer, id, to_json(&Queued { queued: true })).await;
        }
        // `/compact [focus]` (0066): ack with whether it ran now or waits for
        // the turn boundary; the fold's own frames ride `session/update`.
        "_hotl/session/fold" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "no session — call session/new first",
                )
                .await;
            };
            let focus = match msg.pointer("/params/focus") {
                None | Some(Value::Null) => None,
                Some(Value::String(raw)) => match hotl_types::normalize_focus(raw) {
                    Ok(f) => f,
                    Err(_) => {
                        return reply_err(
                            writer,
                            id,
                            codes::INVALID_PARAMS,
                            "_hotl/session/fold: focus must be 1–4000 chars after trimming",
                        )
                        .await
                    }
                },
                Some(_) => {
                    return reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        "_hotl/session/fold: focus must be 1–4000 chars after trimming",
                    )
                    .await
                }
            };
            match state.handle.fold(focus).await {
                Some(hotl_engine::FoldAdmission::Started) => {
                    reply_ok(writer, id, to_json(&Queued { queued: false })).await
                }
                Some(hotl_engine::FoldAdmission::Queued) => {
                    reply_ok(writer, id, to_json(&Queued { queued: true })).await
                }
                None => {
                    reply_err(
                        writer,
                        id,
                        codes::NOT_FOUND,
                        "_hotl/session/fold: the session is gone",
                    )
                    .await
                }
            }
        }
        "session/cancel" => {
            let request: CancelNotification = msg
                .get("params")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            let Some(state) = session.as_ref() else {
                return;
            };
            if request.session_id.is_some_and(|s| s != state.id) {
                return;
            }
            {
                let mut book = lock_prompts(&state.book);
                let running = book.current.clone();
                book.cancelled.extend(running);
            }
            state.handle.interrupt();
            let dropped = state.handle.drop_queued();
            for (_, slot) in pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain()
            {
                let _ = slot.reply.send(AskReply::Deny { message: None });
            }
            for (_, slot) in pending_questions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain()
            {
                let _ = slot.reply.send(hotl_types::QuestionAnswer::NoHuman);
            }
            for (_, slot) in pending_egress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain()
            {
                let _ = slot.reply.send(hotl_tools::net::EgressDecision::NoAnswer);
            }
            tokio::spawn(answer_dropped(writer, state.book.clone(), dropped));
        }
        // Re-read `config.toml` and rebuild the engine behind this connection,
        // then re-open the live session through the ordinary resume path so its
        // items, todos, name and mode carry forward. Everything a scaffold
        // owns reloads: model, `[[allow]]`, `[[mcp]]`, `[[hook]]`, skills, the
        // system prompt, `[context]`. What cannot is process-wide set-once
        // state — `[sandbox]` extras, `[network]` egress, the thread pools —
        // which the client is told about rather than left to guess at.
        "_hotl/session/reload_config" => {
            let Some(hook) = reload.as_mut() else {
                return reply_err(
                    writer,
                    id,
                    codes::INTERNAL,
                    "_hotl/session/reload_config is not supported on this connection",
                )
                .await;
            };
            // A `config.toml` with a typo must never take the live session
            // down: on failure the running engine is left exactly as it was.
            let (new_factory, new_info, warnings) = match hook().await {
                Ok(triple) => triple,
                Err(e) => {
                    let mut error =
                        ErrorObject::new(codes::INTERNAL, format!("config reload failed: {e}"));
                    error.data = Some(to_json(&AcpOnlyUpdate::ConfigReloadFailed {
                        reason: e.to_string(),
                    }));
                    send(writer, &json!({"jsonrpc": "2.0", "id": id, "error": error})).await;
                    return;
                }
            };
            if !new_info.key_available && session.is_some() {
                return reply_err(writer, id, codes::AUTH_REQUIRED, NO_KEY_MESSAGE).await;
            }
            *factory = new_factory;
            *info = new_info;
            // No live session: the new factory is in place and the next
            // `session/new` gets the reloaded engine. Nothing to re-open.
            let Some((client_id, store_id, cwd, mcp)) = session.as_ref().map(|s| {
                (
                    s.id.clone(),
                    s.store_id.clone(),
                    s.cwd.clone(),
                    s.mcp.clone(),
                )
            }) else {
                return reply_ok(
                    writer,
                    id,
                    to_json(&ReloadAck {
                        ok: true,
                        model: info.model.clone(),
                        warnings,
                    }),
                )
                .await;
            };
            // `name: None` so the replay inherits the session's own name, and
            // `SessionSpec::Load` so it inherits its own mode: a logged
            // `/mode` outlives a `[permissions] mode` edit, exactly as across
            // `hotl resume`. A session that never set one has nothing to
            // inherit and takes the reloaded default — which the ack and the
            // broadcast both report, so no client has to infer it.
            let spec = SessionSpec::Load {
                session_id: store_id,
                name: None,
                no_worktree: false,
                cwd: cwd.clone(),
                replay: false,
                mcp: mcp.clone(),
            };
            match factory(spec) {
                Ok(open) => {
                    let mode = open.mode.clone();
                    let plan = open.plan;
                    let sid = client_id;
                    install_session(
                        open,
                        sid.clone(),
                        cwd,
                        false,
                        writer,
                        session,
                        pending,
                        pending_questions,
                        pending_egress,
                        next_id,
                        &info.model,
                        &info.fallback_models,
                        *client_caps,
                        info.context_window,
                    )
                    .await;
                    if let Some(state) = session.as_mut() {
                        state.mcp = mcp;
                    }
                    reply_ok(
                        writer,
                        id,
                        to_json(&ReloadSessionAck {
                            kind: "config_reloaded",
                            ok: true,
                            session_id: sid,
                            model: info.model.clone(),
                            mode,
                            plan,
                            context_window: info.context_window,
                            skills: skill_roster(info),
                            workflows: workflow_roster(info),
                            warnings,
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
                        codes::INTERNAL,
                        &format!("config reloaded but session re-open failed: {e}"),
                    )
                    .await
                }
            }
        }
        // `/btw` (0071 T5): ack, then the engine broadcasts `aside_result`
        // (or `aside_failed`) through the update stream like any event.
        "_hotl/session/aside" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/aside requires an open session",
                )
                .await;
            };
            let Some(question) = msg
                .pointer("/params/question")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
            else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "_hotl/session/aside requires params.question",
                )
                .await;
            };
            state.handle.aside(question.to_string()).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
        }
        "_hotl/session/context" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/context requires an open session",
                )
                .await;
            };
            match state.handle.context_breakdown().await {
                Some(b) => {
                    reply_ok(
                        writer,
                        id,
                        to_json(&AcpOnlyUpdate::ContextReport {
                            window: b.window,
                            rows: b.rows,
                        }),
                    )
                    .await;
                }
                None => {
                    reply_err(
                        writer,
                        id,
                        codes::NOT_FOUND,
                        "_hotl/session/context: the session is gone",
                    )
                    .await
                }
            }
        }
        // Every workflow run this process has started (0044), live.
        "_hotl/session/workflows" => {
            if session.is_none() {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/workflows requires an open session",
                )
                .await;
            }
            let mut payload = (info.workflows_report)();
            payload["type"] = json!("workflows_report");
            reply_ok(writer, id, payload).await;
        }
        // `/jobs` (0064): the reply is the `jobs_report`.
        "_hotl/session/jobs" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/jobs requires an open session",
                )
                .await;
            };
            let jobs = state.handle.jobs().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let rows: Vec<JobRow> = jobs
                .iter()
                .map(|j| {
                    let (state_word, rc, ended) = match &j.state {
                        hotl_tools::jobs::JobState::Running => ("running", None, now),
                        hotl_tools::jobs::JobState::Done { rc, ended_ms } => {
                            ("done", Some(rc.to_string()), *ended_ms)
                        }
                    };
                    JobRow {
                        id: j.id.clone(),
                        pid: j.pid,
                        command: j.command.clone(),
                        started_ms: j.started_ms,
                        detached: j.detached,
                        state: state_word.into(),
                        rc,
                        secs: ended.saturating_sub(j.started_ms) / 1000,
                    }
                })
                .collect();
            reply_ok(
                writer,
                id,
                to_json(&AcpOnlyUpdate::JobsReport { jobs: rows }),
            )
            .await;
        }
        // `/agents` (0072): the reply is the `children_report`.
        "_hotl/session/children" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/children requires an open session",
                )
                .await;
            };
            let children = state.handle.children().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let rows: Vec<ChildRow> = children
                .iter()
                .map(|c| {
                    use hotl_tools::children::ChildState;
                    let (state_word, ok) = match &c.state {
                        ChildState::Running | ChildState::Finishing => ("running", None),
                        ChildState::Done { ok, .. } => ("done", Some(*ok)),
                        ChildState::Closed => ("closed", None),
                    };
                    ChildRow {
                        id: c.id.clone(),
                        call_id: c.call_id.clone(),
                        agent_type: c.agent_type.clone(),
                        summary: c.summary.clone(),
                        started_ms: c.started_ms,
                        background: c.background,
                        isolated: c.isolated,
                        state: state_word.into(),
                        ok,
                        secs: c.secs(now),
                    }
                })
                .collect();
            reply_ok(
                writer,
                id,
                to_json(&AcpOnlyUpdate::ChildrenReport { children: rows }),
            )
            .await;
        }
        // `/agents stop <id>` (0072): interrupts a running child or closes a
        // parked one; the `agent` done frame that follows is the outcome.
        "_hotl/session/children/stop" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/children/stop requires an open session",
                )
                .await;
            };
            let Some(child) = msg
                .pointer("/params/child")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "_hotl/session/children/stop requires params.child",
                )
                .await;
            };
            let stopped = state.handle.stop_child(child).await;
            reply_ok(writer, id, to_json(&StoppedAck { ok: true, stopped })).await;
        }
        // `/jobs stop <id>` (0064): kills the tree; the `job_done` frame that
        // follows is the outcome.
        "_hotl/session/jobs/stop" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "_hotl/session/jobs/stop requires an open session",
                )
                .await;
            };
            let Some(job) = msg
                .pointer("/params/job")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "_hotl/session/jobs/stop requires params.job",
                )
                .await;
            };
            let stopped = state.handle.stop_job(job).await;
            reply_ok(writer, id, to_json(&StoppedAck { ok: true, stopped })).await;
        }
        other => {
            reply_err(
                writer,
                id,
                codes::METHOD_NOT_FOUND,
                &format!("unknown method `{other}`"),
            )
            .await
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenKind {
    New,
    Load,
    Resume,
    Fork,
}

struct Opening {
    spec: SessionSpec,
    kind: OpenKind,
    client_id: Option<String>,
    cwd: std::path::PathBuf,
    mcp: Vec<hotl_mcp::config::ServerConfig>,
}

#[derive(Default)]
struct OpenMeta {
    name: Option<String>,
    worktree: Option<WorktreeArg>,
    no_worktree: bool,
}

fn session_name_param(v: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(hotl_types::normalize_session_name)
            .map(Some)
            .ok_or_else(|| format!("{field} must be 1–64 chars after trimming")),
    }
}

fn open_meta(meta: Option<&crate::acp_types::Meta>) -> Result<OpenMeta, String> {
    let Some(open) = meta.and_then(|m| m.get(META_OPEN)) else {
        return Ok(OpenMeta::default());
    };
    Ok(OpenMeta {
        name: session_name_param(open.get("name"), "_meta[\"hotl.dev/open\"].name")?,
        worktree: open
            .get("worktree")
            .and_then(Value::as_str)
            .map(WorktreeArg::from_wire),
        no_worktree: open
            .get("noWorktree")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn session_root(cwd: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !cwd.is_absolute() {
        return Err(format!("params.cwd must be an absolute path, got {cwd:?}"));
    }
    if !cwd.is_dir() {
        return Err(format!("params.cwd is not an existing directory: {cwd:?}"));
    }
    Ok(cwd.to_path_buf())
}

fn mcp_servers_param(msg: &Value) -> Result<Vec<hotl_mcp::config::ServerConfig>, String> {
    let listed: Vec<McpServer> = match msg.pointer("/params/mcpServers") {
        None | Some(Value::Null) => Vec::new(),
        Some(v @ Value::Array(_)) => serde_json::from_value(v.clone())
            .map_err(|e| format!("invalid params.mcpServers: {e}"))?,
        Some(_) => return Err("params.mcpServers must be an array".into()),
    };
    crate::acp_map::mcp_from_acp(&listed)
}

fn parse_params<T: serde::de::DeserializeOwned>(method: &str, msg: &Value) -> Result<T, String> {
    serde_json::from_value(msg.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("invalid {method} params: {e}"))
}

fn parse_open(method: &str, msg: &Value) -> Result<Opening, String> {
    if method == "session/new" {
        parse_new(method, msg)
    } else if method == "session/load" || method == "session/resume" {
        parse_load(method, msg)
    } else {
        parse_fork(method, msg)
    }
}

fn parse_new(method: &str, msg: &Value) -> Result<Opening, String> {
    let mcp = mcp_servers_param(msg)?;
    let request: NewSessionRequest = parse_params(method, msg)?;
    let cwd = session_root(&request.cwd)?;
    let meta = open_meta(request.meta.as_ref())?;
    if meta.no_worktree {
        return Err("noWorktree only applies to session/load or session/resume".into());
    }
    Ok(Opening {
        spec: SessionSpec::New {
            name: meta.name,
            worktree: meta.worktree,
            cwd: cwd.clone(),
            mcp: mcp.clone(),
        },
        kind: OpenKind::New,
        client_id: None,
        cwd,
        mcp,
    })
}

fn parse_load(method: &str, msg: &Value) -> Result<Opening, String> {
    for key in ["fork", "keepItems", "keepTurns"] {
        if msg.pointer(&format!("/params/{key}")).is_some() {
            return Err(format!(
                "params.{key} is not part of {method} — fork with _hotl/session/fork"
            ));
        }
    }
    let mcp = mcp_servers_param(msg)?;
    let (session_id, cwd, meta, kind) = if method == "session/load" {
        let r: LoadSessionRequest = parse_params(method, msg)?;
        (r.session_id, r.cwd, r.meta, OpenKind::Load)
    } else {
        let r: ResumeSessionRequest = parse_params(method, msg)?;
        (r.session_id, r.cwd, r.meta, OpenKind::Resume)
    };
    let cwd = session_root(&cwd)?;
    let meta = open_meta(meta.as_ref())?;
    if meta.worktree.is_some() {
        return Err(
            "worktree only applies to a new session or a fork — a resumed session \
             already has a worktree, or had none"
                .into(),
        );
    }
    Ok(Opening {
        spec: SessionSpec::Load {
            session_id: session_id.clone(),
            name: meta.name,
            no_worktree: meta.no_worktree,
            cwd: cwd.clone(),
            replay: kind == OpenKind::Load,
            mcp: mcp.clone(),
        },
        kind,
        client_id: Some(session_id),
        cwd,
        mcp,
    })
}

fn parse_fork(method: &str, msg: &Value) -> Result<Opening, String> {
    let session_id = msg
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} requires params.sessionId"))?;
    let mcp = mcp_servers_param(msg)?;
    let keep = keep_spec(msg)?;
    let name = session_name_param(msg.pointer("/params/name"), "params.name")?;
    let worktree = msg
        .pointer("/params/worktree")
        .and_then(Value::as_str)
        .map(WorktreeArg::from_wire);
    let cwd = match msg.pointer("/params/cwd").and_then(Value::as_str) {
        Some(c) => session_root(std::path::Path::new(c))?,
        None => std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?,
    };
    Ok(Opening {
        spec: SessionSpec::Fork {
            session_id: session_id.to_string(),
            keep,
            name,
            worktree,
            cwd: cwd.clone(),
            mcp: mcp.clone(),
        },
        kind: OpenKind::Fork,
        client_id: None,
        cwd,
        mcp,
    })
}

const SESSION_LIST_PAGE: usize = 50;

pub(crate) fn stored_session(dir: &std::path::Path, session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.chars().all(|c| c.is_ascii_alphanumeric())
        && dir.join(format!("{session_id}.jsonl")).is_file()
}

fn canonical(path: &std::path::Path) -> String {
    dunce::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn chain_rows(
    logs: &[(
        String,
        std::path::PathBuf,
        std::time::SystemTime,
        hotl_store::LogHead,
    )],
) -> std::collections::HashSet<&str> {
    let present: std::collections::HashSet<&str> =
        logs.iter().map(|(id, ..)| id.as_str()).collect();
    let mut newest: HashMap<&str, &str> = HashMap::new();
    let mut roots = Vec::new();
    for (id, .., head) in logs {
        match head.parent.as_deref() {
            Some(parent) if !head.fork && present.contains(parent) => {
                let listable = if head.cwd.is_some() { id.as_str() } else { "" };
                newest.entry(parent).or_insert(listable);
            }
            _ => roots.push(id.as_str()),
        }
    }
    roots
        .into_iter()
        .map(|root| {
            let mut seen = std::collections::HashSet::from([root]);
            let mut row = root;
            for _ in 0..hotl_store::LINEAGE_DEPTH_CAP {
                match newest.get(row) {
                    Some(next) if !next.is_empty() && seen.insert(next) => row = next,
                    _ => break,
                }
            }
            row
        })
        .collect()
}

fn list_sessions(
    dir: Option<&std::path::Path>,
    request: &ListSessionsRequest,
) -> Result<ListSessionsResponse, String> {
    let start = match request.cursor.as_deref() {
        None => 0,
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| format!("params.cursor {c:?} is not a cursor this server gave out"))?,
    };
    let want = request.cwd.as_deref().map(canonical);
    let logs: Vec<_> = dir
        .map(hotl_store::list_sessions)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(session_id, path, modified)| {
            let head = hotl_store::log_head(&path)?;
            Some((session_id, path, modified, head))
        })
        .collect();
    let rows = chain_rows(&logs);
    let mut sessions: Vec<SessionInfo> = logs
        .iter()
        .filter(|(session_id, ..)| rows.contains(session_id.as_str()))
        .filter_map(|(session_id, path, modified, head)| {
            let cwd = head.cwd.clone()?;
            if want.as_ref().is_some_and(|w| *w != cwd) {
                return None;
            }
            Some(SessionInfo {
                session_id: session_id.clone(),
                cwd: cwd.into(),
                title: hotl_store::session_label_from_tail(path).or_else(|| head.label.clone()),
                updated_at: Some(crate::acp_map::rfc3339(*modified)),
            })
        })
        .skip(start)
        .take(SESSION_LIST_PAGE + 1)
        .collect();
    let next_cursor = (sessions.len() > SESSION_LIST_PAGE).then(|| {
        sessions.truncate(SESSION_LIST_PAGE);
        (start + SESSION_LIST_PAGE).to_string()
    });
    Ok(ListSessionsResponse {
        sessions,
        next_cursor,
    })
}

/// `keepItems` / `keepTurns` off a `_hotl/session/fork` request. Mutually exclusive:
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

async fn retire_session(
    writer: &Writer,
    session: &mut Option<SessionState>,
    pending: &Pending,
    pending_questions: &PendingQuestions,
    pending_egress: &PendingEgress,
) {
    let Some(mut old) = session.take() else {
        return;
    };
    old.handle.interrupt();
    {
        let _line = writer.sink.lock().await;
        old.drain.abort();
        let _ = (&mut old.drain).await;
        writer.reinitialized();
    }
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
    let outstanding: Vec<Value> = {
        let mut book = lock_prompts(&old.book);
        let mut ids: Vec<(u64, Value)> = std::mem::take(&mut book.ids).into_iter().collect();
        ids.sort_by_key(|(tag, _)| *tag);
        book.current.clear();
        book.cancelled.clear();
        ids.into_iter().map(|(_, id)| id).collect()
    };
    for id in outstanding {
        reply_cancelled(writer, id).await;
    }
}

fn lock_config(config: &SharedConfig) -> std::sync::MutexGuard<'_, crate::acp_map::ConfigState> {
    config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Make `open` the connection's session, retiring whatever was there.
///
/// Replacing a session: interrupt its in-flight turn (its events are about to
/// stop rendering anywhere — it must not keep running tools invisibly in the
/// shared cwd), stop its drain task, and drop its parked state — a dropped ask
/// sender reads as a deny to the old engine, and a stale prompt id must never
/// be answered with the new session's first TurnDone.
///
/// Shared by `session/new`, `session/load` and `_hotl/session/reload_config` so the
/// three can never drift on what "retire the old session" means.
#[allow(clippy::too_many_arguments)]
async fn install_session(
    open: SessionOpen,
    id: String,
    cwd: std::path::PathBuf,
    hold_drain: bool,
    writer: &Writer,
    session: &mut Option<SessionState>,
    pending: &Pending,
    pending_questions: &PendingQuestions,
    pending_egress: &PendingEgress,
    next_id: &mut u64,
    model: &str,
    fallbacks: &[String],
    caps: ClientCaps,
    context_window: u64,
) {
    retire_session(writer, session, pending, pending_questions, pending_egress).await;
    let config = Arc::new(std::sync::Mutex::new(crate::acp_map::ConfigState {
        model: open.model.clone().unwrap_or_else(|| model.to_string()),
        plan: open.plan,
        effort: crate::acp_map::effort_wire(&open),
        models: std::iter::once(model.to_string())
            .chain(fallbacks.iter().cloned())
            .collect(),
        ext: caps.ext,
        context_window,
    }));
    let lease = open.lease;
    let mode = open.mode;
    let mut state = start_session(
        open.handle,
        config,
        caps,
        crate::acp_map::ToolCalls::rooted(
            open.worktree
                .as_ref()
                .map_or_else(|| cwd.clone(), |(path, _)| path.into()),
        ),
        id,
        open.session_id,
        writer.clone(),
        pending.clone(),
        pending_questions.clone(),
        pending_egress.clone(),
        next_id,
        model.to_string(),
        None,
    );
    state._lease = lease;
    state.mode = mode;
    state.cwd = cwd;
    if !hold_drain {
        state.release_drain();
    }
    *session = Some(state);
}

#[allow(clippy::too_many_arguments)]
fn start_session(
    mut handle: SessionHandle,
    config: SharedConfig,
    caps: ClientCaps,
    calls: crate::acp_map::ToolCalls,
    id: String,
    store_id: String,
    writer: Writer,
    pending: Pending,
    pending_questions: PendingQuestions,
    pending_egress: PendingEgress,
    next_id: &mut u64,
    model: String,
    signal: Option<RunSignal>,
) -> SessionState {
    // Permission/question request ids for this session are disjoint from
    // every other id.
    let req_id_seed = *next_id * 1_000_000;
    *next_id += 1;
    let events = std::mem::replace(&mut handle.events, mpsc::channel(1).1);
    let sid = id.clone();
    let drain_config = Arc::clone(&config);
    let link = EngineLink {
        meter: handle.meter(),
    };
    let book = PendingPrompt::default();
    let pending_prompt = book.clone();
    let caps: SharedCaps = Arc::new(std::sync::Mutex::new(caps));
    let drain_caps = Arc::clone(&caps);
    let (go, held) = oneshot::channel::<()>();
    let drain = tokio::spawn(async move {
        if held.await.is_ok() {
            drain_events(
                events,
                writer,
                pending,
                pending_questions,
                pending_egress,
                pending_prompt,
                sid,
                req_id_seed,
                model,
                drain_config,
                link,
                drain_caps,
                calls,
                signal,
            )
            .await;
        }
    });
    SessionState {
        id,
        store_id,
        cwd: std::path::PathBuf::new(),
        mcp: Vec::new(),
        handle,
        config,
        caps,
        mode: String::new(),
        drain,
        go: Some(go),
        book,
        _lease: None,
    }
}

struct EngineLink {
    meter: hotl_engine::Meter,
}

fn settle_on_answer<T: Send + 'static>(
    reply: oneshot::Sender<T>,
    writer: Writer,
    shown: ShownTo,
    session_id: String,
    tool_call_id: String,
    allowed: fn(&T) -> bool,
) -> oneshot::Sender<T> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut reply = reply;
        tokio::select! {
            answer = rx => {
                let Ok(answer) = answer else {
                    return;
                };
                let done = allowed(&answer);
                let _ = reply.send(answer);
                send_update(
                    &writer.stamped_for(shown.get()),
                    &session_id,
                    crate::acp_map::settled(tool_call_id, done),
                )
                .await;
            }
            () = reply.closed() => {
                send_update(
                    &writer.stamped_for(shown.get()),
                    &session_id,
                    crate::acp_map::settled(tool_call_id, false),
                )
                .await;
            }
        }
    });
    tx
}

/// Bounds one coalesced delta line's size; a burst that large is already a
/// caught-up backlog, not a live stream.
const DELTA_COALESCE_MAX: usize = 16 * 1024;

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
    config: SharedConfig,
    link: EngineLink,
    shared_caps: SharedCaps,
    mut calls: crate::acp_map::ToolCalls,
    signal: Option<RunSignal>,
) {
    let mut held: Option<EngineEvent> = None;
    loop {
        let event = match held.take() {
            Some(e) => e,
            None => match events.recv().await {
                Some(e) => e,
                None => break,
            },
        };
        let out = writer.stamped();
        let caps = *lock_caps(&shared_caps);
        // Fold a burst that is already sitting in the queue into one frame —
        // one JSON line, one flush, one client parse. Cross-kind or control
        // events end the fold and are handled next iteration, in order.
        // Child prose folds per parent (0062 T1).
        let event = match event {
            EngineEvent::TextDelta(mut t) => {
                while t.len() < DELTA_COALESCE_MAX {
                    match events.try_recv() {
                        Ok(EngineEvent::TextDelta(next)) => t.push_str(&next),
                        Ok(other) => {
                            held = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                EngineEvent::TextDelta(t)
            }
            EngineEvent::ThinkingDelta(mut t) => {
                while t.len() < DELTA_COALESCE_MAX {
                    match events.try_recv() {
                        Ok(EngineEvent::ThinkingDelta(next)) => t.push_str(&next),
                        Ok(other) => {
                            held = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                EngineEvent::ThinkingDelta(t)
            }
            EngineEvent::ChildText {
                parent_id,
                text: mut t,
            } => {
                while t.len() < DELTA_COALESCE_MAX {
                    match events.try_recv() {
                        Ok(EngineEvent::ChildText {
                            parent_id: p,
                            text: next,
                        }) if p == parent_id => t.push_str(&next),
                        Ok(other) => {
                            held = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                EngineEvent::ChildText { parent_id, text: t }
            }
            other => other,
        };
        if let EngineEvent::ModelChanged {
            model,
            previous,
            context_tokens,
            context_window,
        } = &event
        {
            let (config_options, ext) = {
                let mut state = lock_config(&config);
                state.model = model.clone();
                state.context_window = *context_window;
                (crate::acp_map::config_options(&state), state.ext)
            };
            let meta = ext.then(|| {
                let mut m = serde_json::Map::new();
                m.insert(
                    META_MODEL.into(),
                    to_json(&crate::acp_ext::ModelChange {
                        model: model.clone(),
                        previous: previous.clone(),
                        context_tokens: *context_tokens,
                        context_window: *context_window,
                    }),
                );
                m
            });
            send_update(
                &out,
                &session_id,
                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate {
                    config_options,
                    meta,
                }),
            )
            .await;
            continue;
        }
        let state_moves = matches!(
            event,
            EngineEvent::Ask { .. }
                | EngineEvent::Question { .. }
                | EngineEvent::EgressAsk { .. }
                | EngineEvent::TurnDone { .. }
                | EngineEvent::TurnStarted { .. }
        );
        match event {
            EngineEvent::Ask {
                summary,
                protected_why,
                call_id,
                tool,
                input,
                origin,
                reply,
            } => {
                req_id += 1;
                let view = crate::acp_map::AskView {
                    call_id: &call_id,
                    tool: &tool,
                    input: &input,
                    summary: &summary,
                    protected_why: protected_why.as_deref(),
                };
                let announced = calls.announced(&call_id);
                let ext = calls.ask(&view, &ClientCaps { ext: true, ..caps });
                let plain = calls.ask(&view, &ClientCaps { ext: false, ..caps });
                if !announced {
                    let shown = if caps.ext { &ext } else { &plain };
                    send_update(&out, &session_id, crate::acp_map::announce(shown)).await;
                }
                let shown = ShownTo::new(out.client());
                let reply = if origin.is_synthetic() {
                    settle_on_answer(
                        reply,
                        writer.clone(),
                        shown.clone(),
                        session_id.clone(),
                        plain.tool_call_id.clone(),
                        |r: &AskReply| {
                            matches!(r, AskReply::Allow | AskReply::AllowWithSecretReads)
                        },
                    )
                } else {
                    reply
                };
                let options = crate::acp_map::ask_options(&summary);
                let what = Request::Ask {
                    session_id: session_id.clone(),
                    ext: Box::new(ext),
                    plain: Box::new(plain),
                    options: options.clone(),
                };
                let request = what.build(req_id, &caps);
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        req_id,
                        AskSlot {
                            reply,
                            offered: options,
                            ext: caps.ext,
                            sent: Sent::to(what, shown),
                        },
                    );
                if let Some(request) = request {
                    send(&out, &request).await;
                }
            }
            EngineEvent::Question {
                question, reply, ..
            } => {
                req_id += 1;
                let form = form_of(&question, &caps);
                let what = Request::Question {
                    session_id: session_id.clone(),
                    question,
                };
                match what.build(req_id, &caps) {
                    Some(request) => {
                        pending_questions
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(
                                req_id,
                                QuestionSlot {
                                    reply,
                                    form,
                                    sent: Sent::to(what, ShownTo::new(out.client())),
                                },
                            );
                        send(&out, &request).await;
                    }
                    None => {
                        let _ = reply.send(hotl_types::QuestionAnswer::NoHuman);
                    }
                }
            }
            // Plan 0026. Both surfaces already carried a two-way allow/deny
            // reply and already defaulted to deny, so excluding ACP would only
            // mean the eventual egress default flip silently breaks every
            // editor integration with no in-protocol recourse.
            EngineEvent::EgressAsk { host, reply } => {
                req_id += 1;
                let call = crate::acp_map::egress_tool_call(req_id, &host, &caps);
                send_update(&out, &session_id, crate::acp_map::announce(&call)).await;
                let shown = ShownTo::new(out.client());
                let reply = settle_on_answer(
                    reply,
                    writer.clone(),
                    shown.clone(),
                    session_id.clone(),
                    call.tool_call_id,
                    |d: &hotl_tools::net::EgressDecision| {
                        *d == hotl_tools::net::EgressDecision::Allow
                    },
                );
                let what = Request::Egress {
                    session_id: session_id.clone(),
                    host,
                };
                let request = what.build(req_id, &caps);
                pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        req_id,
                        EgressSlot {
                            reply,
                            sent: Sent::to(what, shown),
                        },
                    );
                if let Some(request) = request {
                    send(&out, &request).await;
                }
            }
            EngineEvent::TurnDone {
                outcome,
                usage,
                mispredictions,
            } => {
                if let Some(signal) = &signal {
                    signal.turn(false);
                }
                // A turn that ended without its asks/questions being answered
                // left dead reply channels behind — drop them so they can't
                // leak.
                pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, slot| !slot.reply.is_closed());
                pending_questions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, slot| !slot.reply.is_closed());
                pending_egress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, slot| !slot.reply.is_closed());
                calls.end_turn();
                let misses = (mispredictions > 0).then_some(mispredictions);
                let frame = crate::wire::UsageFrame::of(&model, &usage);
                let (size, ext) = {
                    let state = lock_config(&config);
                    (state.context_window, state.ext)
                };
                let spent = link.meter.spent_usd();
                send_update(
                    &out,
                    &session_id,
                    SessionUpdate::UsageUpdate(UsageUpdate {
                        used: link.meter.context_tokens(),
                        size,
                        cost: (spent > 0.0).then(|| Cost {
                            amount: spent,
                            currency: "USD".into(),
                        }),
                    }),
                )
                .await;
                let replies = writer.linked();
                let answered: Vec<(u64, Value, bool)> = {
                    let mut book = lock_prompts(&pending_prompt);
                    let tags = std::mem::take(&mut book.current);
                    tags.into_iter()
                        .filter_map(|tag| {
                            let cancelled = book.cancelled.contains(&tag);
                            book.ids.get(&tag).map(|id| (tag, id.clone(), cancelled))
                        })
                        .collect()
                };
                let turn = TurnMeta {
                    outcome: crate::wire::OutcomeFrame::of(&outcome),
                    usage: frame,
                    mispredictions: misses,
                };
                for (tag, id, cancelled) in answered {
                    reply_turn_end(&replies, id, &outcome, cancelled, ext, turn.clone()).await;
                    let mut book = lock_prompts(&pending_prompt);
                    book.ids.remove(&tag);
                    book.cancelled.remove(&tag);
                }
            }
            EngineEvent::TurnStarted { tags } => {
                lock_prompts(&pending_prompt).current = tags;
                if let Some(signal) = &signal {
                    signal.turn(true);
                }
            }
            other => match crate::acp_map::outgoing(&other, &caps, &mut calls) {
                Some(crate::acp_map::Outgoing::Update(update)) => {
                    send_update(&out, &session_id, update).await
                }
                Some(crate::acp_map::Outgoing::Ext(update)) => {
                    notify_ext(&out, &session_id, update).await
                }
                None => {}
            },
        }
        if let (true, Some(signal)) = (state_moves, &signal) {
            signal.changed.notify_one();
        }
    }
}

async fn reply_turn_end(
    writer: &Writer,
    id: Value,
    outcome: &Outcome,
    cancelled: bool,
    ext: bool,
    turn: TurnMeta,
) {
    let meta = ext.then(|| {
        let mut m = serde_json::Map::new();
        m.insert(META_TURN.into(), to_json(&turn));
        m
    });
    let stop = if cancelled {
        Ok(crate::acp_types::StopReason::Cancelled)
    } else {
        crate::acp_map::stop_reason(outcome)
    };
    match stop {
        Ok(stop_reason) => {
            reply_ok(writer, id, to_json(&PromptResponse { stop_reason, meta })).await
        }
        Err(mut data) => {
            if let (Some(meta), Some(obj)) = (meta, data.as_object_mut()) {
                obj.insert("_meta".into(), Value::Object(meta));
            }
            let message = match outcome {
                Outcome::Error { message } => message.clone(),
                _ => "the turn failed".into(),
            };
            send(
                writer,
                &json!({"jsonrpc": "2.0", "id": id, "error": ErrorObject {
                    code: codes::INTERNAL,
                    message,
                    data: Some(data),
                }}),
            )
            .await;
        }
    }
}

async fn notify_ext(writer: &Writer, session_id: &str, update: Value) {
    send(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": crate::acp_ext::ext("session/update"),
            "params": {"sessionId": session_id, "update": update},
        }),
    )
    .await;
}

pub(crate) async fn send_update(writer: &Writer, session_id: &str, update: SessionUpdate) {
    let params = SessionNotification {
        session_id: session_id.to_string(),
        update,
    };
    send(
        writer,
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": to_json(&params)}),
    )
    .await;
}

pub(crate) async fn reply_ok(writer: &Writer, id: Value, result: Value) {
    send(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
    .await;
}

pub(crate) async fn reply_err(writer: &Writer, id: Value, code: i64, message: &str) {
    send(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "error": ErrorObject::new(code, message)}),
    )
    .await;
}

pub(crate) async fn send(writer: &Writer, msg: &Value) {
    let mut line = msg.to_string();
    line.push('\n');
    let mut w = writer.sink.lock().await;
    if writer.stale() {
        return;
    }
    let _ = w.write_all(line.as_bytes()).await;
    let _ = w.flush().await;
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use hotl_types::TokenUsage;
    use tokio::io::AsyncReadExt;

    /// Run `drain_events` to completion over a pre-closed channel, capturing
    /// every line it writes.
    async fn run_drain_events_capturing_writer(rx: mpsc::Receiver<EngineEvent>) -> Vec<String> {
        drain_with_queue(rx, PendingPrompt::default()).await
    }

    fn test_config() -> SharedConfig {
        Arc::new(std::sync::Mutex::new(crate::acp_map::ConfigState {
            model: "test-model".into(),
            plan: false,
            effort: "default".into(),
            models: Vec::new(),
            ext: false,
            context_window: 200_000,
        }))
    }

    async fn drain_with_queue(
        rx: mpsc::Receiver<EngineEvent>,
        queue: PendingPrompt,
    ) -> Vec<String> {
        drain_with(rx, queue, test_config()).await
    }

    async fn drain_with(
        rx: mpsc::Receiver<EngineEvent>,
        queue: PendingPrompt,
        config: SharedConfig,
    ) -> Vec<String> {
        let (write, mut read) = tokio::io::duplex(1 << 20);
        let writer = Writer::new(write);
        drain_events(
            rx,
            writer.clone(),
            Pending::default(),
            PendingQuestions::default(),
            PendingEgress::default(),
            queue,
            "acp-test".into(),
            0,
            "test-model".into(),
            config,
            EngineLink {
                meter: hotl_engine::Meter::default(),
            },
            Arc::new(std::sync::Mutex::new(ClientCaps {
                ext: true,
                ..ClientCaps::default()
            })),
            crate::acp_map::ToolCalls::default(),
            None,
        )
        .await;
        drop(writer); // close the duplex so read_to_string sees EOF
        let mut buf = String::new();
        read.read_to_string(&mut buf).await.unwrap();
        buf.lines().map(str::to_string).collect()
    }

    #[tokio::test]
    async fn a_prompt_reply_in_flight_survives_a_new_initialize_on_the_same_connection() {
        let book = PendingPrompt::default();
        lock_prompts(&book).ids.insert(1, json!(7));
        let (tx, rx) = mpsc::channel(4);
        tx.send(EngineEvent::TurnStarted { tags: vec![1] })
            .await
            .unwrap();
        tx.send(turn_done()).await.unwrap();
        drop(tx);
        let (write, mut read) = tokio::io::duplex(1 << 20);
        let writer = Writer::new(write);
        let held = writer.sink.lock().await;
        let drain = tokio::spawn(drain_events(
            rx,
            writer.clone(),
            Pending::default(),
            PendingQuestions::default(),
            PendingEgress::default(),
            book,
            "acp-test".into(),
            0,
            "test-model".into(),
            test_config(),
            EngineLink {
                meter: hotl_engine::Meter::default(),
            },
            Arc::new(std::sync::Mutex::new(ClientCaps::default())),
            crate::acp_map::ToolCalls::default(),
            None,
        ));
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        writer.reinitialized();
        drop(held);
        drain.await.unwrap();
        drop(writer);
        let mut buf = String::new();
        read.read_to_string(&mut buf).await.unwrap();
        assert!(
            buf.lines()
                .any(|l| l.contains("\"id\":7") && l.contains("end_turn")),
            "{buf}"
        );
    }

    #[tokio::test]
    async fn cancelled_replies_for_dropped_prompts_survive_a_new_initialize() {
        let book = PendingPrompt::default();
        lock_prompts(&book).ids.insert(2, json!(8));
        let (write, mut read) = tokio::io::duplex(1 << 20);
        let writer = Writer::new(write);
        let (dropped_tx, dropped) = oneshot::channel();
        let answering = answer_dropped(&writer, book, dropped);
        writer.reinitialized();
        dropped_tx.send(vec![2]).unwrap();
        answering.await;
        drop(writer);
        let mut buf = String::new();
        read.read_to_string(&mut buf).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(buf.trim()).unwrap(),
            json!({"jsonrpc": "2.0", "id": 8, "result": {"stopReason": "cancelled"}})
        );
    }

    fn turn_done() -> EngineEvent {
        EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage: TokenUsage::default(),
            mispredictions: 0,
        }
    }

    #[tokio::test]
    async fn a_turn_cancelled_by_the_client_answers_cancelled_whatever_its_outcome() {
        let book = PendingPrompt::default();
        {
            let mut b = lock_prompts(&book);
            b.ids.insert(1, json!(7));
            b.cancelled.insert(1);
        }
        let (tx, rx) = mpsc::channel(4);
        tx.send(EngineEvent::TurnStarted { tags: vec![1] })
            .await
            .unwrap();
        tx.send(EngineEvent::TurnDone {
            outcome: Outcome::Error {
                message: "stream aborted".into(),
            },
            usage: TokenUsage::default(),
            mispredictions: 0,
        })
        .await
        .unwrap();
        drop(tx);
        let lines = drain_with_queue(rx, book.clone()).await;
        let reply: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(
            reply,
            json!({"jsonrpc": "2.0", "id": 7, "result": {"stopReason": "cancelled"}})
        );
        let b = lock_prompts(&book);
        assert!(b.cancelled.is_empty() && b.ids.is_empty());
    }

    #[tokio::test]
    async fn a_turn_that_names_no_prompt_answers_nobody() {
        let book = PendingPrompt::default();
        lock_prompts(&book).ids.insert(1, json!(7));
        let (tx, rx) = mpsc::channel(4);
        tx.send(EngineEvent::TurnStarted { tags: Vec::new() })
            .await
            .unwrap();
        tx.send(EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "x".into() },
            usage: TokenUsage::default(),
            mispredictions: 0,
        })
        .await
        .unwrap();
        drop(tx);
        let lines = drain_with_queue(rx, book.clone()).await;
        assert!(lines.iter().all(|l| !l.contains("\"id\":7")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("usage_update")),
            "{lines:?}"
        );
        assert_eq!(lock_prompts(&book).ids.len(), 1);
    }

    #[tokio::test]
    async fn a_fallback_lasts_one_turn_and_never_becomes_the_model_option() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(EngineEvent::FallbackModel {
            model: "backup".into(),
        })
        .await
        .unwrap();
        tx.send(turn_done()).await.unwrap();
        drop(tx);
        let config = test_config();
        let lines = drain_with(rx, PendingPrompt::default(), Arc::clone(&config)).await;
        assert!(
            lines.iter().all(|l| !l.contains("config_option_update")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("_hotl/session/update") && l.contains("fallback_model")),
            "{lines:?}"
        );
        let options = crate::acp_map::config_options(&lock_config(&config));
        let model = serde_json::to_value(&options[0]).unwrap();
        assert_eq!(model["currentValue"], "test-model");
    }

    /// 0033 Task 2: a burst already sitting in the queue folds into ONE
    /// frame — one JSON line, one flush, one client parse.
    #[tokio::test]
    async fn queued_text_deltas_coalesce_into_one_update() {
        let (tx, rx) = mpsc::channel(64);
        for chunk in ["a", "b", "c"] {
            tx.send(EngineEvent::TextDelta(chunk.into())).await.unwrap();
        }
        tx.send(turn_done()).await.unwrap();
        drop(tx);
        let lines = run_drain_events_capturing_writer(rx).await;
        let updates: Vec<_> = lines.iter().filter(|l| l.contains("text_delta")).collect();
        assert_eq!(updates.len(), 1, "burst must fold: {lines:?}");
        assert!(updates[0].contains("\"text\":\"abc\""), "{updates:?}");
    }

    /// 0062 T1: a queued burst of one child's prose is one frame; a chunk
    /// from a different child ends the fold, in order.
    #[tokio::test]
    async fn queued_child_text_coalesces_per_parent() {
        let (tx, rx) = mpsc::channel(64);
        for (p, t) in [("s1", "a"), ("s1", "b"), ("s2", "c"), ("s1", "d")] {
            tx.send(EngineEvent::ChildText {
                parent_id: p.into(),
                text: t.into(),
            })
            .await
            .unwrap();
        }
        tx.send(turn_done()).await.unwrap();
        drop(tx);
        let lines = run_drain_events_capturing_writer(rx).await;
        let frames: Vec<_> = lines.iter().filter(|l| l.contains("child_text")).collect();
        assert_eq!(frames.len(), 3, "s1 burst folds, s2 splits it: {lines:?}");
        assert!(
            frames[0].contains("\"parent_id\":\"s1\"") && frames[0].contains("\"text\":\"ab\""),
            "{frames:?}"
        );
        assert!(
            frames[1].contains("\"parent_id\":\"s2\"") && frames[1].contains("\"text\":\"c\""),
            "{frames:?}"
        );
        assert!(
            frames[2].contains("\"parent_id\":\"s1\"") && frames[2].contains("\"text\":\"d\""),
            "{frames:?}"
        );
    }

    /// Only *adjacent same-kind* deltas merge — a cross-kind event ends the
    /// fold and everything stays in order.
    #[tokio::test]
    async fn a_cross_kind_event_ends_the_fold_in_order() {
        let (tx, rx) = mpsc::channel(64);
        tx.send(EngineEvent::TextDelta("a".into())).await.unwrap();
        tx.send(EngineEvent::TextDelta("b".into())).await.unwrap();
        tx.send(EngineEvent::ToolStart {
            id: "t1".into(),
            name: "bash".into(),
            summary: "ls".into(),
            input: serde_json::Value::Null,
        })
        .await
        .unwrap();
        tx.send(EngineEvent::TextDelta("c".into())).await.unwrap();
        tx.send(EngineEvent::ThinkingDelta("t1".into()))
            .await
            .unwrap();
        tx.send(EngineEvent::ThinkingDelta("t2".into()))
            .await
            .unwrap();
        tx.send(turn_done()).await.unwrap();
        drop(tx);
        let lines = run_drain_events_capturing_writer(rx).await;
        let kinds: Vec<&str> = lines
            .iter()
            .filter_map(|l| {
                let v: Value = serde_json::from_str(l).ok()?;
                match v["params"]["update"]["_meta"][crate::acp_map::META_FRAME]["type"].as_str()? {
                    "text_delta" => Some("text"),
                    "thinking_delta" => Some("thinking"),
                    "tool_start" => Some("tool"),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(kinds, ["text", "tool", "text", "thinking"], "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("\"text\":\"ab\"")));
        assert!(lines.iter().any(|l| l.contains("\"text\":\"t1t2\"")));
    }
}

#[cfg(test)]
mod parked_tests {
    use super::*;

    fn connection() -> Connection {
        let writer = Writer::new(tokio::io::sink());
        let info = ServerInfo {
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
        };
        let mut connection = Connection::new(writer, Box::new(|_| Err("no".into())), info, None);
        connection.set_client_caps(ClientCaps {
            ext: true,
            elicitation_form: true,
            ..ClientCaps::default()
        });
        connection
    }

    fn sent_at(what: Request, at: std::time::Instant) -> Sent {
        Sent {
            what,
            at,
            client: ShownTo::new(u64::MAX),
        }
    }

    fn ask(at: std::time::Instant) -> (AskSlot, oneshot::Receiver<AskReply>) {
        let (reply, rx) = oneshot::channel();
        (
            AskSlot {
                reply,
                offered: crate::acp_map::ask_options("bash: ls"),
                ext: true,
                sent: sent_at(ask_request("bash: ls"), at),
            },
            rx,
        )
    }

    fn two_hours_ago() -> std::time::Instant {
        std::time::Instant::now() - std::time::Duration::from_secs(7200)
    }

    #[tokio::test]
    async fn only_asks_past_the_deadline_are_denied_and_the_denial_says_why() {
        let connection = connection();
        let (old, old_rx) = ask(two_hours_ago());
        let (fresh, fresh_rx) = ask(std::time::Instant::now());
        connection.pending.lock().unwrap().insert(1, old);
        connection.pending.lock().unwrap().insert(2, fresh);
        assert_eq!(
            connection.expire_parked(std::time::Duration::from_secs(3600)),
            1
        );
        match old_rx.await.expect("the expired ask was answered") {
            AskReply::Deny { message } => assert_eq!(
                message.as_deref(),
                Some("expired after 3600 s with no human")
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(connection.pending.lock().unwrap().len(), 1);
        let parked = connection.parked_requests();
        assert_eq!(parked.len(), 2);
        assert_eq!(parked[0]["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(parked[1]["id"], 2);
        drop(fresh_rx);
        assert!(connection.parked_requests().is_empty());
    }

    #[tokio::test]
    async fn an_expired_egress_ask_is_no_answer_not_a_session_deny() {
        let connection = connection();
        let (reply, rx) = oneshot::channel();
        connection.pending_egress.lock().unwrap().insert(
            1,
            EgressSlot {
                reply,
                sent: sent_at(
                    Request::Egress {
                        session_id: "s".into(),
                        host: "example.com".into(),
                    },
                    two_hours_ago(),
                ),
            },
        );
        assert_eq!(
            connection.expire_parked(std::time::Duration::from_secs(3600)),
            1
        );
        assert_eq!(
            rx.await.expect("the expired egress ask was answered"),
            hotl_tools::net::EgressDecision::NoAnswer
        );
    }

    #[tokio::test]
    async fn the_ask_expiry_never_touches_a_parked_question() {
        let connection = connection();
        let (ask_slot, ask_rx) = ask(two_hours_ago());
        let (q_tx, q_rx) = oneshot::channel();
        connection.pending.lock().unwrap().insert(1, ask_slot);
        connection.pending_questions.lock().unwrap().insert(
            2,
            QuestionSlot {
                reply: q_tx,
                form: None,
                sent: sent_at(question_request(&[]), two_hours_ago()),
            },
        );
        assert_eq!(
            connection.expire_parked(std::time::Duration::from_secs(3600)),
            1
        );
        assert!(matches!(ask_rx.await, Ok(AskReply::Deny { .. })));
        let parked = connection.parked_requests();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0]["method"], "elicitation/create");
        assert_eq!(
            connection.expire_parked(std::time::Duration::ZERO),
            0,
            "a zero expiry never denies"
        );
        drop(q_rx);
    }
}

#[cfg(test)]
fn ask_request(summary: &str) -> Request {
    let input = json!({"command": "ls"});
    let view = crate::acp_map::AskView {
        call_id: "c1",
        tool: "bash",
        input: &input,
        summary,
        protected_why: None,
    };
    let mut calls = crate::acp_map::ToolCalls::default();
    let caps = ClientCaps::default();
    Request::Ask {
        session_id: "s".into(),
        ext: Box::new(calls.ask(&view, &ClientCaps { ext: true, ..caps })),
        plain: Box::new(calls.ask(&view, &caps)),
        options: crate::acp_map::ask_options(summary),
    }
}

#[cfg(test)]
fn question_request(labels: &[&str]) -> Request {
    Request::Question {
        session_id: "s".into(),
        question: hotl_types::Question {
            header: "h".into(),
            prompt: "p".into(),
            options: labels
                .iter()
                .map(|l| hotl_types::QuestionOption {
                    label: l.to_string(),
                    description: None,
                })
                .collect(),
            multi: false,
        },
    }
}

#[cfg(test)]
mod question_reply_tests {
    use super::*;

    #[test]
    fn a_permission_shaped_reply_to_a_question_is_no_human() {
        let (tx, _rx) = oneshot::channel();
        let slot = QuestionSlot {
            reply: tx,
            form: Some(FormAsk {
                offered: vec!["MVP".into()],
                multi: false,
            }),
            sent: Sent {
                what: question_request(&["MVP"]),
                at: std::time::Instant::now(),
                client: ShownTo::new(0),
            },
        };
        let msg =
            json!({"id": 1, "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}});
        assert_eq!(
            question_reply_of(&msg, &slot),
            hotl_types::QuestionAnswer::NoHuman
        );
    }
}

#[cfg(test)]
mod ask_reply_tests {
    use super::*;

    fn slot(summary: &str, ext: bool) -> (AskSlot, oneshot::Receiver<AskReply>) {
        let (reply, rx) = oneshot::channel();
        (
            AskSlot {
                reply,
                offered: crate::acp_map::ask_options(summary),
                ext,
                sent: Sent {
                    what: ask_request(summary),
                    at: std::time::Instant::now(),
                    client: ShownTo::new(0),
                },
            },
            rx,
        )
    }

    #[test]
    fn documented_client_replies_parse_and_validate() {
        use hotl_types::QuestionAnswer;
        let (ext, _rx) = slot("bash [sandboxed:seatbelt]: cat ~/.ssh/id", true);
        let answer = |result: Value| ask_reply_of(&json!({"id": 1, "result": result}), &ext);
        let examples = [
            json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
            json!({"outcome": {"outcome": "selected", "optionId": "allow-secret-reads"}}),
            json!({"outcome": {"outcome": "selected", "optionId": "deny"}, "_meta": {"hotl.dev/reply": {"message": "no"}}}),
            json!({"outcome": {"outcome": "selected", "optionId": "allow"}, "_meta": {"hotl.dev/reply": {"respond": "use main"}}}),
            json!({"outcome": {"outcome": "selected", "optionId": "allow"}, "_meta": {"hotl.dev/reply": {"input": {"command": "ls"}}}}),
            json!({"outcome": {"outcome": "cancelled"}}),
        ];
        assert_eq!(answer(examples[0].clone()), AskReply::Allow);
        assert_eq!(answer(examples[1].clone()), AskReply::AllowWithSecretReads);
        assert_eq!(
            answer(examples[2].clone()),
            AskReply::Deny {
                message: Some("no".into())
            }
        );
        assert_eq!(
            answer(examples[3].clone()),
            AskReply::Respond {
                content: "use main".into()
            }
        );
        assert_eq!(
            answer(examples[4].clone()),
            AskReply::AllowEdited {
                input: json!({"command": "ls"})
            }
        );
        assert_eq!(
            answer(examples[5].clone()),
            AskReply::Deny { message: None }
        );
        let q = |v: Value| question_answer_from_result(Some(&v));
        assert_eq!(
            q(json!({"selected": ["MVP"]})),
            QuestionAnswer::Selected(vec!["MVP".into()])
        );
        assert_eq!(
            q(json!({"freeText": "later"})),
            QuestionAnswer::FreeText("later".into())
        );
        let schema = crate::acp_ext::schema();
        let defs = &schema["$defs"];
        let mut cases: Vec<(&str, Value)> = examples
            .iter()
            .filter_map(|e| e.pointer("/_meta/hotl.dev~1reply"))
            .map(|r| ("Reply", r.clone()))
            .collect();
        assert_eq!(cases.len(), 3);
        cases.push(("QuestionReply", json!({"selected": ["MVP"]})));
        cases.push(("QuestionReply", json!({"freeText": "later"})));
        for (def, example) in cases {
            let mut sub = defs[def].clone();
            sub["$defs"] = defs.clone();
            let v = jsonschema::validator_for(&sub).expect("a valid schema");
            assert!(v.is_valid(&example), "{def} rejects {example}");
        }
    }

    #[test]
    fn a_reply_that_is_not_a_permission_response_denies() {
        let (plain, _rx) = slot("bash: ls", false);
        for msg in [
            json!({"id": 1}),
            json!({"id": 1, "error": {"code": -32603, "message": "x"}}),
            json!({"id": 1, "result": {"allow": true}}),
            json!({"id": 1, "result": {"outcome": {"outcome": "maybe"}}}),
            json!({"id": 1, "result": {"outcome": {"outcome": "selected"}}}),
        ] {
            assert_eq!(
                ask_reply_of(&msg, &plain),
                AskReply::Deny { message: None },
                "{msg}"
            );
        }
    }

    #[test]
    fn an_egress_reply_allows_on_allow_denies_on_deny_and_is_no_answer_otherwise() {
        use hotl_tools::net::EgressDecision;
        let decide = |result: Value| egress_decision_of(&json!({"id": 1, "result": result}));
        assert_eq!(
            decide(json!({"outcome": {"outcome": "selected", "optionId": "allow"}})),
            EgressDecision::Allow
        );
        assert_eq!(
            decide(json!({"outcome": {"outcome": "selected", "optionId": "deny"}})),
            EgressDecision::Deny
        );
        for result in [
            json!({"outcome": {"outcome": "selected", "optionId": "allow-secret-reads"}}),
            json!({"outcome": {"outcome": "cancelled"}}),
            json!({"allow": true}),
        ] {
            assert_eq!(decide(result.clone()), EgressDecision::NoAnswer, "{result}");
        }
        assert_eq!(
            egress_decision_of(&json!({"id": 1})),
            EgressDecision::NoAnswer
        );
        assert_eq!(
            egress_decision_of(&json!({"id": 1, "error": {"code": -32603, "message": "x"}})),
            EgressDecision::NoAnswer
        );
    }
}

// --- Typed payloads (0076 D2) -------------------------------------------------
//
// Every result and notification body this server writes, as types, so the
// published schema (`acp_ext::schema`) is generated from what is actually sent.
// Field order is the wire order (`preserve_order` serializes declaration order).

/// `{name, description}` — a skill or workflow in a roster.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RosterEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientCaps {
    pub ext: bool,
    pub notices: bool,
    pub compaction: bool,
    pub elicitation_form: bool,
    pub auth_terminal: bool,
}

pub fn caps_of_initialize(msg: &Value) -> ClientCaps {
    let request: InitializeRequest = msg
        .get("params")
        .and_then(|p| serde_json::from_value(p.clone()).ok())
        .unwrap_or_default();
    client_caps_of(&request)
}

pub fn client_caps_of(req: &InitializeRequest) -> ClientCaps {
    let caps = &req.client_capabilities;
    let session = caps.session.as_ref();
    ClientCaps {
        ext: caps
            .meta
            .as_ref()
            .and_then(|m| m.get(crate::acp_ext::META_EXT))
            .and_then(Value::as_u64)
            == Some(1),
        notices: session.is_some_and(|s| s.notices.is_some()),
        compaction: session.is_some_and(|s| s.compaction.is_some()),
        elicitation_form: caps.elicitation.as_ref().is_some_and(|e| e.form.is_some()),
        auth_terminal: caps.auth.terminal,
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HotlInit {
    pub skills: Vec<RosterEntry>,
    pub workflows: Vec<RosterEntry>,
    pub default_mode: String,
    pub default_plan: bool,
    pub context_window: u64,
    pub stream_idle_secs: u64,
    pub acp_schema: String,
}

pub const AUTH_METHOD_KEY: &str = "hotl-key";
pub const AUTH_METHOD_SETUP: &str = "hotl-setup";
pub const NO_KEY_MESSAGE: &str = "no API key — run `hotl setup`";

pub fn key_available(info: &ServerInfo) -> bool {
    info.key_available
}

fn auth_methods(client_caps: ClientCaps) -> Vec<AuthMethod> {
    let mut methods = vec![AuthMethod::Agent {
        id: AUTH_METHOD_KEY.into(),
        name: "API key from hotl config".into(),
        description: Some("Uses the key hotl setup stored, or the provider's env var".into()),
        meta: None,
    }];
    if client_caps.auth_terminal {
        methods.push(AuthMethod::Terminal {
            id: AUTH_METHOD_SETUP.into(),
            name: "Run hotl setup".into(),
            args: vec!["setup".into()],
            env: None,
            description: None,
            meta: None,
        });
    }
    methods
}

pub const META_INIT: &str = "hotl.dev/init";

pub fn initialize_response(info: &ServerInfo, client_caps: ClientCaps) -> InitializeResponse {
    let ext = client_caps.ext;
    let meta = ext.then(|| {
        let init = HotlInit {
            skills: skill_roster(info),
            workflows: workflow_roster(info),
            default_mode: info.default_mode.clone(),
            default_plan: info.default_plan,
            context_window: info.context_window,
            stream_idle_secs: info.stream_idle_secs,
            acp_schema: ACP_SCHEMA_CHECKED.into(),
        };
        let mut m = serde_json::Map::new();
        m.insert(META_INIT.into(), to_json(&init));
        m.insert(META_METHODS.into(), json!(EXTENSION_METHODS));
        m
    });
    InitializeResponse {
        protocol_version: ACP_PROTOCOL_VERSION,
        agent_capabilities: AgentCapabilities {
            load_session: true,
            prompt_capabilities: PromptCapabilities {
                image: true,
                audio: false,
                embedded_context: true,
            },
            mcp_capabilities: McpCapabilities {
                http: true,
                sse: false,
            },
            session_capabilities: SessionCapabilities {
                list: Some(Default::default()),
                resume: Some(Default::default()),
                close: Some(Default::default()),
            },
            meta,
        },
        auth_methods: auth_methods(client_caps),
        agent_info: Implementation {
            name: "hotl".into(),
            title: Some("hotl".into()),
            version: env!("CARGO_PKG_VERSION").into(),
            meta: None,
        },
        meta: None,
    }
}

/// `{path, branch}` of a session's worktree.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: String,
}

pub const META_OPEN: &str = "hotl.dev/open";
pub const META_MODEL: &str = "hotl.dev/model";

/// Absent keys mean something different from `null` (probe failed / never
/// set / same model).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpenExtras {
    pub name: Option<String>,
    pub plan: bool,
    pub default_effort: Option<String>,
    pub goal: Option<String>,
    /// Feature detection: an old server never sends it.
    pub images: bool,
    pub todos: Vec<hotl_types::Todo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Absent = never set; `null` = explicitly cleared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_schedule: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

pub const META_TURN: &str = "hotl.dev/turn";

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnMeta {
    pub outcome: crate::wire::OutcomeFrame,
    pub usage: crate::wire::UsageFrame,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mispredictions: Option<u32>,
}

/// `{"ok": true}`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Ack {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ModelAck {
    pub ok: bool,
    pub model: String,
    /// `false` = parked until the running turn ends.
    pub applied: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GoalAck {
    pub ok: bool,
    pub goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StoppedAck {
    pub ok: bool,
    pub stopped: bool,
}

/// `_hotl/session/reload_config` with no open session.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReloadAck {
    pub ok: bool,
    pub model: String,
    pub warnings: Vec<String>,
}

/// `_hotl/session/reload_config` with an open session (it is re-opened).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReloadSessionAck {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub ok: bool,
    pub session_id: String,
    pub model: String,
    pub mode: String,
    pub plan: bool,
    pub context_window: u64,
    pub skills: Vec<RosterEntry>,
    pub workflows: Vec<RosterEntry>,
    pub warnings: Vec<String>,
}

/// `_hotl/session/steer` and `_hotl/session/aside`: `false` = there was no turn to queue on.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Queued {
    pub queued: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MemoryEntryRow {
    pub date: String,
    pub note: String,
}

/// One background job in a `jobs_report`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct JobRow {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub started_ms: u64,
    pub detached: bool,
    /// `running` | `done`.
    pub state: String,
    /// The exit status once `done`; `null` while running.
    pub rc: Option<String>,
    pub secs: u64,
}

/// One sub-agent in a `children_report`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChildRow {
    pub id: String,
    pub call_id: Option<String>,
    pub agent_type: String,
    pub summary: String,
    pub started_ms: u64,
    pub background: bool,
    pub isolated: bool,
    /// `running` | `done` | `closed`.
    pub state: String,
    /// Set once `done`.
    pub ok: Option<bool>,
    pub secs: u64,
}

/// `session/update` bodies only the ACP surface sends.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpOnlyUpdate {
    MemoryReport {
        mode: String,
        path: String,
        bytes: usize,
        cap: usize,
        entries: Vec<MemoryEntryRow>,
    },
    ConfigReloadFailed {
        reason: String,
    },
    ContextReport {
        window: u64,
        rows: Vec<hotl_types::ContextRow>,
    },
    JobsReport {
        jobs: Vec<JobRow>,
    },
    ChildrenReport {
        children: Vec<ChildRow>,
    },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExtSessionUpdateParams {
    pub session_id: String,
    pub update: crate::wire::Frame,
}

/// Server → client `_hotl/session/request_question` params.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestQuestionParams {
    pub session_id: String,
    pub header: String,
    pub prompt: String,
    pub options: Vec<hotl_types::QuestionOption>,
    pub multi: bool,
}

/// What a client answers `_hotl/session/request_question` with. Schema only.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)] // schema-only
pub enum QuestionReply {
    /// Option labels, as shown.
    Selected { selected: Vec<String> },
    FreeText {
        #[serde(rename = "freeText")]
        free_text: String,
    },
}

fn to_json<T: Serialize>(payload: &T) -> Value {
    serde_json::to_value(payload).expect("an ACP payload always serializes")
}
