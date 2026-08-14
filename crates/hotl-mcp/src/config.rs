//! `~/.config/hotl/mcp.toml` — the servers the owner has installed.
//!
//! ```toml
//! [[server]]
//! name = "docs"
//! command = "/usr/local/bin/docs-mcp"
//! args = ["--stdio"]
//! description = "project documentation search"
//! ```

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Ordered env overlay applied over the inherited environment at
    /// spawn; a later duplicate wins. Plugin servers arrive with the
    /// reserved `PLUGIN_ROOT`/`PLUGIN_DATA` pair already appended last,
    /// so the client-set values replace any configured same-name entry
    /// (Agent Plugins §9.1) without this crate knowing about plugins.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory for the server process; inherited when absent.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl ServerConfig {
    /// The spawn seam: program, args, env overlay (applied in order,
    /// last wins), and cwd — no stdio or spawn flags, so a test can read
    /// the result through `as_std().get_envs()` / `get_current_dir()`
    /// without a live subprocess.
    pub fn build_command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.args(&self.args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        cmd
    }
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
        // No `env`/`cwd`: the pre-0032 shape must keep parsing.
        let cfg: McpConfig = toml::from_str(
            "[[server]]\nname = \"docs\"\ncommand = \"/bin/docs\"\ndescription = \"d\"\n",
        )
        .unwrap();
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].name, "docs");
        assert!(cfg.servers[0].env.is_empty());
        assert_eq!(cfg.servers[0].cwd, None);
    }

    #[test]
    fn parses_env_and_cwd() {
        let cfg: McpConfig = toml::from_str(
            "[[server]]\nname = \"docs\"\ncommand = \"/bin/docs\"\n\
             env = [[\"CONFIG\", \"/etc/docs.json\"]]\ncwd = \"/srv/docs\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.servers[0].env,
            vec![("CONFIG".to_string(), "/etc/docs.json".to_string())]
        );
        assert_eq!(
            cfg.servers[0].cwd.as_deref(),
            Some(PathBuf::from("/srv/docs").as_path())
        );
    }

    /// §9.1's overlay order, held by `Command`'s own env map: pairs apply
    /// first-to-last and a later duplicate replaces the earlier one — so
    /// a reserved pair appended last always beats a configured same-name
    /// entry.
    #[test]
    fn build_command_applies_env_in_order_and_cwd() {
        let cfg = ServerConfig {
            name: "s".into(),
            command: "server-bin".into(),
            args: vec!["--stdio".into()],
            description: String::new(),
            env: vec![
                ("K".into(), "first".into()),
                ("PLUGIN_ROOT".into(), "configured-should-lose".into()),
                ("K".into(), "second".into()),
                ("PLUGIN_ROOT".into(), "/real/root".into()),
            ],
            cwd: Some(PathBuf::from("/srv/plugin")),
        };
        let cmd = cfg.build_command();
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "server-bin");
        let env: Vec<(String, String)> = std
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.unwrap_or_default().to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert!(env.contains(&("K".into(), "second".into())), "{env:?}");
        assert!(
            env.contains(&("PLUGIN_ROOT".into(), "/real/root".into())),
            "the last-appended value wins: {env:?}"
        );
        assert!(
            !env.iter()
                .any(|(_, v)| v == "first" || v == "configured-should-lose"),
            "{env:?}"
        );
        assert_eq!(
            std.get_current_dir(),
            Some(PathBuf::from("/srv/plugin").as_path())
        );
    }
}
