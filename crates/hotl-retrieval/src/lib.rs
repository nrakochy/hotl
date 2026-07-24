//! The retrieval seam (design: specs/design-docs/2026-07-23-rag-injection-design.md).
//!
//! One trait ([`Retriever`]), one model-facing tool (`recall`), pluggable
//! backends. hotl ships no vector DB — backends are owner-configured (MCP
//! servers in P1; a local lexical index arrives in P2). Everything a backend
//! returns passes one sanitizer chokepoint (untrusted envelope with
//! `recall:<backend>` provenance) before entering the transcript.

pub mod config;
pub mod mcp;
mod sanitize;
pub mod testing;
mod tool;

pub use sanitize::MAX_RESULT_BYTES;
pub use tool::RecallTool;

use futures_util::future::BoxFuture;
use hotl_tools::Permission;
use tokio_util::sync::CancellationToken;

/// Forge's paired-query contract: `text` is what to find, `purpose` is why —
/// backends that rerank use the purpose; lexical backends ignore it.
#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub purpose: Option<String>,
    pub k: usize,
}

/// Default hit count when the model doesn't ask for one.
pub const DEFAULT_K: usize = 8;

/// Where a hit came from. Provenance is mandatory: every excerpt the model
/// sees names its source, so claims can be verified with `read`.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceRef {
    File { path: String, line: Option<u64> },
    Server { name: String },
}

impl std::fmt::Display for SourceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceRef::File {
                path,
                line: Some(line),
            } => write!(f, "{path}:{line}"),
            SourceRef::File { path, line: None } => write!(f, "{path}"),
            SourceRef::Server { name } => write!(f, "server:{name}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub source: SourceRef,
    pub excerpt: String,
    /// Backend-relative relevance, if the backend scores at all.
    pub score: Option<f32>,
    /// When the backend's index last saw this source (unix seconds).
    /// `None` = live or unknown. Surfaced in results so staleness is visible
    /// to the model — a stale hit is a prompt to `read` the live file.
    pub indexed_at_unix: Option<u64>,
}

/// A pluggable retrieval backend. P1 ships the MCP adapter ([`mcp`]); the
/// local lexical index is P2. The trait is deliberately minimal — `sync()`
/// (index maintenance) arrives with the first backend that has an index.
pub trait Retriever: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// What running a search requires from the human. Read-only in-process
    /// backends keep the default (`None`); backends that execute a configured
    /// program (MCP) return the same ask/protected posture as the `mcp` tool.
    fn permission(&self, query: &str) -> Permission {
        let _ = query;
        Permission::None
    }
    fn search<'a>(
        &'a self,
        query: &'a Query,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::StaticRetriever;
    use hotl_tools::Permission;

    #[test]
    fn source_ref_displays_for_result_headers() {
        let f = SourceRef::File {
            path: "notes/rust.md".into(),
            line: Some(12),
        };
        assert_eq!(f.to_string(), "notes/rust.md:12");
        let f = SourceRef::File {
            path: "notes/rust.md".into(),
            line: None,
        };
        assert_eq!(f.to_string(), "notes/rust.md");
        let s = SourceRef::Server {
            name: "docs".into(),
        };
        assert_eq!(s.to_string(), "server:docs");
    }

    #[tokio::test]
    async fn static_retriever_serves_its_hits_and_default_permission_is_none() {
        let r = StaticRetriever {
            name: "notes".into(),
            description: "test notes".into(),
            hits: vec![Hit {
                source: SourceRef::File {
                    path: "a.md".into(),
                    line: None,
                },
                excerpt: "alpha".into(),
                score: Some(0.5),
                indexed_at_unix: None,
            }],
            error: None,
        };
        assert_eq!(r.permission("anything"), Permission::None);
        let q = Query {
            text: "q".into(),
            purpose: None,
            k: DEFAULT_K,
        };
        let hits = r
            .search(&q, tokio_util::sync::CancellationToken::new())
            .await
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].excerpt, "alpha");
    }
}
