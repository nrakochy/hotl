//! Tier-0 backend: any stdio MCP server as a retriever. The adapter calls one
//! named tool on the server with the paired-query arguments and wraps the
//! text reply as a single hit — structured (path/span) hits arrive with P2's
//! local backend; an MCP server's reply is opaque text by contract.
//!
//! Trust: same `TrustStore` (and trust.toml) as the `mcp` tool, keyed by
//! server name — the protected first-use ask carries the binary's SHA-256,
//! and the grant is recorded on first successful connect.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use futures_util::future::BoxFuture;
use hotl_mcp::config::ServerConfig;
use hotl_mcp::trust::{binary_hash, TrustStore};
use hotl_tools::Permission;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{Hit, Query, Retriever, SourceRef};

/// The one MCP operation the adapter needs; `hotl_mcp::client::Client`
/// implements it, tests inject fakes.
pub trait McpCall: Send + Sync {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<(String, bool), String>>;
}

impl McpCall for hotl_mcp::client::Client {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<(String, bool), String>> {
        Box::pin(self.call_tool(tool, args))
    }
}

type Connector =
    Box<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn McpCall>, String>> + Send + Sync>;

pub struct McpRetriever {
    cfg: ServerConfig,
    tool: String,
    slot: tokio::sync::OnceCell<Arc<dyn McpCall>>,
    trust: Mutex<TrustStore>,
    hash: OnceLock<String>,
    connector: Connector,
}

impl McpRetriever {
    pub fn new(cfg: ServerConfig, tool: String, trust: TrustStore) -> Self {
        let command = cfg.command.clone();
        let args = cfg.args.clone();
        Self::with_connector(
            cfg,
            tool,
            trust,
            Box::new(move || {
                let command = command.clone();
                let args = args.clone();
                Box::pin(async move {
                    let client = hotl_mcp::client::Client::connect(&command, &args)?;
                    client.initialize().await?;
                    let client: Arc<dyn McpCall> = client;
                    Ok(client)
                })
            }),
        )
    }

    /// Tests inject an in-process transport here.
    pub fn with_connector(
        cfg: ServerConfig,
        tool: String,
        trust: TrustStore,
        connector: Connector,
    ) -> Self {
        Self {
            cfg,
            tool,
            slot: tokio::sync::OnceCell::new(),
            trust: Mutex::new(trust),
            hash: OnceLock::new(),
            connector,
        }
    }

    fn hash(&self) -> &str {
        self.hash.get_or_init(|| binary_hash(&self.cfg.command))
    }

    async fn ensure(&self) -> Result<Arc<dyn McpCall>, String> {
        let client = self
            .slot
            .get_or_try_init(|| async {
                let client = (self.connector)().await?;
                // Reaching search() means the (protected) ask was approved
                // upstream — record the grant, keyed to the same hash the
                // screen showed (the McpTool H-07 discipline).
                let hash = self.hash().to_string();
                self.trust
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .record(&self.cfg.name, &hash);
                Ok::<_, String>(client)
            })
            .await?;
        Ok(client.clone())
    }
}

impl Retriever for McpRetriever {
    fn name(&self) -> &str {
        &self.cfg.name
    }
    fn description(&self) -> &str {
        &self.cfg.description
    }

    /// Trusted server → plain ask per call; first use (or changed binary) →
    /// the protected screen, never auto-allowable (docs/SECURITY.md §Retrieval).
    fn permission(&self, query: &str) -> Permission {
        let summary = format!("recall: {} \"{}\"", self.cfg.name, truncate(query, 60));
        let hash = self.hash().to_string();
        if self
            .trust
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_trusted(&self.cfg.name, &hash)
        {
            Permission::Ask { summary }
        } else {
            Permission::AskProtected {
                summary,
                why: format!(
                    "first use of retrieval backend `{}` (or its binary changed).\n\
                     binary: {}\n  {hash}\n\
                     Approving runs this program on your machine and lets its \
                     output into the model's context.",
                    self.cfg.name, self.cfg.command
                ),
            }
        }
    }

    fn search<'a>(
        &'a self,
        query: &'a Query,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let client = self.ensure().await?;
            let args = json!({
                "query": query.text,
                "purpose": query.purpose,
                "k": query.k,
            });
            let (text, is_error) = client.call(&self.tool, args).await?;
            if is_error {
                return Err(text);
            }
            Ok(vec![Hit {
                source: SourceRef::Server {
                    name: self.cfg.name.clone(),
                },
                excerpt: text,
                score: None,
                indexed_at_unix: None,
            }])
        })
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_tools::Permission;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct FakeCall {
        reply: Result<(String, bool), String>,
    }
    impl McpCall for FakeCall {
        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _args: serde_json::Value,
        ) -> futures_util::future::BoxFuture<'a, Result<(String, bool), String>> {
            let reply = self.reply.clone();
            Box::pin(async move { reply })
        }
    }

    fn retriever(reply: Result<(String, bool), String>, dir: &std::path::Path) -> McpRetriever {
        let cfg = hotl_mcp::config::ServerConfig {
            name: "docs".into(),
            command: "/bin/true".into(),
            args: vec![],
            description: "doc search".into(),
        };
        McpRetriever::with_connector(
            cfg,
            "search".into(),
            hotl_mcp::trust::TrustStore::load(dir),
            Box::new(move || {
                let reply = reply.clone();
                Box::pin(async move {
                    let client: Arc<dyn McpCall> = Arc::new(FakeCall { reply });
                    Ok(client)
                })
            }),
        )
    }

    fn query() -> Query {
        Query {
            text: "how do we deploy".into(),
            purpose: Some("release checklist".into()),
            k: 8,
        }
    }

    #[tokio::test]
    async fn first_use_is_protected_then_a_plain_ask_after_a_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = retriever(Ok(("found it".into(), false)), dir.path());
        assert!(
            matches!(r.permission("q"), Permission::AskProtected { .. }),
            "unknown binary → protected first-use screen"
        );
        let hits = r
            .search(&query(), CancellationToken::new())
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1, "one hit wrapping the server text");
        assert_eq!(
            hits[0].source,
            SourceRef::Server {
                name: "docs".into()
            }
        );
        assert_eq!(hits[0].excerpt, "found it");
        assert!(
            matches!(r.permission("q"), Permission::Ask { .. }),
            "the connect recorded the grant — later calls are a plain ask"
        );
    }

    #[tokio::test]
    async fn a_server_error_result_is_an_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = retriever(Ok(("index not built".into(), true)), dir.path());
        let err = r
            .search(&query(), CancellationToken::new())
            .await
            .expect_err("is_error result must surface as Err");
        assert!(err.contains("index not built"));
    }
}
