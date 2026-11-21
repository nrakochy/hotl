use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Meta = serde_json::Map<String, Value>;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    #[allow(dead_code)]
    #[serde(default)]
    pub protocol_version: Option<Value>,
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    #[allow(dead_code)]
    #[serde(default)]
    pub client_info: Option<Implementation>,
    #[allow(dead_code)]
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[allow(dead_code)]
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub session: Option<ClientSessionCapabilities>,
    #[serde(default)]
    pub auth: ClientAuthCapabilities,
    #[serde(default)]
    pub elicitation: Option<ElicitationCapabilities>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSessionCapabilities {
    #[serde(default)]
    pub compaction: Option<Meta>,
    #[serde(default)]
    pub notices: Option<Meta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientAuthCapabilities {
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationCapabilities {
    #[serde(default)]
    pub form: Option<Meta>,
    #[allow(dead_code)]
    #[serde(default)]
    pub url: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u16,
    pub agent_capabilities: AgentCapabilities,
    pub auth_methods: Vec<AuthMethod>,
    pub agent_info: Implementation,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AuthMethod {
    Agent {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Meta>,
    },
    Terminal {
        id: String,
        name: String,
        args: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        env: Option<std::collections::BTreeMap<String, String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
        meta: Option<Meta>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateRequest {
    #[serde(default)]
    pub method_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub load_session: bool,
    pub prompt_capabilities: PromptCapabilities,
    pub mcp_capabilities: McpCapabilities,
    pub session_capabilities: SessionCapabilities,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpCapabilities {
    pub http: bool,
    pub sse: bool,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct EmptyCapability {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<EmptyCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<EmptyCapability>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close: Option<EmptyCapability>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: std::path::PathBuf,
    #[allow(dead_code)]
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServer>>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    #[allow(dead_code)]
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServer>>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionRequest {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    #[allow(dead_code)]
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServer>>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloseSessionRequest {
    pub session_id: String,
    #[allow(dead_code)]
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServer {
    Http(McpServerHttp),
    Sse(McpServerHttp),
    #[serde(untagged)]
    Stdio(McpServerStdio),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerHttp {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<HttpHeader>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStdio {
    pub name: String,
    pub command: std::path::PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVariable>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetryMeta {
    #[schemars(description = "Which try is about to be made, counting from 1")]
    pub attempt: u32,
    #[schemars(description = "How many tries the ladder allows")]
    pub max: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "The HTTP status that caused the retry, when there was one")]
    pub status: Option<u16>,
    #[schemars(description = "Milliseconds hotl waits before the next try")]
    pub delay_ms: u64,
    #[schemars(
        description = "Which retry ladder this is: sample, summarize, goal_eval, title or aside"
    )]
    pub scope: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRowMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "The listed session this one was forked from")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "The run id of the live hotl serve process holding this session")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    pub current_mode_id: String,
    pub available_modes: Vec<SessionMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    UserMessageChunk(ContentChunk),
    AgentMessageChunk(ContentChunk),
    AgentThoughtChunk(ContentChunk),
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    Plan(Plan),
    AvailableCommandsUpdate(AvailableCommandsUpdate),
    UsageUpdate(UsageUpdate),
    SessionInfoUpdate(SessionInfoUpdate),
    CurrentModeUpdate(CurrentModeUpdate),
    ConfigOptionUpdate(ConfigOptionUpdate),
    Notice(Notice),
    CompactionUpdate(CompactionUpdate),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub severity: NoticeSeverity,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ContentChunk {
    pub content: ContentBlock,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextContent),
    Image(ImageContent),
    Audio(AudioContent),
    ResourceLink(ResourceLink),
    Resource(EmbeddedResource),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent { text: text.into() })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TextContent {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioContent {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLink {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EmbeddedResource {
    pub resource: EmbeddedResourceResource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum EmbeddedResourceResource {
    TextResourceContents(TextResourceContents),
    BlobResourceContents(BlobResourceContents),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextResourceContents {
    pub uri: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlobResourceContents {
    pub uri: String,
    pub blob: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    pub prompt: Vec<ContentBlock>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TurnErrorData {
    #[serde(rename = "_meta")]
    pub meta: Meta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    #[serde(default)]
    pub session_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolCallContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolCallLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(flatten)]
    pub fields: ToolCallUpdateFields,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdateFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolCallContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ToolCallLocation {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    #[allow(dead_code)]
    Delete,
    #[allow(dead_code)]
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    #[allow(dead_code)]
    SwitchMode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content(Content),
    Diff(Diff),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Diff {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Content {
    pub content: ContentBlock,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    #[allow(dead_code)]
    High,
    Medium,
    #[allow(dead_code)]
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommandsUpdate {
    pub available_commands: Vec<AvailableCommand>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<AvailableCommandInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum AvailableCommandInput {
    Unstructured(UnstructuredCommandInput),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct UnstructuredCommandInput {
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct UsageUpdate {
    pub used: u64,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    InProgress,
    Completed,
    #[allow(dead_code)]
    Failed,
    #[allow(dead_code)]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactionUpdate {
    pub compaction_id: String,
    pub status: CompactionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CurrentModeUpdate {
    pub current_mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionUpdate {
    pub config_options: Vec<SessionConfigOption>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigOption {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<SessionConfigOptionCategory>,
    #[serde(flatten)]
    pub kind: SessionConfigKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionConfigOptionCategory {
    #[allow(dead_code)]
    Mode,
    Model,
    #[allow(dead_code)]
    ModelConfig,
    ThoughtLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigKind {
    Select(SessionConfigSelect),
    Boolean(SessionConfigBoolean),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelect {
    pub current_value: String,
    pub options: Vec<SessionConfigSelectOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigSelectOption {
    pub value: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigBoolean {
    pub current_value: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModeRequest {
    #[serde(default)]
    pub mode_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionRequest {
    #[serde(default)]
    pub config_id: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionConfigOptionResponse {
    pub config_options: Vec<SessionConfigOption>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "meta_or_none"
    )]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RequestPermissionOutcome {
    Cancelled,
    Selected(SelectedPermissionOutcome),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SelectedPermissionOutcome {
    pub option_id: String,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "meta_or_none"
    )]
    pub meta: Option<Meta>,
}

fn meta_or_none<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Meta>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Object(m) => Some(m),
        _ => None,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateElicitationRequest {
    #[serde(flatten)]
    pub mode: ElicitationMode,
    pub message: String,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ElicitationMode {
    Form(ElicitationFormMode),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ElicitationFormMode {
    pub session_id: String,
    pub requested_schema: ElicitationSchema,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ElicitationSchema {
    #[serde(rename = "type")]
    pub type_: ElicitationSchemaType,
    pub properties: std::collections::BTreeMap<String, ElicitationPropertySchema>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationSchemaType {
    Object,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ElicitationPropertySchema {
    String(StringPropertySchema),
    Array(MultiSelectPropertySchema),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct StringPropertySchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct MultiSelectPropertySchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub items: MultiSelectItems,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MultiSelectItems {
    String(StringMultiSelectItems),
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct StringMultiSelectItems {
    #[serde(rename = "enum")]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct CreateElicitationResponse {
    #[serde(flatten)]
    pub action: ElicitationAction,
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "meta_or_none"
    )]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ElicitationAction {
    Accept(ElicitationAcceptAction),
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ElicitationAcceptAction {
    #[serde(default)]
    pub content: Option<std::collections::BTreeMap<String, ElicitationContentValue>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ElicitationContentValue {
    String(String),
    Integer(i64),
    Number(f64),
    Boolean(bool),
    StringArray(Vec<String>),
}

pub mod client {
    use serde::de::{DeserializeOwned, Deserializer};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use super::{
        ContentBlock, ErrorObject, Implementation, McpServer, PermissionOptionKind, StopReason,
        ToolCallStatus,
    };

    pub const JSONRPC: &str = "2.0";

    #[derive(Debug, Clone, Default, PartialEq)]
    pub enum Field<T> {
        #[default]
        Absent,
        Invalid,
        Valid(T),
    }

    impl<T: DeserializeOwned> Field<T> {
        pub fn from_value(v: Value) -> Self {
            if v.is_array() {
                return Self::Invalid;
            }
            match serde_json::from_value(v) {
                Ok(t) => Self::Valid(t),
                Err(_) => Self::Invalid,
            }
        }
    }

    impl<T> Field<T> {
        pub fn valid(&self) -> Option<&T> {
            match self {
                Self::Valid(t) => Some(t),
                _ => None,
            }
        }

        pub fn is_absent(&self) -> bool {
            matches!(self, Self::Absent)
        }
    }

    impl<'de, T: DeserializeOwned> Deserialize<'de> for Field<T> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            Ok(Self::from_value(Value::deserialize(d)?))
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub struct Items<T>(pub Vec<Field<T>>);

    impl<T> Default for Items<T> {
        fn default() -> Self {
            Self(Vec::new())
        }
    }

    impl<T> Items<T> {
        pub fn valid(&self) -> impl Iterator<Item = &T> {
            self.0.iter().filter_map(Field::valid)
        }
    }

    impl<'de, T: DeserializeOwned> Deserialize<'de> for Items<T> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            Ok(match Value::deserialize(d)? {
                Value::Array(a) => Self(a.into_iter().map(Field::from_value).collect()),
                _ => Self::default(),
            })
        }
    }

    fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        Value::deserialize(d).map(Some)
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct Request<'a, P> {
        pub jsonrpc: &'static str,
        pub id: u64,
        pub method: &'a str,
        pub params: P,
    }

    impl<'a, P> Request<'a, P> {
        pub fn new(id: u64, method: &'a str, params: P) -> Self {
            Self {
                jsonrpc: JSONRPC,
                id,
                method,
                params,
            }
        }
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct Notification<'a, P> {
        pub jsonrpc: &'static str,
        pub method: &'a str,
        pub params: P,
    }

    impl<'a, P> Notification<'a, P> {
        pub fn new(method: &'a str, params: P) -> Self {
            Self {
                jsonrpc: JSONRPC,
                method,
                params,
            }
        }
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct Response<'a, R> {
        pub jsonrpc: &'static str,
        pub id: &'a Value,
        pub result: R,
    }

    impl<'a, R> Response<'a, R> {
        pub fn new(id: &'a Value, result: R) -> Self {
            Self {
                jsonrpc: JSONRPC,
                id,
                result,
            }
        }
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct ErrorResponse<'a> {
        pub jsonrpc: &'static str,
        pub id: &'a Value,
        pub error: ErrorObject,
    }

    impl<'a> ErrorResponse<'a> {
        pub fn new(id: &'a Value, error: ErrorObject) -> Self {
            Self {
                jsonrpc: JSONRPC,
                id,
                error,
            }
        }
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct Message {
        #[serde(default)]
        pub method: Field<String>,
        #[serde(default, deserialize_with = "present")]
        pub id: Option<Value>,
        #[serde(default)]
        pub params: Option<Value>,
        #[serde(default)]
        pub result: Option<Value>,
        #[serde(default)]
        pub error: Field<ResponseError>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct ResponseError {
        #[serde(default)]
        pub message: Field<String>,
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct InitializeRequest {
        pub protocol_version: u64,
        pub client_capabilities: ClientCapabilities,
        pub client_info: Implementation,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct ClientCapabilities {
        pub fs: FileSystemCapability,
        pub terminal: bool,
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct FileSystemCapability {
        pub read_text_file: bool,
        pub write_text_file: bool,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct InitializeResponse {
        #[serde(default)]
        pub protocol_version: Field<u64>,
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct NewSessionRequest {
        pub cwd: std::path::PathBuf,
        pub mcp_servers: Vec<McpServer>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct NewSessionResponse {
        #[serde(default)]
        pub session_id: Field<String>,
        #[serde(default)]
        pub modes: Field<SessionModeState>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct SessionModeState {
        #[serde(default)]
        pub available_modes: Items<SessionMode>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct SessionMode {
        #[serde(default)]
        pub id: Field<String>,
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct SetSessionModeRequest {
        pub session_id: String,
        pub mode_id: String,
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PromptRequest {
        pub session_id: String,
        pub prompt: Vec<ContentBlock>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PromptResponse {
        #[serde(default)]
        pub stop_reason: Field<String>,
    }

    impl PromptResponse {
        pub fn stop_reason(&self) -> Result<StopReason, Option<&str>> {
            let raw = self.stop_reason.valid().map(String::as_str);
            match raw.map(|r| Field::<StopReason>::from_value(Value::from(r))) {
                Some(Field::Valid(reason)) => Ok(reason),
                _ => Err(raw),
            }
        }
    }

    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CancelNotification {
        pub session_id: String,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct SessionNotification {
        #[serde(default)]
        pub update: Field<SessionUpdate>,
    }

    #[derive(Debug, Clone, Deserialize)]
    #[serde(tag = "sessionUpdate", rename_all = "snake_case")]
    pub enum SessionUpdate {
        AgentMessageChunk(ContentChunk),
        AgentThoughtChunk(ContentChunk),
        ToolCall(ToolCallUpdate),
        ToolCallUpdate(ToolCallUpdate),
        UsageUpdate(UsageUpdate),
        #[serde(other)]
        Other,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct ContentChunk {
        #[serde(default)]
        pub content: Field<ContentBlock>,
    }

    impl ContentChunk {
        pub fn text(&self) -> Option<&str> {
            match self.content.valid()? {
                ContentBlock::Text(t) => Some(&t.text),
                _ => None,
            }
        }
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ToolCallUpdate {
        #[serde(default)]
        pub tool_call_id: Field<String>,
        #[serde(default)]
        pub name: Field<String>,
        #[serde(default)]
        pub kind: Field<String>,
        #[serde(default)]
        pub title: Field<String>,
        #[serde(default)]
        pub status: Field<ToolCallStatus>,
        #[serde(default)]
        pub raw_input: Option<Value>,
        #[serde(default)]
        pub locations: Items<ToolCallLocation>,
        #[serde(default)]
        pub content: Items<ToolCallContent>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct ToolCallLocation {
        #[serde(default)]
        pub path: Field<String>,
    }

    #[derive(Debug, Clone, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ToolCallContent {
        Diff(DiffContent),
        #[serde(other)]
        Other,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct DiffContent {
        #[serde(default)]
        pub path: Field<String>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct UsageUpdate {
        #[serde(default)]
        pub cost: Field<Cost>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct Cost {
        #[serde(default, deserialize_with = "present")]
        pub amount: Option<Value>,
        #[serde(default)]
        pub currency: Field<String>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct RequestPermissionRequest {
        #[serde(default)]
        pub tool_call: Field<ToolCallUpdate>,
        #[serde(default)]
        pub options: Items<PermissionOption>,
    }

    impl RequestPermissionRequest {
        pub fn option(&self, kind: PermissionOptionKind) -> Option<&str> {
            self.options
                .valid()
                .find(|o| o.kind.valid() == Some(&kind))?
                .option_id
                .valid()
                .map(String::as_str)
        }
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PermissionOption {
        #[serde(default)]
        pub option_id: Field<String>,
        #[serde(default)]
        pub kind: Field<PermissionOptionKind>,
    }
}
