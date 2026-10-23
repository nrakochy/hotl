//! `~/.config/hotl/mcp.toml` — the servers the owner has installed.
//!
//! ```toml
//! [[server]]
//! name = "docs"
//! command = "/usr/local/bin/docs-mcp"
//! args = ["--stdio"]
//! description = "project documentation search"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    /// A local program to start and speak to over stdio. Exactly one of
    /// `command` and `url` (0063 T1); `validate` enforces it.
    #[serde(default)]
    pub command: Option<String>,
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
    /// A remote server reached over Streamable HTTP (0063). Absolute http(s);
    /// plain `http` only for loopback.
    #[serde(default)]
    pub url: Option<String>,
    /// Literal, non-secret headers sent on every request to a URL server. A
    /// literal `Authorization` is refused — use `bearer_token_env`.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// The *name* of an environment variable holding the bearer token. The
    /// value is read at connect time through `SecretStore` and never written
    /// to any file hotl owns.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
}

/// How a server is reached. Borrowed from a validated [`ServerConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport<'a> {
    Stdio {
        command: &'a str,
        args: &'a [String],
        env: &'a [(String, String)],
        cwd: Option<&'a Path>,
    },
    Http {
        url: &'a str,
        headers: &'a [(String, String)],
        bearer_token_env: Option<&'a str>,
    },
}

impl ServerConfig {
    /// A stdio server with nothing else set — the shape every test wants.
    pub fn stdio(name: &str, command: &str, args: &[&str]) -> Self {
        Self {
            name: name.into(),
            command: Some(command.into()),
            args: args.iter().map(|a| a.to_string()).collect(),
            description: String::new(),
            env: Vec::new(),
            cwd: None,
            url: None,
            headers: Vec::new(),
            bearer_token_env: None,
        }
    }

    /// A URL server with no headers and no token.
    pub fn http(name: &str, url: &str) -> Self {
        Self {
            name: name.into(),
            command: None,
            args: Vec::new(),
            description: String::new(),
            env: Vec::new(),
            cwd: None,
            url: Some(url.into()),
            headers: Vec::new(),
            bearer_token_env: None,
        }
    }

    /// The transport a validated config names. `url` wins when both are set
    /// (an invalid config `validate` already refused); neither reads as a
    /// stdio server with an empty program, which `validate` also refuses.
    pub fn transport(&self) -> Transport<'_> {
        match (&self.url, &self.command) {
            (Some(url), _) => Transport::Http {
                url,
                headers: &self.headers,
                bearer_token_env: self.bearer_token_env.as_deref(),
            },
            (None, command) => Transport::Stdio {
                command: command.as_deref().unwrap_or(""),
                args: &self.args,
                env: &self.env,
                cwd: self.cwd.as_deref(),
            },
        }
    }

    pub fn is_remote(&self) -> bool {
        self.url.is_some()
    }

    /// Every rule a `[[mcp]]` entry must pass before it loads (0063 T1). The
    /// error is the message the owner sees: what is wrong and the fix.
    pub fn validate(&self) -> Result<(), String> {
        let n = &self.name;
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "`{n}` sets both `command` and `url`; a server is a program or an address, not both"
                ))
            }
            (None, None) => {
                return Err(format!(
                    "`{n}` needs `command` (a program to start) or `url` (a Streamable HTTP endpoint)"
                ))
            }
            (Some(c), None) if c.trim().is_empty() => {
                return Err(format!("`{n}` has an empty `command`"))
            }
            _ => {}
        }
        let Some(url) = &self.url else {
            if self.bearer_token_env.is_some() || !self.headers.is_empty() {
                return Err(format!(
                    "`{n}` sets `headers`/`bearer_token_env`, which only apply to a `url` server"
                ));
            }
            return Ok(());
        };
        hotl_tools::plugins::validate_remote_url(url).map_err(|e| format!("`{n}`: {e}"))?;
        if !self.args.is_empty() || !self.env.is_empty() || self.cwd.is_some() {
            return Err(format!(
                "`{n}` is a `url` server: `args`, `env` and `cwd` only apply to a program"
            ));
        }
        for (name, _) in &self.headers {
            if name.eq_ignore_ascii_case("authorization") {
                return Err(format!(
                    "`{n}` sets a literal `Authorization` header; name the token's env var with `bearer_token_env` instead, so the secret never sits in config.toml"
                ));
            }
            if !hotl_tools::plugins::valid_header_name(name) {
                return Err(format!("`{n}`: `{name}` is not a valid HTTP header name"));
            }
        }
        if let Some(var) = &self.bearer_token_env {
            let ok = var
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(format!(
                    "`{n}`: `bearer_token_env = \"{var}\"` is not an environment variable name (letters, digits, `_`; not starting with a digit)"
                ));
            }
        }
        Ok(())
    }

    /// The spawn seam for a stdio server: program, args, env overlay
    /// (applied in order, last wins), and cwd — no stdio or spawn flags, so a
    /// test can read the result through `as_std().get_envs()` /
    /// `get_current_dir()` without a live subprocess. `None` for a URL server.
    pub fn build_command(&self) -> Option<tokio::process::Command> {
        let Transport::Stdio {
            command,
            args,
            env,
            cwd,
        } = self.transport()
        else {
            return None;
        };
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args);
        for (key, value) in env {
            cmd.env(key, value);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        Some(cmd)
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
        assert_eq!(cfg.servers[0].command.as_deref(), Some("/bin/docs"));
        assert!(cfg.servers[0].url.is_none());
        cfg.servers[0].validate().unwrap();
    }

    fn one(toml: &str) -> ServerConfig {
        toml::from_str::<McpConfig>(toml).unwrap().servers.remove(0)
    }

    #[test]
    fn parses_a_url_server() {
        let cfg = one(
            "[[server]]\nname = \"github\"\nurl = \"https://api.githubcopilot.com/mcp/\"\n\
             bearer_token_env = \"GITHUB_MCP_TOKEN\"\nheaders = [[\"X-Team\", \"core\"]]\n",
        );
        cfg.validate().unwrap();
        assert!(cfg.is_remote());
        assert!(cfg.build_command().is_none());
        assert_eq!(
            cfg.transport(),
            Transport::Http {
                url: "https://api.githubcopilot.com/mcp/",
                headers: &[("X-Team".to_string(), "core".to_string())],
                bearer_token_env: Some("GITHUB_MCP_TOKEN"),
            }
        );
    }

    #[test]
    fn command_and_url_together_is_an_error() {
        let err =
            one("[[server]]\nname = \"x\"\ncommand = \"/bin/x\"\nurl = \"https://x.com/mcp\"\n")
                .validate()
                .unwrap_err();
        assert!(err.contains("`x` sets both"), "{err}");
    }

    #[test]
    fn neither_command_nor_url_is_an_error() {
        let err = one("[[server]]\nname = \"x\"\n").validate().unwrap_err();
        assert!(
            err.contains("needs `command`") && err.contains("`url`"),
            "{err}"
        );
    }

    #[test]
    fn plain_http_is_loopback_only() {
        for ok in [
            "http://127.0.0.1:3845/mcp",
            "http://localhost:3845/mcp",
            "http://[::1]:3845/mcp",
            "https://example.com/mcp",
        ] {
            ServerConfig::http("s", ok)
                .validate()
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let err = ServerConfig::http("s", "http://example.com/mcp")
            .validate()
            .unwrap_err();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn a_literal_authorization_header_is_refused_and_names_bearer_token_env() {
        let cfg = ServerConfig {
            headers: vec![("Authorization".into(), "Bearer abc".into())],
            ..ServerConfig::http("s", "https://x.com/mcp")
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("bearer_token_env"), "{err}");
        let cfg = ServerConfig {
            headers: vec![("authorization".into(), "x".into())],
            ..ServerConfig::http("s", "https://x.com/mcp")
        };
        assert!(cfg.validate().is_err(), "case-insensitive");
    }

    #[test]
    fn args_env_cwd_on_a_url_server_are_errors() {
        let base = ServerConfig::http("s", "https://x.com/mcp");
        for cfg in [
            ServerConfig {
                args: vec!["--x".into()],
                ..base.clone()
            },
            ServerConfig {
                env: vec![("K".into(), "v".into())],
                ..base.clone()
            },
            ServerConfig {
                cwd: Some(PathBuf::from("/tmp")),
                ..base.clone()
            },
        ] {
            let err = cfg.validate().unwrap_err();
            assert!(err.contains("only apply to a program"), "{err}");
        }
        // And the reverse: URL-only keys on a program.
        let cfg = ServerConfig {
            bearer_token_env: Some("T".into()),
            ..ServerConfig::stdio("s", "/bin/x", &[])
        };
        assert!(cfg
            .validate()
            .unwrap_err()
            .contains("only apply to a `url` server"));
        // A bad env var name.
        let cfg = ServerConfig {
            bearer_token_env: Some("9BAD-NAME".into()),
            ..base
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("not an environment variable name"), "{err}");
    }

    #[test]
    fn a_metadata_address_url_is_refused() {
        for bad in [
            "http://169.254.169.254/",
            "https://169.254.169.254/mcp",
            "https://10.0.0.5/mcp",
            "https://[fd00:ec2::254]/mcp",
        ] {
            let err = ServerConfig::http("s", bad).validate().unwrap_err();
            assert!(err.contains("private or metadata address"), "{bad}: {err}");
        }
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
            env: vec![
                ("K".into(), "first".into()),
                ("PLUGIN_ROOT".into(), "configured-should-lose".into()),
                ("K".into(), "second".into()),
                ("PLUGIN_ROOT".into(), "/real/root".into()),
            ],
            cwd: Some(PathBuf::from("/srv/plugin")),
            ..ServerConfig::stdio("s", "server-bin", &["--stdio"])
        };
        let cmd = cfg.build_command().expect("a stdio server");
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
