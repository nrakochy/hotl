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
    #[serde(default)]
    pub protocol_version: Option<Value>,
    #[serde(default)]
    pub client_capabilities: ClientCapabilities,
    #[serde(default)]
    pub client_info: Option<Implementation>,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
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
    #[serde(default, rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CloseSessionRequest {
    pub session_id: String,
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
    Plan(Plan),
    AvailableCommandsUpdate(AvailableCommandsUpdate),
    UsageUpdate(UsageUpdate),
    SessionInfoUpdate(SessionInfoUpdate),
    CurrentModeUpdate(CurrentModeUpdate),
    ConfigOptionUpdate(ConfigOptionUpdate),
    Notice(Notice),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
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
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Content {
    pub content: ContentBlock,
}

#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
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
    High,
    Medium,
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
    Mode,
    Model,
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
