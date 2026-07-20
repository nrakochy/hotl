//! The `mcp` meta-tool: one registry entry covers every configured server
//! (deferred loading, corpus 09). `{server}` lists a server's tools;
//! `{server, tool, arguments}` calls one. Results, listings, and errors all
//! pass the sanitizer chokepoint; first use of a server is a protected ask.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use hotl_tools::{Permission, Tool, ToolOutcome};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::client::Client;
use crate::config::ServerConfig;
use crate::sanitize::sanitize;
use crate::trust::{binary_hash, TrustStore};

type Connector =
    Box<dyn Fn(ServerConfig) -> BoxFuture<'static, Result<Arc<Client>, String>> + Send + Sync>;

pub struct McpTool {
    servers: Vec<ServerConfig>,
    clients: tokio::sync::Mutex<HashMap<String, Arc<Client>>>,
    trust: Mutex<TrustStore>,
    /// Binary hash per server, computed once and reused for the trust screen
    /// and the recorded grant (H-07): the value the user is shown is exactly
    /// the value persisted, and the file isn't re-read on every call.
    hashes: Mutex<HashMap<String, String>>,
    connector: Connector,
    /// Leaked once at construction: the Tool trait hands out &'static str.
    description: &'static str,
}

impl McpTool {
    pub fn new(servers: Vec<ServerConfig>, trust: TrustStore) -> Self {
        Self::with_connector(
            servers,
            trust,
            Box::new(|cfg| {
                Box::pin(async move {
                    let client = Client::connect(&cfg.command, &cfg.args)?;
                    client.initialize().await?;
                    Ok(client)
                })
            }),
        )
    }

    /// Tests inject an in-process transport here.
    pub fn with_connector(servers: Vec<ServerConfig>, trust: TrustStore, connector: Connector) -> Self {
        let listing = servers
            .iter()
            .map(|s| format!("`{}` ({})", s.name, s.description))
            .collect::<Vec<_>>()
            .join(", ");
        let description = Box::leak(
            format!(
                "Call tools on the user's configured MCP servers: {listing}. \
                 Call with only {{\"server\"}} first to see that server's tools; \
                 then {{\"server\", \"tool\", \"arguments\"}} to invoke one."
            )
            .into_boxed_str(),
        );
        Self {
            servers,
            clients: tokio::sync::Mutex::new(HashMap::new()),
            trust: Mutex::new(trust),
            hashes: Mutex::new(HashMap::new()),
            connector,
            description,
        }
    }

    fn server(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// Cached binary hash for a server (computed once per session).
    fn hash_of(&self, cfg: &ServerConfig) -> String {
        let mut cache = self.hashes.lock().expect("hash cache");
        cache
            .entry(cfg.name.clone())
            .or_insert_with(|| binary_hash(&cfg.command))
            .clone()
    }

    async fn ensure_client(&self, cfg: &ServerConfig) -> Result<Arc<Client>, String> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&cfg.name) {
            return Ok(client.clone());
        }
        let client = (self.connector)(cfg.clone()).await?;
        // Reaching run() means the (protected) ask was approved upstream —
        // record the grant now, keyed to the *same* hash the screen showed
        // (H-07: shown value == recorded value, from one read).
        self.trust
            .lock()
            .expect("trust mutex")
            .record(&cfg.name, &self.hash_of(cfg));
        clients.insert(cfg.name.clone(), client.clone());
        Ok(client.clone())
    }

    async fn run_impl(&self, input: Value) -> ToolOutcome {
        let Some(server_name) = input.get("server").and_then(Value::as_str) else {
            return ToolOutcome::err("`server` is required. See the tool description for configured servers.");
        };
        let Some(cfg) = self.server(server_name) else {
            let known: Vec<_> = self.servers.iter().map(|s| s.name.as_str()).collect();
            return ToolOutcome::err(format!(
                "Unknown MCP server `{server_name}`. Configured servers: {}.",
                known.join(", ")
            ));
        };
        let client = match self.ensure_client(cfg).await {
            Ok(c) => c,
            Err(e) => return ToolOutcome::err(sanitize(server_name, "connect", &e)),
        };
        match input.get("tool").and_then(Value::as_str) {
            None => self.list(server_name, &client).await,
            Some(tool) => {
                let arguments = input.get("arguments").cloned().unwrap_or(json!({}));
                match client.call_tool(tool, arguments).await {
                    Ok((text, is_error)) => ToolOutcome {
                        content: sanitize(server_name, tool, &text),
                        is_error,
                    },
                    Err(e) => ToolOutcome::err(sanitize(server_name, tool, &e)),
                }
            }
        }
    }

    async fn list(&self, server: &str, client: &Client) -> ToolOutcome {
        match client.list_tools().await {
            Ok(tools) => {
                let mut out = String::new();
                for t in &tools {
                    out.push_str(&format!("{} — {}\n  schema: {}\n", t.name, t.description, t.input_schema));
                }
                if tools.is_empty() {
                    out = "(this server exposes no tools)".into();
                }
                ToolOutcome::ok(sanitize(server, "tools/list", &out))
            }
            Err(e) => ToolOutcome::err(sanitize(server, "tools/list", &e)),
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &'static str {
        "mcp"
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "arguments": {"type": "object"}
            },
            "required": ["server"]
        })
    }

    /// Trusted server → plain ask per call; first use (or changed binary) →
    /// the protected first-use screen, never auto-allowable (SECURITY §M3a).
    fn permission(&self, input: &Value) -> Permission {
        let server = input.get("server").and_then(Value::as_str).unwrap_or("?");
        let tool = input.get("tool").and_then(Value::as_str).unwrap_or("tools/list");
        let summary = format!("mcp: {server}.{tool}");
        let Some(cfg) = self.server(server) else {
            // Unknown server: run() errors without side effects.
            return Permission::None;
        };
        let hash = self.hash_of(cfg);
        if self.trust.lock().expect("trust mutex").is_trusted(server, &hash) {
            Permission::Ask { summary }
        } else {
            Permission::AskProtected {
                summary,
                why: format!(
                    "first use of MCP server `{server}` (or its binary changed).\n\
                     binary: {}\n  {hash}\n\
                     Approving runs this program on your machine and lets its \
                     output into the model's context.",
                    cfg.command
                ),
            }
        }
    }

    fn run<'a>(&'a self, input: Value, _cancel: CancellationToken) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.run_impl(input))
    }
}
