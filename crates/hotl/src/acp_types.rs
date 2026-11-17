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
