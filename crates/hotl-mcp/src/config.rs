//! `~/.config/hotl/mcp.toml` — the servers the owner has installed.
//!
//! ```toml
//! [[server]]
//! name = "docs"
//! command = "/usr/local/bin/docs-mcp"
//! args = ["--stdio"]
//! description = "project documentation search"
//! ```

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "server")]
    pub servers: Vec<ServerConfig>,
}

/// Load the config; a malformed file returns a warning and no servers
/// (fail-closed: a typo can't silently drop a server *or* invent one).
pub fn load(config_dir: &Path) -> (McpConfig, Option<String>) {
    let path = config_dir.join("mcp.toml");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return (McpConfig::default(), None);
    };
    match toml::from_str(&raw) {
        Ok(cfg) => (cfg, None),
        Err(e) => (
            McpConfig::default(),
            Some(format!("mcp.toml ignored (parse error): {e}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.toml"),
            "[[server]]\nname = \"docs\"\ncommand = \"/bin/docs\"\ndescription = \"d\"\n",
        )
        .unwrap();
        let (cfg, warning) = load(dir.path());
        assert!(warning.is_none());
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "docs");

        std::fs::write(dir.path().join("mcp.toml"), "not [ toml").unwrap();
        let (cfg, warning) = load(dir.path());
        assert!(cfg.servers.is_empty(), "malformed config must not half-load");
        assert!(warning.is_some());
    }
}
