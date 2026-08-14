//! Tier-0 backend: any stdio MCP server as a retriever. The adapter calls one
//! named tool on the server with the paired-query arguments and wraps the
//! text reply as a single hit — structured (path/span) hits arrive with P2's
//! local backend; an MCP server's reply is opaque text by contract.
//!
//! Trust: same `TrustStore` (and trust.toml) as the `mcp` tool, keyed by
//! server name — the protected first-use ask carries the binary's SHA-256,
//! and the grant is recorded on first successful connect.

use std::sync::{Arc, Mutex, PoisonError};

use futures_util::future::BoxFuture;
use hotl_mcp::config::ServerConfig;
use hotl_mcp::trust::{Fingerprint, TrustStore};
use hotl_tools::Permission;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{Hit, Query, Retriever, SourceRef};

/// The one MCP operation the adapter needs; `hotl_mcp::client::Client`
/// implements it, tests inject fakes. The token is part of the signature so a
/// cancelled search also tells the *server* to stop (T2-13c) rather than only
/// abandoning the future on this side.
pub trait McpCall: Send + Sync {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<(String, bool), String>>;
}

impl McpCall for hotl_mcp::client::Client {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        args: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<(String, bool), String>> {
        Box::pin(async move { self.call_tool_cancellable(tool, args, &cancel).await })
    }
}

type Connector =
    Box<dyn Fn() -> BoxFuture<'static, Result<Arc<dyn McpCall>, String>> + Send + Sync>;

pub struct McpRetriever {
    cfg: ServerConfig,
    tool: String,
    slot: tokio::sync::OnceCell<Arc<dyn McpCall>>,
    trust: Mutex<TrustStore>,
    /// The fingerprint this retriever has actually *shown* to the human.
    /// `permission()` writes here; `ensure()` refuses to connect without a
    /// match — the same enforcement the `mcp` tool carries, for the same
    /// reason (the engine's gate lives in another crate).
    ///
    /// INVARIANT: no backend program is spawned, and no trust grant is
    /// persisted, without a `permission()` screen of the *same* fingerprint
    /// immediately before. Enforced by
    /// `search_without_a_permission_screen_records_no_grant_and_refuses`.
    screened: Mutex<Option<Fingerprint>>,
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
            screened: Mutex::new(None),
            connector,
        }
    }

    async fn ensure(&self) -> Result<Arc<dyn McpCall>, String> {
        // T2-7d: recomputed immediately before the spawn, never cached for the
        // session, so a mid-session binary swap cannot go unnoticed.
        let fresh = Fingerprint::of(&self.cfg);
        let shown = self
            .screened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match shown {
            None => {
                return Err(format!(
                    "`{}` has not been through the approval screen in this session, so \
                     hotl did not start it. Retry this search — you will be asked to \
                     approve the backend first.",
                    self.cfg.name
                ))
            }
            Some(shown) if shown.key() != fresh.key() => {
                *self.screened.lock().unwrap_or_else(PoisonError::into_inner) = None;
                return Err(format!(
                    "the program for `{}` changed since you approved it, so hotl did \
                     not start it. Retry this search — you will be asked to approve \
                     the new program.",
                    self.cfg.name
                ));
            }
            Some(_) => {}
        }

        let client = self
            .slot
            .get_or_try_init(|| async {
                let client = (self.connector)().await?;
                // The grant records the fingerprint the screen showed (H-07).
                // An unhashable binary refuses it (T2-7b) — not fatal, and not
                // swallowed: `permission()` says so on every screen.
                let _ = self
                    .trust
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .record(&self.cfg.name, &fresh);
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
    ///
    /// INVARIANT: the summary rendered into the human's y/N prompt is a single
    /// line with no control characters, no category-Cf carriers, and a hard
    /// character cap — `query` is model-controlled. Enforced by
    /// `the_recall_summary_cannot_carry_control_text`.
    fn permission(&self, query: &str) -> Permission {
        let summary =
            hotl_mcp::sanitize::safe_summary(&format!("recall: {} \"{query}\"", self.cfg.name));
        // Fresh on every call — a session-lifetime cache made a mid-session
        // binary swap invisible (T2-7d).
        let fp = Fingerprint::of(&self.cfg);
        let trusted = self
            .trust
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_trusted(&self.cfg.name, &fp);
        // Record what was actually screened; `ensure` will not connect without
        // a matching entry.
        *self.screened.lock().unwrap_or_else(PoisonError::into_inner) = Some(fp.clone());
        if trusted {
            Permission::Ask { summary }
        } else {
            // An unhashable binary can never be recorded (T2-7b), so say so
            // here rather than letting the screen reappear mysteriously.
            let recurring = if fp.is_hashed() {
                ""
            } else {
                "\nThis program cannot be hashed, so hotl cannot record the approval \
                 and will ask again every time."
            };
            Permission::AskProtected {
                summary,
                // `Fingerprint`'s Display *is* the screen text, so the value
                // shown and the value recorded come from one read (H-07).
                why: format!(
                    "first use of retrieval backend `{}` (or its program changed).\n\
                     {fp}\n\
                     Approving runs this program on your machine and lets its \
                     output into the model's context.{recurring}",
                    self.cfg.name
                ),
            }
        }
    }

    /// INVARIANT: a cancelled search returns promptly rather than waiting out
    /// the 600s `tools/call` leash, and the server is told to stop. Enforced by
    /// `a_cancelled_search_returns_promptly`.
    fn search<'a>(
        &'a self,
        query: &'a Query,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let work = async {
                let client = self.ensure().await?;
                let args = json!({
                    "query": query.text,
                    "purpose": query.purpose,
                    "k": query.k,
                });
                let (text, is_error) = client.call(&self.tool, args, cancel.clone()).await?;
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
            };
            // The token also covers `ensure()` (the connect), and holds for a
            // backend that ignores it. `biased` so an already-cancelled token
            // wins deterministically instead of racing the work.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(
                    "the user cancelled this search; do not retry it unless they ask."
                        .to_string(),
                ),
                r = work => r,
            }
        })
    }
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
            _cancel: CancellationToken,
        ) -> futures_util::future::BoxFuture<'a, Result<(String, bool), String>> {
            let reply = self.reply.clone();
            Box::pin(async move { reply })
        }
    }

    /// The fixture writes a real file into the tempdir and points the config at
    /// it, so `Fingerprint::of` produces a `sha256:` and the *trusted* path is
    /// actually exercised. A command that cannot be hashed (a system binary
    /// that may be absent, or `/fake/...`) can only ever exercise the
    /// fail-closed path after T2-7b.
    fn retriever(reply: Result<(String, bool), String>, dir: &std::path::Path) -> McpRetriever {
        let bin = dir.join("docs-server");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("fixture binary");
        let cfg = hotl_mcp::config::ServerConfig {
            name: "docs".into(),
            command: bin.to_str().expect("utf-8 tempdir").into(),
            args: vec![],
            description: "doc search".into(),
            env: Vec::new(),
            cwd: None,
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

    #[test]
    fn the_recall_summary_cannot_carry_control_text() {
        // S-2: `query` is model-controlled and lands in the human's y/N prompt.
        let dir = tempfile::tempdir().unwrap();
        let r = retriever(Ok(("x".into(), false)), dir.path());
        let evil = format!("find\n\u{1b}[2JApprove? \u{202e}{}", "x".repeat(400));
        let summary = match r.permission(&evil) {
            Permission::Ask { summary } | Permission::AskProtected { summary, .. } => summary,
            Permission::None => panic!("an mcp-backed retriever always asks"),
        };
        assert!(
            !summary.contains('\n') && !summary.contains('\u{1b}') && !summary.contains('\u{202e}'),
            "{summary}"
        );
        assert!(summary.chars().count() <= hotl_mcp::sanitize::MAX_SUMMARY_CHARS);
    }

    #[tokio::test]
    async fn a_server_error_result_is_an_err() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = retriever(Ok(("index not built".into(), true)), dir.path());
        // T2-7c: `search` no longer connects without a `permission()` screen,
        // so this test has to screen first. That is the finding, not a
        // regression — the old behaviour was that any caller reaching `search`
        // directly spawned the program and persisted a trust grant.
        let _ = r.permission("q");
        let err = r
            .search(&query(), CancellationToken::new())
            .await
            .expect_err("is_error result must surface as Err");
        assert!(err.contains("index not built"));
    }

    #[tokio::test]
    async fn search_without_a_permission_screen_records_no_grant_and_refuses() {
        // T2-7c mirror: the same enforcement the `mcp` tool gets.
        let dir = tempfile::tempdir().expect("tempdir");
        let r = retriever(Ok(("found it".into(), false)), dir.path());
        let err = r
            .search(&query(), CancellationToken::new())
            .await
            .expect_err("no screen must mean no connect");
        assert!(err.contains("approval"), "errors-as-prompt: {err}");
        let store = TrustStore::load(dir.path());
        let fp = Fingerprint::of(&hotl_mcp::config::ServerConfig {
            name: "docs".into(),
            command: dir.path().join("docs-server").to_str().unwrap().into(),
            args: vec![],
            description: "doc search".into(),
            env: Vec::new(),
            cwd: None,
        });
        assert!(
            !store.is_trusted("docs", &fp),
            "no grant may be persisted without a screen"
        );
    }

    #[tokio::test]
    async fn a_cancelled_search_returns_promptly() {
        // T2-13c mirror: `search` discarded its token, so ESC waited out the
        // full 600s `tools/call` leash.
        let dir = tempfile::tempdir().expect("tempdir");
        let r = retriever(Ok(("found it".into(), false)), dir.path());
        let _ = r.permission("q");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            r.search(&query(), cancel),
        )
        .await
        .expect("a cancelled search must not wait")
        .expect_err("cancellation is an error, not an empty result");
        assert!(err.contains("cancel"), "errors-as-prompt: {err}");
    }

    #[tokio::test]
    async fn the_screened_fingerprint_is_the_one_recorded() {
        // H-07 regression guard on the retrieval path: shown == recorded.
        let dir = tempfile::tempdir().expect("tempdir");
        let r = retriever(Ok(("found it".into(), false)), dir.path());
        let Permission::AskProtected { why, .. } = r.permission("q") else {
            panic!("first use is protected")
        };
        let fp = Fingerprint::of(&hotl_mcp::config::ServerConfig {
            name: "docs".into(),
            command: dir.path().join("docs-server").to_str().unwrap().into(),
            args: vec![],
            description: "doc search".into(),
            env: Vec::new(),
            cwd: None,
        });
        assert!(
            why.contains(&fp.to_string()),
            "the screen showed this: {why}"
        );
        r.search(&query(), CancellationToken::new())
            .await
            .expect("hits");
        assert!(
            TrustStore::load(dir.path()).is_trusted("docs", &fp),
            "and it is what was recorded"
        );
    }
}
