//! `~/.config/hotl/mcp.toml` — the servers the owner has installed.
//!
//! ```toml
//! [[server]]
//! name = "docs"
//! command = "/usr/local/bin/docs-mcp"
//! args = ["--stdio"]
//! description = "project documentation search"
//! ```

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_mcp_section() {
        // The binary feeds config.toml's `[[mcp]]` in as `[[server]]`.
        let cfg: McpConfig = toml::from_str(
            "[[server]]\nname = \"docs\"\ncommand = \"/bin/docs\"\ndescription = \"d\"\n",
        )
        .unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "docs");
    }
}
