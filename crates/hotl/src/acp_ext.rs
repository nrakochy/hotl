pub mod codes {
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const AUTH_REQUIRED: i64 = -32000;
    pub const NOT_FOUND: i64 = -32002;
    pub const OUTPUT_INVALID: i64 = -32003;
}

pub const EXT_PREFIX: &str = "_hotl/";

pub const META_EXT: &str = "hotl.dev/extensions";

pub const META_SOCKET_TOKEN: &str = "hotl.dev/socketToken";

pub fn ext(method: &str) -> String {
    format!("{EXT_PREFIX}{method}")
}

use schemars::{JsonSchema, SchemaGenerator};
use serde::Serialize;
use serde_json::{json, Value};

use crate::acp::{
    Ack, ExtSessionUpdateParams, GoalAck, HotlInit, ModelAck, Queued, ReloadAck, ReloadSessionAck,
    RequestQuestionParams, SessionOpenExtras, StoppedAck, TurnMeta,
};

pub const SCHEMA_ID: &str =
    "https://nrakochy.github.io/hotl/schema/acp-hotl-extensions.schema.json";

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelChange {
    pub model: String,
    pub previous: String,
    pub context_tokens: u64,
    pub context_window: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Flagged {
    pub why: String,
    pub denied: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Egress {
    pub host: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AskDetail {
    pub summary: String,
    pub protected_why: Option<String>,
    pub diff: Vec<DiffRow>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffRow {
    #[schemars(description = "ctx, add, del, or trailer (the \"[+N more lines]\" row)")]
    pub op: String,
    pub text: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct OpenParams {
    #[schemars(description = "A session name, 1 to 64 characters")]
    name: Option<String>,
    #[schemars(description = "session/new only: run the session in a new git worktree")]
    worktree: Option<String>,
    #[schemars(description = "session/load and session/resume only: do not reuse the worktree")]
    no_worktree: Option<bool>,
    #[schemars(
        description = "session/new only: a throwaway session whose log is never stored; cannot be combined with worktree"
    )]
    ephemeral: Option<bool>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Reply {
    #[schemars(description = "With the deny option: a reason the model reads")]
    message: Option<String>,
    #[schemars(
        description = "With an allow option: skip the tool and send this text as its result"
    )]
    respond: Option<String>,
    #[schemars(description = "With an allow option: run the tool with this input instead")]
    input: Option<Value>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ImageParam {
    media_type: String,
    #[schemars(description = "Base64 image bytes")]
    data: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ForkParams {
    session_id: String,
    #[schemars(description = "Absolute path; defaults to the agent's working directory")]
    cwd: Option<String>,
    #[schemars(description = "Same shape as session/new's mcpServers")]
    mcp_servers: Option<Vec<Value>>,
    name: Option<String>,
    worktree: Option<String>,
    #[schemars(description = "Keep only the first N history items; not with keepTurns")]
    keep_items: Option<u64>,
    #[schemars(description = "Keep only the first N turns; not with keepItems")]
    keep_turns: Option<u64>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(
    description = "The same result as session/new: sessionId, modes, configOptions and _meta[\"hotl.dev/open\"]"
)]
#[allow(dead_code)]
struct ForkResult {
    session_id: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct SessionParams {
    session_id: Option<String>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct RenameParams {
    session_id: Option<String>,
    #[schemars(description = "1 to 64 characters after trimming")]
    name: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ColorParams {
    session_id: Option<String>,
    #[schemars(description = "A color name, or null to clear")]
    color: Option<String>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GoalParams {
    session_id: Option<String>,
    #[schemars(description = "1 to 4000 characters, or null to clear")]
    goal: Option<String>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct RememberParams {
    session_id: Option<String>,
    #[schemars(
        description = "One line to save to memory after the owner approves it; without a note the reply is a memory_report"
    )]
    note: Option<String>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct SteerParams {
    session_id: Option<String>,
    text: String,
    images: Option<Vec<ImageParam>>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct FoldParams {
    session_id: Option<String>,
    #[schemars(description = "What the summary should keep, 1 to 4000 characters")]
    focus: Option<String>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct AsideParams {
    session_id: Option<String>,
    question: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct JobStopParams {
    session_id: Option<String>,
    job: String,
}

#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct ChildStopParams {
    session_id: Option<String>,
    child: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct WorkflowsReport {
    #[serde(rename = "type")]
    #[schemars(description = "Always workflows_report")]
    kind: String,
    runs: Vec<serde_json::Map<String, Value>>,
}

fn sub<T: JsonSchema>(g: &mut SchemaGenerator) -> Value {
    g.subschema_for::<T>().to_value()
}

fn def(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

fn one_of(names: &[&str]) -> Value {
    json!({ "oneOf": names.iter().map(|n| def(n)).collect::<Vec<_>>() })
}

fn camel(tag: &str) -> String {
    tag.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect()
}

fn split_tagged(defs: &mut serde_json::Map<String, Value>, name: &str) {
    let Some(whole) = defs.remove(name) else {
        return;
    };
    for variant in whole["oneOf"].as_array().into_iter().flatten() {
        if let Some(tag) = variant
            .pointer("/properties/type/const")
            .and_then(Value::as_str)
        {
            defs.insert(camel(tag), variant.clone());
        }
    }
}

fn request(from: &str, params: Value, result: Value) -> Value {
    json!({"from": from, "params": params, "result": result})
}

pub fn schema() -> Value {
    let mut g = SchemaGenerator::default();
    let g = &mut g;
    let session = sub::<SessionParams>(g);
    let ack = sub::<Ack>(g);
    let queued = sub::<Queued>(g);
    let stopped = sub::<StoppedAck>(g);
    sub::<crate::acp::AcpOnlyUpdate>(g);
    let mut reload = request(
        "client",
        session.clone(),
        json!({ "oneOf": [sub::<ReloadAck>(g), sub::<ReloadSessionAck>(g)] }),
    );
    reload["errorData"] = def("ConfigReloadFailed");
    let methods = json!({
        "_hotl/session/fork": request("client", sub::<ForkParams>(g), sub::<ForkResult>(g)),
        "_hotl/session/rename": request("client", sub::<RenameParams>(g), ack.clone()),
        "_hotl/session/set_color": request("client", sub::<ColorParams>(g), ack.clone()),
        "_hotl/session/set_goal": request("client", sub::<GoalParams>(g), sub::<GoalAck>(g)),
        "_hotl/session/remember": request("client", sub::<RememberParams>(g), one_of(&["Ack", "MemoryReport"])),
        "_hotl/session/steer": request("client", sub::<SteerParams>(g), queued.clone()),
        "_hotl/session/fold": request("client", sub::<FoldParams>(g), queued.clone()),
        "_hotl/session/reload_config": reload,
        "_hotl/session/aside": request("client", sub::<AsideParams>(g), ack.clone()),
        "_hotl/session/context": request("client", session.clone(), def("ContextReport")),
        "_hotl/session/workflows": request("client", session.clone(), sub::<WorkflowsReport>(g)),
        "_hotl/session/jobs": request("client", session.clone(), def("JobsReport")),
        "_hotl/session/jobs/stop": request("client", sub::<JobStopParams>(g), stopped.clone()),
        "_hotl/session/children": request("client", session.clone(), def("ChildrenReport")),
        "_hotl/session/children/stop": request("client", sub::<ChildStopParams>(g), stopped),
        "_hotl/session/stop": request("client", session, ack),
        "_hotl/session/update": {"from": "agent", "notification": true, "params": sub::<ExtSessionUpdateParams>(g)},
        "_hotl/session/request_question": request("agent", sub::<RequestQuestionParams>(g), sub::<crate::acp::QuestionReply>(g)),
    });
    let at = |on: &str, schema: Value| json!({"on": on, "schema": schema});
    let frame = sub::<crate::wire::Frame>(g);
    let meta = json!({
        (META_EXT): [at("initialize params: clientCapabilities._meta", json!({"const": 1}))],
        (META_SOCKET_TOKEN): [at("initialize params: _meta (hotl serve socket only)", json!({"type": "string"}))],
        (crate::acp::META_INIT): [at("initialize result: agentCapabilities._meta", sub::<HotlInit>(g))],
        (crate::acp::META_METHODS): [at("initialize result: agentCapabilities._meta", json!({"type": "array", "items": {"type": "string"}}))],
        (crate::acp::META_OPEN): [
            at("session/new, session/load, session/resume params: _meta", sub::<OpenParams>(g)),
            at("session/new, session/load, session/resume, _hotl/session/fork result: _meta", sub::<SessionOpenExtras>(g)),
        ],
        (crate::acp_map::META_RETRY): [at("session/update notice: _meta (client offered session.notices)", sub::<crate::acp_types::RetryMeta>(g))],
        (crate::acp::META_SESSION): [at("session/list result: sessions[]._meta", sub::<crate::acp_types::SessionRowMeta>(g))],
        (crate::acp::META_MODEL): [
            at("session/update config_option_update: _meta", sub::<ModelChange>(g)),
            at("session/set_config_option result: _meta", sub::<ModelAck>(g)),
        ],
        (crate::acp::META_OUTPUT_SCHEMA): [
            at("initialize result: agentCapabilities._meta", json!({"const": true})),
            at("session/prompt params: _meta", json!({"type": "object", "description": "A JSON Schema the answer must fit; the reply carries the parsed answer in hotl.dev/output"})),
        ],
        (crate::acp::META_OUTPUT): [at("session/prompt result: _meta (every client)", json!({"description": "The answer as JSON, valid against the request's hotl.dev/outputSchema"}))],
        (crate::acp::META_TURN): [
            at("session/prompt result: _meta", sub::<TurnMeta>(g)),
            at("session/prompt error: error.data._meta", sub::<TurnMeta>(g)),
        ],
        (crate::acp_map::META_FRAME): [at("session/update: update._meta", frame.clone())],
        (crate::acp_map::META_DENIED): [at("session/update tool_call_update: _meta", json!({"const": true}))],
        (crate::acp_map::META_AUTO_ALLOWED): [at("session/update tool_call_update: _meta", json!({"type": "string", "description": "The allow rule that let the call run"}))],
        (crate::acp_map::META_FLAGGED): [at("session/update tool_call_update: _meta", sub::<Flagged>(g))],
        (crate::acp_map::META_CHILD): [at("session/update tool_call_update of a spawn call: _meta", frame)],
        (crate::acp_map::META_PROTECTED_WHY): [at("session/request_permission params: toolCall._meta", json!({"type": "string"}))],
        (crate::acp_map::META_ASK): [at("session/request_permission params: toolCall._meta", sub::<AskDetail>(g))],
        (crate::acp_map::META_EGRESS): [at("session/request_permission params: toolCall._meta", sub::<Egress>(g))],
        (crate::acp_map::META_REPLY): [at("session/request_permission result (sent by the client): _meta", sub::<Reply>(g))],
        (crate::acp_map::META_QUESTION): [at("elicitation/create params: _meta", sub::<hotl_types::Question>(g))],
    });
    let mut defs = g.take_definitions(true);
    split_tagged(&mut defs, "AcpOnlyUpdate");
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": SCHEMA_ID,
        "title": "hotl ACP extensions",
        "description": "What hotl adds to ACP v1: the _hotl/ methods and the hotl.dev/ _meta keys. A client sees them only after it sets clientCapabilities._meta[\"hotl.dev/extensions\"] to 1 in initialize, except hotl.dev/output, which answers any client that sent hotl.dev/outputSchema. Every standard ACP message follows the ACP v1 schema.",
        "methods": methods,
        "meta": meta,
        "$defs": defs,
    })
}
