//! `[[retrieval]]` in config.toml — the backends the owner has configured.
//! The binary re-serializes the section as `[[backend]]` (the `[[mcp]]` →
//! `[[server]]` precedent). Bad entries warn and skip — fail-closed, a bad
//! entry never loads a backend.
//!
//! ```toml
//! [[retrieval]]
//! name = "notes"
//! kind = "mcp"                       # the only kind in P1; "lexical" is P2
//! command = "/usr/local/bin/notes-rag"
//! args = ["--stdio"]
//! tool = "search"                    # the MCP tool to call; default "search"
//! description = "personal notes semantic search"
//! ```

use std::path::Path;

use serde::Deserialize;

use crate::mcp::McpRetriever;
use crate::Retriever;

#[derive(Debug, Clone, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    pub kind: String,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub tool: Option<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RetrievalConfig {
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendConfig>,
}

/// Build the configured backends. Returns the survivors and one warning per
/// skipped entry (each warning is a fix instruction).
pub fn build(
    backends: Vec<BackendConfig>,
    config_dir: &Path,
) -> (Vec<Box<dyn Retriever>>, Vec<String>) {
    let mut out: Vec<Box<dyn Retriever>> = Vec::new();
    let mut warnings = Vec::new();
    for b in backends {
        match b.kind.as_str() {
            "mcp" => {
                let Some(command) = b.command else {
                    warnings.push(format!(
                        "[[retrieval]] `{}` skipped: kind \"mcp\" requires `command`",
                        b.name
                    ));
                    continue;
                };
                let trust = hotl_mcp::trust::TrustStore::load(config_dir);
                out.push(Box::new(McpRetriever::new(
                    hotl_mcp::config::ServerConfig {
                        name: b.name,
                        command,
                        args: b.args,
                        description: b.description,
                    },
                    b.tool.unwrap_or_else(|| "search".into()),
                    trust,
                )));
            }
            other => warnings.push(format!(
                "[[retrieval]] `{}` skipped: unknown kind \"{other}\" (P1 supports \"mcp\")",
                b.name
            )),
        }
    }
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_sections() {
        let cfg: RetrievalConfig = toml::from_str(
            "[[backend]]\nname = \"docs\"\nkind = \"mcp\"\ncommand = \"/bin/docs-rag\"\n\
             tool = \"search\"\ndescription = \"doc search\"\n",
        )
        .unwrap();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].name, "docs");
        assert_eq!(cfg.backends[0].kind, "mcp");
    }

    #[test]
    fn build_skips_bad_entries_fail_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            BackendConfig {
                name: "good".into(),
                kind: "mcp".into(),
                command: Some("/bin/true".into()),
                args: vec![],
                tool: None,
                description: "ok".into(),
            },
            BackendConfig {
                name: "no-command".into(),
                kind: "mcp".into(),
                command: None,
                args: vec![],
                tool: None,
                description: String::new(),
            },
            BackendConfig {
                name: "future".into(),
                kind: "lexical".into(),
                command: None,
                args: vec![],
                tool: None,
                description: String::new(),
            },
        ];
        let (backends, warnings) = build(entries, dir.path());
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].name(), "good");
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("no-command") && warnings[0].contains("command"));
        assert!(warnings[1].contains("future") && warnings[1].contains("kind"));
    }
}
