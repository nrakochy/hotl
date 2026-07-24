//! Test backends. Public (not `#[cfg(test)]`) so other crates' integration
//! tests can drive the seam — the `ScriptedProvider` precedent.

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::{Hit, Query, Retriever};

/// Serves a fixed hit list (or a fixed error). No execution, no egress —
/// permission stays the default `None`.
pub struct StaticRetriever {
    pub name: String,
    pub description: String,
    pub hits: Vec<Hit>,
    pub error: Option<String>,
}

impl Retriever for StaticRetriever {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn search<'a>(
        &'a self,
        _query: &'a Query,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            match &self.error {
                Some(e) => Err(e.clone()),
                None => Ok(self.hits.clone()),
            }
        })
    }
}
