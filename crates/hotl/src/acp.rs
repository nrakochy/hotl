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
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::acp_ext::codes;
use crate::acp_types::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, CloseSessionRequest, ConfigOptionUpdate,
    CurrentModeUpdate, ErrorObject, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, NewSessionRequest, NewSessionResponse, PromptCapabilities,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionInfo,
    SessionInfoUpdate, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, UsageUpdate,
};

/// Stable schema version of `session/update` / prompt-result payloads (an MD
/// Tier-1 contract — bump only on a breaking change).
pub const UPDATE_SCHEMA_VERSION: u32 = 1;
pub const ACP_PROTOCOL_VERSION: u16 = 1;

pub const ACP_SCHEMA_CHECKED: &str = "agent-client-protocol-schema 1.18.0";

/// Every method this dialect speaks beyond `initialize`: what the handler
/// answers, then the three requests it sends the client. Advertised under
/// `agentCapabilities._meta.hotl.extensions` (0076 D6).
pub const EXTENSION_METHODS: &[&str] = &[
    "session/new",
    "session/load",
    "session/resume",
    "session/list",
    "session/close",
    "session/prompt",
    "session/rename",
    "session/set_color",
    "session/set_mode",
    "session/set_config_option",
    "session/set_goal",
    "session/remember",
    "session/steer",
    "session/fold",
    "session/cancel",
    "session/reload_config",
    "session/aside",
    "session/context",
    "session/workflows",
    "session/jobs",
    "session/children",
    "session/children/stop",
    "session/jobs/stop",
    "session/request_permission",
    "session/request_question",
    "session/request_egress",
];

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
    },
    Load {
        session_id: String,
        name: Option<String>,
        /// `--no-worktree` (0065): reopen in the main checkout, tombstone the
        /// chain's worktree.
        no_worktree: bool,
        cwd: std::path::PathBuf,
        replay: bool,
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
    /// what `SessionSpec::Load` takes. `session/reload_config` resumes through
    /// this.
    pub session_id: String,
    /// Per-session startup notices (an instruction file cut at the cap,
    /// 0070 T9). The open reply carries them as `warnings`; the client shows
    /// each as a notice.
    pub warnings: Vec<String>,
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
/// `session/reload_config`'s engine knowledge: the protocol layer never learns
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
    /// The session's primary model — prices `turn_done`'s `usage.cost_usd`
    /// (Task 5 cost telemetry). One process serves one model for its whole
    /// lifetime (process-per-session), so this is fixed for every
    /// `session/new` and `session/load` this connection sees; a fallback
    /// model used mid-turn is still priced as this one (see
    /// `wire::usage_frame`).
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
    let mut client_caps = ClientCaps::default();
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
            &mut client_caps,
        )
        .await;
    }
}

type SharedConfig = Arc<std::sync::Mutex<crate::acp_map::ConfigState>>;

struct SessionState {
    id: String,
    /// The durable id this session's log carries — what a resume (and so
    /// `session/reload_config`) has to name.
    store_id: String,
    cwd: std::path::PathBuf,
    handle: SessionHandle,
    config: SharedConfig,
    drain: tokio::task::JoinHandle<()>,
    go: Option<oneshot::Sender<()>>,
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
    pending_prompt: &PendingPrompt,
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
                pending_prompt,
                next_id,
                &info.model,
            );
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
                retire_session(
                    session,
                    pending,
                    pending_questions,
                    pending_egress,
                    pending_prompt,
                );
            }
            reply_ok(writer, id, json!({})).await;
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
            let Some(text) = msg.pointer("/params/text").and_then(Value::as_str) else {
                return reply_err(
                    writer,
                    id,
                    codes::INVALID_PARAMS,
                    "session/prompt requires params.text",
                )
                .await;
            };
            // Images are validated HERE, before the log: a poisoned base64
            // item would be durable and 400 every later sample (see
            // `crate::images`). A rejected prompt commits nothing.
            let images =
                match crate::images::parse_images(msg.pointer("/params").unwrap_or(&Value::Null)) {
                    Ok(v) => v,
                    Err(e) => {
                        return reply_err(
                            writer,
                            id,
                            codes::INTERNAL,
                            &format!("session/prompt: {e}"),
                        )
                        .await
                    }
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
                    "session/rename requires params.name (1–64 chars after trimming)",
                )
                .await;
            };
            state.handle.rename(name).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
        }
        "session/set_color" => {
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
                        "session/set_color requires params.color (a color name or null)",
                    )
                    .await
                }
            };
            state.handle.set_color(color).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
        }
        "session/set_mode" => {
            let Some(state) = session.as_ref() else {
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
            send_update(
                writer,
                &session_id,
                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate { config_options }),
            )
            .await;
        }
        "session/set_goal" => {
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
            // bespoke broadcast, unlike `set_effort`: the engine emits
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
                            "session/set_goal requires params.goal of 1–4000 chars \
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
                        "session/set_goal requires params.goal (a string, or null to clear)",
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
        // `memory_report` notification with the dial, the file and its
        // entries. The write itself reaches every surface as `memory_changed`.
        "session/remember" => {
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
                        "session/remember: memory is off",
                    )
                    .await;
                };
                reply_ok(writer, id, to_json(&Ack { ok: true })).await;
                notify(
                    writer,
                    &state.id,
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
                        "session/remember requires params.note of 1–{} characters on one line",
                        hotl_types::MEMORY_NOTE_MAX_CHARS
                    ),
                )
                .await;
            };
            // Off the read loop: the owner's answer to the confirm arrives
            // through this very loop, so awaiting here would deadlock it.
            let pending = state.handle.remember(note);
            let writer = writer.clone();
            tokio::spawn(async move {
                match pending.await {
                    Ok(()) => reply_ok(&writer, id, to_json(&Ack { ok: true })).await,
                    Err(why) => {
                        reply_err(
                            &writer,
                            id,
                            codes::INTERNAL,
                            &format!("session/remember: {why}"),
                        )
                        .await
                    }
                }
            });
        }
        "session/steer" => {
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
                    "session/steer requires params.text",
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
                            &format!("session/steer: {e}"),
                        )
                        .await
                    }
                };
            state.handle.steer_with(text.to_string(), images).await;
            reply_ok(writer, id, to_json(&Queued { queued: true })).await;
        }
        // `/compact [focus]` (0066): ack with whether it ran now or waits for
        // the turn boundary; the fold's own frames ride `session/update`.
        "session/fold" => {
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
                            "session/fold: focus must be 1–4000 chars after trimming",
                        )
                        .await
                    }
                },
                Some(_) => {
                    return reply_err(
                        writer,
                        id,
                        codes::INVALID_PARAMS,
                        "session/fold: focus must be 1–4000 chars after trimming",
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
                        "session/fold: the session is gone",
                    )
                    .await
                }
            }
        }
        "session/cancel" => {
            if let Some(state) = session.as_ref() {
                state.handle.interrupt();
            }
            reply_ok(writer, id, to_json(&Cancelled { cancelled: true })).await;
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
                    codes::INTERNAL,
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
                            to_json(&AcpOnlyUpdate::ConfigReloadFailed {
                                reason: e.to_string(),
                            }),
                        )
                        .await;
                    }
                    return reply_err(
                        writer,
                        id,
                        codes::INTERNAL,
                        &format!("config reload failed: {e}"),
                    )
                    .await;
                }
            };
            if !new_info.key_available && session.is_some() {
                return reply_err(writer, id, codes::AUTH_REQUIRED, NO_KEY_MESSAGE).await;
            }
            *factory = new_factory;
            *info = new_info;
            // No live session: the new factory is in place and the next
            // `session/new` gets the reloaded engine. Nothing to re-open.
            let Some((client_id, store_id, cwd)) = session
                .as_ref()
                .map(|s| (s.id.clone(), s.store_id.clone(), s.cwd.clone()))
            else {
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
                        pending_prompt,
                        next_id,
                        &info.model,
                    );
                    let skills = skill_roster(info);
                    let workflows = workflow_roster(info);
                    reply_ok(
                        writer,
                        id,
                        to_json(&ReloadSessionAck {
                            ok: true,
                            session_id: sid.clone(),
                            model: info.model.clone(),
                            mode: mode.clone(),
                            plan,
                            context_window: info.context_window,
                            warnings: warnings.clone(),
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
                        to_json(&AcpOnlyUpdate::ConfigReloaded {
                            model: info.model.clone(),
                            mode,
                            plan,
                            context_window: info.context_window,
                            skills,
                            workflows,
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
        // What currently fills the context window, by source. A read: it
        // appends nothing and publishes nothing, so unlike `reload_config`
        // there is no idle guard and no session rebuild.
        // `/btw` (0071 T5): ack, then the engine broadcasts `aside_result`
        // (or `aside_failed`) through the update stream like any event.
        "session/aside" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/aside requires an open session",
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
                    "session/aside requires params.question",
                )
                .await;
            };
            state.handle.aside(question.to_string()).await;
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
        }
        "session/context" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/context requires an open session",
                )
                .await;
            };
            let sid = state.id.clone();
            match state.handle.context_breakdown().await {
                Some(b) => {
                    // Thin ack, payload broadcast — the `config_reloaded`
                    // shape, for the same reason: a client's loop needs no
                    // id-plumbing to get it. Additive, so
                    // `UPDATE_SCHEMA_VERSION` stands.
                    reply_ok(writer, id, to_json(&Ack { ok: true })).await;
                    notify(
                        writer,
                        &sid,
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
                        "session/context: the session is gone",
                    )
                    .await
                }
            }
        }
        // Every workflow run this process has started (0044), live. The
        // registry is process-wide, but the broadcast needs a session to ride
        // on — the same guard and ack-then-broadcast shape as `session/context`.
        "session/workflows" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/workflows requires an open session",
                )
                .await;
            };
            let sid = state.id.clone();
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
            let mut payload = (info.workflows_report)();
            payload["type"] = json!("workflows_report");
            notify(writer, &sid, payload).await;
        }
        // `/jobs` (0064): thin ack, `jobs_report` broadcast — the
        // `session/workflows` shape. Additive, so `UPDATE_SCHEMA_VERSION` stands.
        "session/jobs" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/jobs requires an open session",
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
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
            notify(
                writer,
                &state.id,
                to_json(&AcpOnlyUpdate::JobsReport { jobs: rows }),
            )
            .await;
        }
        // `/agents` (0072): thin ack, `children_report` broadcast — the
        // `session/jobs` shape. Additive, so `UPDATE_SCHEMA_VERSION` stands.
        "session/children" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/children requires an open session",
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
            reply_ok(writer, id, to_json(&Ack { ok: true })).await;
            notify(
                writer,
                &state.id,
                to_json(&AcpOnlyUpdate::ChildrenReport { children: rows }),
            )
            .await;
        }
        // `/agents stop <id>` (0072): interrupts a running child or closes a
        // parked one; the `agent` done frame that follows is the outcome.
        "session/children/stop" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/children/stop requires an open session",
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
                    "session/children/stop requires params.child",
                )
                .await;
            };
            let stopped = state.handle.stop_child(child).await;
            reply_ok(writer, id, to_json(&StoppedAck { ok: true, stopped })).await;
        }
        // `/jobs stop <id>` (0064): kills the tree; the `job_done` frame that
        // follows is the outcome.
        "session/jobs/stop" => {
            let Some(state) = session.as_ref() else {
                return reply_err(
                    writer,
                    id,
                    codes::NOT_FOUND,
                    "session/jobs/stop requires an open session",
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
                    "session/jobs/stop requires params.job",
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

fn check_mcp_servers(msg: &Value) -> Result<(), String> {
    match msg.pointer("/params/mcpServers") {
        None | Some(Value::Null) | Some(Value::Array(_)) => Ok(()),
        Some(_) => Err("params.mcpServers must be an array".into()),
    }
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
    check_mcp_servers(msg)?;
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
        },
        kind: OpenKind::New,
        client_id: None,
        cwd,
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
    check_mcp_servers(msg)?;
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
        },
        kind,
        client_id: Some(session_id),
        cwd,
    })
}

fn parse_fork(method: &str, msg: &Value) -> Result<Opening, String> {
    let session_id = msg
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method} requires params.sessionId"))?;
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
        },
        kind: OpenKind::Fork,
        client_id: None,
        cwd,
    })
}

const SESSION_LIST_PAGE: usize = 50;

fn stored_session(dir: &std::path::Path, session_id: &str) -> bool {
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
    let continued: std::collections::HashSet<&str> = logs
        .iter()
        .filter(|(.., head)| !head.fork)
        .filter_map(|(.., head)| head.parent.as_deref())
        .collect();
    let mut sessions: Vec<SessionInfo> = logs
        .iter()
        .filter(|(session_id, ..)| !continued.contains(session_id.as_str()))
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

fn retire_session(
    session: &mut Option<SessionState>,
    pending: &Pending,
    pending_questions: &PendingQuestions,
    pending_egress: &PendingEgress,
    pending_prompt: &PendingPrompt,
) {
    let Some(old) = session.take() else {
        return;
    };
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

/// Make `open` the connection's session, retiring whatever was there.
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
fn lock_config(config: &SharedConfig) -> std::sync::MutexGuard<'_, crate::acp_map::ConfigState> {
    config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[allow(clippy::too_many_arguments)]
fn install_session(
    open: SessionOpen,
    id: String,
    cwd: std::path::PathBuf,
    hold_drain: bool,
    writer: &Writer,
    session: &mut Option<SessionState>,
    pending: &Pending,
    pending_questions: &PendingQuestions,
    pending_egress: &PendingEgress,
    pending_prompt: &PendingPrompt,
    next_id: &mut u64,
    model: &str,
) {
    retire_session(
        session,
        pending,
        pending_questions,
        pending_egress,
        pending_prompt,
    );
    let config = Arc::new(std::sync::Mutex::new(crate::acp_map::ConfigState {
        model: open.model.clone().unwrap_or_else(|| model.to_string()),
        plan: open.plan,
        effort: crate::acp_map::effort_wire(&open),
    }));
    let lease = open.lease;
    let mut state = start_session(
        open.handle,
        config,
        id,
        open.session_id,
        writer.clone(),
        pending.clone(),
        pending_questions.clone(),
        pending_egress.clone(),
        pending_prompt.clone(),
        next_id,
        model.to_string(),
    );
    state._lease = lease;
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
    id: String,
    store_id: String,
    writer: Writer,
    pending: Pending,
    pending_questions: PendingQuestions,
    pending_egress: PendingEgress,
    pending_prompt: PendingPrompt,
    next_id: &mut u64,
    model: String,
) -> SessionState {
    // Permission/question request ids for this session are disjoint from
    // every other id.
    let req_id_seed = *next_id * 1_000_000;
    *next_id += 1;
    let events = std::mem::replace(&mut handle.events, mpsc::channel(1).1);
    let sid = id.clone();
    let drain_config = Arc::clone(&config);
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
            )
            .await;
        }
    });
    SessionState {
        id,
        store_id,
        cwd: std::path::PathBuf::new(),
        handle,
        config,
        drain,
        go: Some(go),
        _lease: None,
    }
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
        if let EngineEvent::ModelChanged { model, .. } = &event {
            lock_config(&config).model = model.clone();
            let config_options = crate::acp_map::config_options(&lock_config(&config));
            send_update(
                &writer,
                &session_id,
                SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate { config_options }),
            )
            .await;
        }
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
                send(
                    &writer,
                    &json!({
                        "jsonrpc": "2.0", "id": req_id, "method": "session/request_permission",
                        "params": to_json(&RequestPermissionParams {
                            session_id: session_id.clone(),
                            summary,
                            protected_why,
                        }),
                    }),
                )
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
                        "params": to_json(&RequestQuestionParams {
                            session_id: session_id.clone(),
                            header: question.header,
                            prompt: question.prompt,
                            options: question.options,
                            multi: question.multi,
                        }),
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
                        "params": to_json(&RequestEgressParams {
                            session_id: session_id.clone(),
                            host,
                        }),
                    }),
                )
                .await;
            }
            EngineEvent::TurnDone {
                outcome,
                usage,
                mispredictions,
            } => {
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
                // `mispredictions` is omitted at zero, exactly as the JSON
                // stream renders it (0050 T5).
                let misses = (mispredictions > 0).then_some(mispredictions);
                let done = AcpOnlyUpdate::TurnDone {
                    outcome: crate::wire::OutcomeFrame::of(&outcome),
                    mispredictions: misses,
                };
                notify(&writer, &session_id, to_json(&done)).await;
                // Take the id and drop the guard *before* awaiting (a
                // std::sync guard held across .await would make this non-Send).
                let prompt_id = pending_prompt
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front();
                if let Some(id) = prompt_id {
                    let reply = PromptResult {
                        schema_version: UPDATE_SCHEMA_VERSION,
                        outcome: crate::wire::OutcomeFrame::of(&outcome),
                        usage: crate::wire::UsageFrame::of(&model, &usage),
                        mispredictions: misses,
                    };
                    reply_ok(&writer, id, to_json(&reply)).await;
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

async fn send_update(writer: &Writer, session_id: &str, update: SessionUpdate) {
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

async fn reply_ok(writer: &Writer, id: Value, result: Value) {
    send(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
    .await;
}

async fn reply_err(writer: &Writer, id: Value, code: i64, message: &str) {
    send(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "error": ErrorObject::new(code, message)}),
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
mod drain_tests {
    use super::*;
    use hotl_types::TokenUsage;
    use tokio::io::AsyncReadExt;

    /// Run `drain_events` to completion over a pre-closed channel, capturing
    /// every line it writes.
    async fn run_drain_events_capturing_writer(rx: mpsc::Receiver<EngineEvent>) -> Vec<String> {
        let (write, mut read) = tokio::io::duplex(1 << 20);
        let writer: Writer = Arc::new(Mutex::new(Box::new(write)));
        drain_events(
            rx,
            Arc::clone(&writer),
            Pending::default(),
            PendingQuestions::default(),
            PendingEgress::default(),
            PendingPrompt::default(),
            "acp-test".into(),
            0,
            "test-model".into(),
            Arc::new(std::sync::Mutex::new(crate::acp_map::ConfigState {
                model: "test-model".into(),
                plan: false,
                effort: "default".into(),
            })),
        )
        .await;
        drop(writer); // close the duplex so read_to_string sees EOF
        let mut buf = String::new();
        read.read_to_string(&mut buf).await.unwrap();
        buf.lines().map(str::to_string).collect()
    }

    fn turn_done() -> EngineEvent {
        EngineEvent::TurnDone {
            outcome: Outcome::Done { text: "ok".into() },
            usage: TokenUsage::default(),
            mispredictions: 0,
        }
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
                match v["params"]["update"]["type"].as_str()? {
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
mod ask_reply_tests {
    use super::*;

    /// 0076 D5: every reply shape the schema documents parses, through the
    /// real lenient parser, to the answer the schema says it means — and
    /// validates against the published reply defs.
    #[test]
    fn documented_client_replies_parse_as_the_schema_says() {
        use hotl_types::QuestionAnswer;
        let ask = |v: Value| ask_reply_from_result(Some(&v));
        assert!(
            matches!(ask(json!({"respond": "use main"})), AskReply::Respond { content } if content == "use main")
        );
        assert!(matches!(
            ask(json!({"input": {"path": "a"}})),
            AskReply::AllowEdited { .. }
        ));
        assert!(matches!(ask(json!({"allow": true})), AskReply::Allow));
        assert!(matches!(
            ask(json!({"allow": true, "secretReads": true})),
            AskReply::AllowWithSecretReads
        ));
        assert!(
            matches!(ask(json!({"allow": false, "message": "no"})), AskReply::Deny { message: Some(m) } if m == "no")
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
        assert!(matches!(
            egress_decision_from_result(Some(&json!({"allow": true}))),
            hotl_tools::net::EgressDecision::Allow
        ));
        let schema = acp_schema();
        let defs = &schema["$defs"];
        for (def, example) in [
            ("PermissionReply", json!({"respond": "use main"})),
            ("PermissionReply", json!({"input": {"path": "a"}})),
            (
                "PermissionReply",
                json!({"allow": true, "secretReads": true}),
            ),
            ("PermissionReply", json!({"allow": false, "message": "no"})),
            ("QuestionReply", json!({"selected": ["MVP"]})),
            ("QuestionReply", json!({"freeText": "later"})),
            ("EgressReply", json!({"allow": true})),
        ] {
            let mut sub = defs[def].clone();
            sub["$defs"] = defs.clone();
            let v = jsonschema::validator_for(&sub).expect("a valid schema");
            assert!(v.is_valid(&example), "{def} rejects {example}");
        }
    }

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

// --- Typed payloads (0076 D2) -------------------------------------------------
//
// Every result and notification body this server writes, as types, so the
// published schema (`acp_schema`) is generated from what is actually sent.
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
    pub extensions: Vec<String>,
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
            extensions: EXTENSION_METHODS.iter().map(|m| m.to_string()).collect(),
        };
        let mut m = serde_json::Map::new();
        m.insert(META_INIT.into(), to_json(&init));
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

/// The `session/prompt` result, sent when the turn ends.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub schema_version: u32,
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

/// `session/reload_config` with no open session.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReloadAck {
    pub ok: bool,
    pub model: String,
    pub warnings: Vec<String>,
}

/// `session/reload_config` with an open session (it is re-opened).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReloadSessionAck {
    pub ok: bool,
    pub session_id: String,
    pub model: String,
    pub mode: String,
    pub plan: bool,
    pub context_window: u64,
    pub warnings: Vec<String>,
}

/// `session/steer` and `session/aside`: `false` = there was no turn to queue on.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Queued {
    pub queued: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Cancelled {
    pub cancelled: bool,
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
    /// The turn ended; the usage rides the prompt result instead.
    TurnDone {
        outcome: crate::wire::OutcomeFrame,
        #[serde(skip_serializing_if = "Option::is_none")]
        mispredictions: Option<u32>,
    },
    MemoryReport {
        mode: String,
        path: String,
        bytes: usize,
        cap: usize,
        entries: Vec<MemoryEntryRow>,
    },
    ConfigReloaded {
        model: String,
        mode: String,
        plan: bool,
        context_window: u64,
        skills: Vec<RosterEntry>,
        workflows: Vec<RosterEntry>,
        warnings: Vec<String>,
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

/// Any `session/update` body: a stream frame or an ACP-only kind. The
/// `workflows_report` body is the workflow registry's own JSON (untyped here).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)] // schema-only: `notify` sends the two halves as values
pub enum AcpUpdate {
    Frame(crate::wire::Frame),
    Acp(AcpOnlyUpdate),
}

/// `session/update` params.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    pub schema_version: u32,
    pub session_id: String,
    pub update: AcpUpdate,
}

/// Server → client `session/request_permission` params.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: String,
    pub summary: String,
    /// Why a protected path is involved, when one is.
    pub protected_why: Option<String>,
}

/// Server → client `session/request_question` params.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestQuestionParams {
    pub session_id: String,
    pub header: String,
    pub prompt: String,
    pub options: Vec<hotl_types::QuestionOption>,
    pub multi: bool,
}

/// Server → client `session/request_egress` params.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestEgressParams {
    pub session_id: String,
    pub host: String,
}

/// What a client answers `session/request_permission` with. Schema only:
/// `ask_reply_from_result` reads these spellings in this priority (0076 D5).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)] // schema-only
pub enum PermissionReply {
    /// A typed answer for an ask that wants one.
    Respond { respond: String },
    /// Allow with edited tool input.
    Input { input: Value },
    Allow {
        allow: bool,
        #[serde(rename = "secretReads", skip_serializing_if = "Option::is_none")]
        secret_reads: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// What a client answers `session/request_question` with. Schema only.
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

/// What a client answers `session/request_egress` with. Schema only.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[allow(dead_code)] // schema-only
pub struct EgressReply {
    pub allow: bool,
}

/// Index type: exists so `schema_for!` puts every payload in `$defs`.
#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // schema-only
pub struct AcpPayloads {
    initialize: InitializeResponse,
    new_session: NewSessionResponse,
    load_session: LoadSessionResponse,
    resume_session: ResumeSessionResponse,
    list_sessions: ListSessionsResponse,
    session_notification: SessionNotification,
    session_open_extras: SessionOpenExtras,
    prompt_result: PromptResult,
    session_update: SessionUpdateParams,
    ack: Ack,
    model_ack: ModelAck,
    goal_ack: GoalAck,
    stopped_ack: StoppedAck,
    reload_ack: ReloadAck,
    reload_session_ack: ReloadSessionAck,
    queued: Queued,
    cancelled: Cancelled,
    request_permission: RequestPermissionParams,
    request_question: RequestQuestionParams,
    request_egress: RequestEgressParams,
    permission_reply: PermissionReply,
    question_reply: QuestionReply,
    egress_reply: EgressReply,
}

/// Where the ACP schema is published (0076 D3).
pub const ACP_SCHEMA_ID: &str = "https://nrakochy.github.io/hotl/schema/acp-v1.schema.json";

/// The published schema of every ACP payload, `$id` set to its URL.
pub fn acp_schema() -> Value {
    let mut v = serde_json::to_value(schemars::schema_for!(AcpPayloads))
        .expect("a schema always serializes");
    v["$id"] = json!(ACP_SCHEMA_ID);
    v["title"] = json!("hotl ACP-dialect payloads");
    v
}

fn to_json<T: Serialize>(payload: &T) -> Value {
    serde_json::to_value(payload).expect("an ACP payload always serializes")
}
