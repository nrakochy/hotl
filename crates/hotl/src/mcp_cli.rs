//! `hotl mcp` — inspect configured MCP servers and their trust grants.
//!
//! INVARIANT: no subcommand here writes config.toml. `add` prints the block
//! for the human to paste. A CLI that edited config would be a path
//! `bash -c 'hotl mcp add …'` could take, and that is not covered by the
//! permission layer — `bash_protected_write_reason` reads redirects, `tee`,
//! and `dd`, not a program that writes config as a side effect. Only the
//! kernel sandbox stops it, and that is incidental rather than designed.
//! Enforced by `no_verb_ever_writes_config_toml`.
//!
//! INVARIANT: there is no verb that *grants* trust. Trust is recorded only by
//! a human answering the in-session fingerprint screen; a CLI grant would be
//! the "always allow" that `rules.rs` deliberately omits. `untrust` is the one
//! mutation, because revocation only ever reduces privilege. `test` spawns a
//! server after showing the same screen text, and records nothing. Enforced by
//! `test_records_no_trust`.

use std::io::IsTerminal;
use std::path::Path;

use hotl_mcp::config::ServerConfig;
use hotl_mcp::trust::{Fingerprint, TrustState, TrustStore};

pub fn mcp_main(args: &[String]) -> i32 {
    let config_dir = crate::agent::config_dir();
    let data_dir = crate::agent::data_dir();
    match args.get(1).map(String::as_str) {
        None | Some("list") => {
            print!("{}", render_list(&config_dir, &data_dir));
            0
        }
        Some("show") => match args.get(2) {
            Some(name) => report(show(&config_dir, &data_dir, name)),
            None => usage(),
        },
        Some("add") => match (args.get(2), args.get(3).map(String::as_str)) {
            // `add <name> --url <url> [--bearer-env VAR] [--header N=V]…`
            (Some(name), Some("--url")) => match args.get(4) {
                Some(url) => report(add_url_args(name, url, &args[5..])),
                None => usage(),
            },
            (Some(name), Some(command)) => report(add(name, command, &args[4..])),
            _ => usage(),
        },
        Some("untrust") => match args.get(2) {
            Some(name) => report(untrust(&config_dir, name)),
            None => usage(),
        },
        Some("test") => match args.get(2) {
            Some(name) => report(test(&config_dir, &data_dir, name)),
            None => usage(),
        },
        _ => usage(),
    }
}

fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("hotl mcp: {e}");
            1
        }
    }
}

fn usage() -> i32 {
    eprintln!(
        "usage: hotl mcp [list] | show <name> | add <name> <command> [args...] \
         | add <name> --url <url> [--bearer-env VAR] [--header NAME=VALUE]... \
         | untrust <name>|--all | test <name>"
    );
    2
}

/// `token: $VAR (set)` / `(unset)` for a URL server with a token, else empty.
fn token_state(cfg: &ServerConfig) -> String {
    use hotl_platform::SecretStore as _;
    match &cfg.bearer_token_env {
        Some(var) => {
            let set = hotl_platform::EnvSecrets
                .get(var)
                .is_some_and(|v| !v.trim().is_empty());
            format!("  token: ${var} ({})", if set { "set" } else { "unset" })
        }
        None => String::new(),
    }
}

/// `command arg arg` as one display string; a URL server shows its address.
fn program(cfg: &ServerConfig) -> String {
    match cfg.transport() {
        hotl_mcp::config::Transport::Http { url, .. } => url.to_string(),
        hotl_mcp::config::Transport::Stdio { command, args, .. } => {
            if args.is_empty() {
                command.to_string()
            } else {
                format!("{command} {}", args.join(" "))
            }
        }
    }
}

/// The one roster composition every verb reads — identical to
/// `build_registry`'s (`[[mcp]]` plus plugin servers as
/// `<plugin>:<server>`), so this CLI can never disagree with what a turn
/// loads. Plugin load warnings are dropped here; they surface at startup
/// and in `hotl plugins list`.
fn all_servers(config_dir: &Path, data_dir: &Path) -> Vec<ServerConfig> {
    all_servers_reporting(config_dir, data_dir).0
}

/// `all_servers` plus the `[[mcp]]` validation warnings (0063 T1), which
/// `list` prints — a roster that silently lost its servers is the thing an
/// owner most needs told.
fn all_servers_reporting(config_dir: &Path, data_dir: &Path) -> (Vec<ServerConfig>, Vec<String>) {
    let cfg = crate::config::Config::load(config_dir);
    let (plugins, _) = cfg.plugins.load(config_dir, data_dir);
    crate::config::all_mcp_servers_reporting(&cfg, &plugins)
}

/// Servers, their trust state, then the warnings `TrustStore::load` swallows,
/// then grants whose server has left the config.
fn render_list(config_dir: &Path, data_dir: &Path) -> String {
    let (servers, mut warnings) = all_servers_reporting(config_dir, data_dir);
    let (store, trust_warnings) = TrustStore::load_reporting(config_dir);
    warnings.extend(trust_warnings);
    let workspace = hotl_tools::workspace_root();
    let mut out = String::new();
    if servers.is_empty() {
        out.push_str("no MCP servers configured — run: hotl mcp add <name> <command>\n");
    }
    let name_w = servers
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    let prog_w = servers
        .iter()
        .map(|s| program(s).chars().count())
        .max()
        .unwrap_or(0);
    for cfg in &servers {
        let state = store.state(&cfg.name, &Fingerprint::of(cfg), workspace);
        out.push_str(&format!(
            "{:<name_w$}  {:<prog_w$}  {}{}\n",
            cfg.name,
            program(cfg),
            state.label(),
            token_state(cfg)
        ));
    }
    for w in &warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    for name in stale_grants(&store, &servers) {
        out.push_str(&format!(
            "warning: trust.toml holds a grant for `{name}`, which is no longer \
             configured — run: hotl mcp untrust {name}\n"
        ));
    }
    out
}

/// Grant names with no matching `[[mcp]]` entry.
fn stale_grants(store: &TrustStore, servers: &[ServerConfig]) -> Vec<String> {
    let mut stale: Vec<String> = store
        .entries()
        .map(|(n, _)| n.to_string())
        .filter(|n| !servers.iter().any(|s| &s.name == n))
        .collect();
    stale.sort();
    stale
}

fn find<'a>(servers: &'a [ServerConfig], name: &str) -> Result<&'a ServerConfig, String> {
    servers.iter().find(|s| s.name == name).ok_or_else(|| {
        let known: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        if known.is_empty() {
            format!("no server named `{name}` is configured; none are — see: hotl mcp add")
        } else {
            format!(
                "no server named `{name}` is configured. Configured: {} — see: hotl mcp list",
                known.join(", ")
            )
        }
    })
}

/// The exact text the in-session screen shows, plus what the gate will do.
/// Reads the binary; never starts it.
fn show(config_dir: &Path, data_dir: &Path, name: &str) -> Result<String, String> {
    let servers = all_servers(config_dir, data_dir);
    let cfg = find(&servers, name)?;
    let fp = Fingerprint::of(cfg);
    let state = TrustStore::load(config_dir).state(name, &fp, hotl_tools::workspace_root());
    Ok(format!(
        "{fp}\n  key: {}\n  trust: {}{}",
        fp.key(),
        state.label(),
        token_state(cfg)
    ))
}

/// `--bearer-env VAR` and `--header NAME=VALUE` after `add <name> --url <url>`.
fn add_url_args(name: &str, url: &str, rest: &[String]) -> Result<String, String> {
    let mut bearer_env = None;
    let mut headers = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match (rest[i].as_str(), rest.get(i + 1)) {
            ("--bearer-env", Some(var)) => {
                bearer_env = Some(var.clone());
                i += 2;
            }
            ("--header", Some(pair)) => {
                let Some((k, v)) = pair.split_once('=') else {
                    return Err(format!("`--header {pair}`: expected NAME=VALUE"));
                };
                headers.push((k.trim().to_string(), v.to_string()));
                i += 2;
            }
            (flag, _) => return Err(format!("unknown flag `{flag}` after --url")),
        }
    }
    add_url(name, url, bearer_env.as_deref(), &headers)
}

/// Print the `[[mcp]]` block for a URL server. Validates the same way the
/// loader will (so a literal `Authorization` is refused here, with the fix),
/// and writes nothing.
fn add_url(
    name: &str,
    url: &str,
    bearer_env: Option<&str>,
    headers: &[(String, String)],
) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!(
            "`{name}` is not a valid server name (letters, digits, `.`/`_`/`-`, \
             alphanumeric first char, <= 64 chars)"
        ));
    }
    let cfg = ServerConfig {
        headers: headers.to_vec(),
        bearer_token_env: bearer_env.map(str::to_string),
        ..ServerConfig::http(name, url)
    };
    cfg.validate()?;
    let mut table = toml_edit::Table::new();
    table["name"] = toml_edit::value(name);
    table["url"] = toml_edit::value(url);
    if let Some(var) = bearer_env {
        table["bearer_token_env"] = toml_edit::value(var);
    }
    if !headers.is_empty() {
        let mut arr = toml_edit::Array::new();
        for (k, v) in headers {
            let mut pair = toml_edit::Array::new();
            pair.push(k.as_str());
            pair.push(v.as_str());
            arr.push(pair);
        }
        table["headers"] = toml_edit::value(arr);
    }
    table["description"] = toml_edit::value("");
    let mut aot = toml_edit::ArrayOfTables::new();
    aot.push(table);
    let mut doc = toml_edit::DocumentMut::new();
    doc["mcp"] = toml_edit::Item::ArrayOfTables(aot);
    let block: String = doc
        .to_string()
        .lines()
        .map(|l| format!("    {l}\n"))
        .collect();
    let token_note = match bearer_env {
        Some(var) => format!(
            "\n\nThe token is read from `${var}` at connect time — export it in the shell \
             that runs hotl; it never goes in this file."
        ),
        None => String::new(),
    };
    Ok(format!(
        "add this to ~/.config/hotl/config.toml\n\n{block}\n\
         Fill in `description` — the model reads it to decide when to use the \
         server.{token_note}\n\n\
         hotl does not write this for you. config.toml is in the protected \
         floor, and a CLI that edited it would be a path the model's own bash \
         could take. Paste it yourself; hotl will show you the server's address \
         the first time a turn uses it."
    ))
}

/// Letters, digits, `.`/`_`/`-`; alphanumeric first char; <= 64 chars. Keeps
/// the printed block well-formed and the name usable as the `mcp` tool's
/// `server` argument.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Print the `[[mcp]]` block to paste. Writes nothing — see the module
/// INVARIANT.
fn add(name: &str, command: &str, args: &[String]) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!(
            "`{name}` is not a valid server name (letters, digits, `.`/`_`/`-`, \
             alphanumeric first char, <= 64 chars)"
        ));
    }
    let mut table = toml_edit::Table::new();
    table["name"] = toml_edit::value(name);
    table["command"] = toml_edit::value(command);
    let mut arr = toml_edit::Array::new();
    for a in args {
        arr.push(a.as_str());
    }
    table["args"] = toml_edit::value(arr);
    table["description"] = toml_edit::value("");
    let mut aot = toml_edit::ArrayOfTables::new();
    aot.push(table);
    let mut doc = toml_edit::DocumentMut::new();
    doc["mcp"] = toml_edit::Item::ArrayOfTables(aot);
    let block: String = doc
        .to_string()
        .lines()
        .map(|l| format!("    {l}\n"))
        .collect();
    Ok(format!(
        "add this to ~/.config/hotl/config.toml\n\n{block}\n\
         Fill in `description` — the model reads it to decide when to use the \
         server.\n\n\
         hotl does not write this for you. config.toml is in the protected \
         floor, and a CLI that edited it would be a path the model's own bash \
         could take. Paste it yourself; hotl will show you the server's \
         fingerprint the first time a turn uses it."
    ))
}

/// Drop a grant, forcing a re-screen. The one mutation this command makes.
fn untrust(config_dir: &Path, name: &str) -> Result<String, String> {
    let mut store = TrustStore::load(config_dir);
    if name == "--all" {
        let all: Vec<String> = store.entries().map(|(n, _)| n.to_string()).collect();
        if all.is_empty() {
            return Ok("no grants recorded — nothing to revoke".into());
        }
        for n in &all {
            store.forget(n)?;
        }
        return Ok(format!(
            "revoked {} grant(s); each server will be screened again",
            all.len()
        ));
    }
    if store.forget(name)? {
        Ok(format!(
            "revoked `{name}` — it will be screened again on next use"
        ))
    } else {
        Err(format!(
            "no grant recorded for `{name}` — see: hotl mcp list"
        ))
    }
}

fn test(config_dir: &Path, data_dir: &Path, name: &str) -> Result<String, String> {
    test_with(config_dir, data_dir, name, std::io::stdin().is_terminal())
}

/// Start the server and list its tools. Screens first when it is not already
/// trusted, and records nothing either way. `interactive` is injected so the
/// refusal path is testable without depending on the harness's stdin.
fn test_with(
    config_dir: &Path,
    data_dir: &Path,
    name: &str,
    interactive: bool,
) -> Result<String, String> {
    let servers = all_servers(config_dir, data_dir);
    let cfg = find(&servers, name)?;
    let fp = Fingerprint::of(cfg);
    let state = TrustStore::load(config_dir).state(name, &fp, hotl_tools::workspace_root());
    if state != TrustState::Trusted {
        confirm_spawn(name, &fp, state, interactive)?;
    }
    let tools = probe(cfg)?;
    let verb = if cfg.is_remote() {
        "connected"
    } else {
        "started"
    };
    if tools.is_empty() {
        return Ok(format!("`{name}` {verb} and answered, but lists no tools"));
    }
    let width = tools
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    let listed: String = tools
        .iter()
        .map(|(n, d)| format!("  {n:<width$}  {}\n", clip(d)))
        .collect();
    Ok(format!(
        "`{name}` {verb} and answered. {} tool(s):\n{listed}\
         \nNothing was recorded — trust is granted only from the in-session screen.",
        tools.len()
    ))
}

/// The same fingerprint text the in-session screen shows. Refuses without a
/// terminal so this can never become a scripted approval.
fn confirm_spawn(
    name: &str,
    fp: &Fingerprint,
    state: TrustState,
    interactive: bool,
) -> Result<(), String> {
    if !interactive {
        return Err(format!(
            "`{name}` is not trusted ({}), so starting it needs a terminal to \
             screen it. Run this from a shell.",
            state.label()
        ));
    }
    println!("{fp}\n  trust: {}\n", state.label());
    let verb = if matches!(fp, Fingerprint::Url { .. }) {
        "connect to"
    } else {
        "start"
    };
    print!("{verb} `{name}` to list its tools? [y/N] ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("could not read your answer ({e})"))?;
    if line.trim().eq_ignore_ascii_case("y") {
        Ok(())
    } else {
        Err("not started".into())
    }
}

/// Connect, handshake, list — through the same path a turn takes
/// (`hotl_mcp::tool::connect`: the egress check and the token for a URL
/// server, the spawn for a program). Its own runtime, as `doctor_main` does.
fn probe(cfg: &ServerConfig) -> Result<Vec<(String, String)>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not build a runtime ({e})"))?;
    rt.block_on(async {
        let client = hotl_mcp::tool::connect(cfg, &hotl_platform::EnvSecrets).await?;
        let tools = client.list_tools().await?;
        Ok(tools.into_iter().map(|t| (t.name, t.description)).collect())
    })
}

/// One-line clip (char-boundary safe).
fn clip(s: &str) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() <= 60 {
        one.to_string()
    } else {
        format!("{}…", one.chars().take(60).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config dir with a `[[mcp]]` entry pointing at a real file, so
    /// `Fingerprint::of` hashes something.
    fn fixture(dir: &Path, name: &str) -> std::path::PathBuf {
        let bin = dir.join("server-bin");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[[mcp]]\nname = \"{name}\"\ncommand = {}\ndescription = \"d\"\n",
                crate::config::toml_path(&bin)
            ),
        )
        .unwrap();
        bin
    }

    /// A `ServerConfig` for a real file, so `Fingerprint::of` hashes something.
    fn cfg_for(name: &str, bin: &Path) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            command: Some(bin.to_string_lossy().into()),
            args: vec![],
            description: String::new(),
            env: Vec::new(),
            cwd: None,
            url: None,
            headers: Vec::new(),
            bearer_token_env: None,
        }
    }

    /// A config dir with a `[[mcp]]` URL entry (0063 T6).
    fn url_fixture(dir: &Path, bearer_env: Option<&str>) {
        let token = bearer_env
            .map(|v| format!("bearer_token_env = \"{v}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[[mcp]]\nname = \"gh\"\nurl = \"https://api.githubcopilot.com/mcp/\"\n{token}\
                 headers = [[\"X-Team\", \"core-platform\"]]\ndescription = \"d\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn list_shows_url_servers_with_origin_and_token_state() {
        let dir = tempfile::tempdir().unwrap();
        url_fixture(dir.path(), Some("HOTL_TEST_UNSET_TOKEN_0063"));
        let out = render_list(dir.path(), dir.path());
        assert!(out.contains("gh"), "{out}");
        assert!(out.contains("https://api.githubcopilot.com/mcp/"), "{out}");
        assert!(out.contains("screens on first use"), "{out}");
        assert!(
            out.contains("token: $HOTL_TEST_UNSET_TOKEN_0063 (unset)"),
            "{out}"
        );
        // One invalid entry: the roster is empty and the warning says why.
        std::fs::write(
            dir.path().join("config.toml"),
            "[[mcp]]\nname = \"bad\"\ncommand = \"/bin/x\"\nurl = \"https://x.com/mcp\"\n",
        )
        .unwrap();
        let out = render_list(dir.path(), dir.path());
        assert!(out.contains("no MCP servers configured"), "{out}");
        assert!(
            out.contains("warning: [[mcp]] loaded no servers") && out.contains("`bad`"),
            "{out}"
        );
    }

    #[test]
    fn show_renders_the_url_screen() {
        let dir = tempfile::tempdir().unwrap();
        url_fixture(dir.path(), Some("HOTL_TEST_UNSET_TOKEN_0063"));
        let out = show(dir.path(), dir.path(), "gh").unwrap();
        assert!(
            out.contains("hotl will send tool calls to https://api.githubcopilot.com"),
            "{out}"
        );
        assert!(
            out.contains("token from $HOTL_TEST_UNSET_TOKEN_0063"),
            "{out}"
        );
        assert!(
            out.contains("X-Team: core…") && !out.contains("core-platform"),
            "{out}"
        );
        assert!(out.contains("key: fpu1:"), "{out}");
        assert!(out.contains("(unset)"), "{out}");
    }

    #[test]
    fn add_url_prints_a_pasteable_url_block() {
        let out = add_url_args(
            "gh",
            "https://api.githubcopilot.com/mcp/",
            &[
                "--bearer-env".into(),
                "GITHUB_MCP_TOKEN".into(),
                "--header".into(),
                "X-Team=core".into(),
            ],
        )
        .unwrap();
        assert!(out.contains("[[mcp]]"), "{out}");
        assert!(
            out.contains("url = \"https://api.githubcopilot.com/mcp/\""),
            "{out}"
        );
        assert!(
            out.contains("bearer_token_env = \"GITHUB_MCP_TOKEN\""),
            "{out}"
        );
        assert!(out.contains("[\"X-Team\", \"core\"]"), "{out}");
        assert!(!out.contains("command ="), "{out}");
        assert!(out.contains("read from `$GITHUB_MCP_TOKEN`"), "{out}");
        assert!(
            add_url_args("gh", "http://example.com/mcp", &[]).is_err(),
            "plain http"
        );
        assert!(add_url_args("gh", "https://x.com/mcp", &["--bogus".into()]).is_err());
    }

    #[test]
    fn add_url_refuses_a_literal_authorization_header() {
        let err = add_url_args(
            "gh",
            "https://x.com/mcp",
            &["--header".into(), "Authorization=Bearer abc".into()],
        )
        .unwrap_err();
        assert!(err.contains("bearer_token_env"), "{err}");
    }

    #[test]
    fn test_on_a_url_server_with_an_unset_token_names_the_var() {
        let dir = tempfile::tempdir().unwrap();
        url_fixture(dir.path(), Some("HOTL_TEST_UNSET_TOKEN_0063"));
        // Trusted already, so `test` goes straight to the connect path.
        let cfg = all_servers(dir.path(), dir.path()).remove(0);
        TrustStore::load(dir.path())
            .record("gh", &Fingerprint::of(&cfg))
            .unwrap();
        let started = std::time::Instant::now();
        let err = test_with(dir.path(), dir.path(), "gh", false).unwrap_err();
        assert!(
            err.contains("needs a token") && err.contains("`HOTL_TEST_UNSET_TOKEN_0063`"),
            "{err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "no connect attempted"
        );
    }

    #[test]
    fn list_reports_state_and_stale_grants() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fixture(dir.path(), "docs");
        let out = render_list(dir.path(), dir.path());
        assert!(
            out.contains("docs") && out.contains("screens on first use"),
            "{out}"
        );

        // A grant for a server that is no longer configured is called out.
        let mut store = TrustStore::load(dir.path());
        store
            .record("gone", &Fingerprint::of(&cfg_for("gone", &bin)))
            .unwrap();
        let out = render_list(dir.path(), dir.path());
        assert!(out.contains("no longer configured"), "{out}");
        assert!(out.contains("hotl mcp untrust gone"), "{out}");

        // A recorded grant for a configured server reads as trusted.
        let mut store = TrustStore::load(dir.path());
        store
            .record("docs", &Fingerprint::of(&cfg_for("docs", &bin)))
            .unwrap();
        assert!(
            render_list(dir.path(), dir.path()).contains("trusted"),
            "{out}"
        );
    }

    /// The roster invariant, seen from the CLI: a plugin's stdio server
    /// appears as `<plugin>:<server>` with its trust state, because list
    /// reads the same `all_mcp_servers` composition the registry does.
    #[test]
    fn list_shows_plugin_servers_with_trust_state() {
        let dir = tempfile::tempdir().unwrap();
        let plug = dir.path().join("myplug");
        std::fs::create_dir_all(plug.join("bin")).unwrap();
        std::fs::write(plug.join("bin/server"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            plug.join("plugin.json"),
            format!(
                r#"{{"$schema": "{}", "name": "acme.tools"}}"#,
                hotl_tools::plugins::MANIFEST_SCHEMA
            ),
        )
        .unwrap();
        std::fs::write(
            plug.join("mcp.json"),
            format!(
                r#"{{"$schema": "{}", "mcpServers":
                    {{"mem": {{"type": "stdio", "command": "./bin/server"}}}}}}"#,
                hotl_tools::plugins::MCP_SCHEMA
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "[plugins.sources]\n\"acme.tools\" = {}\n",
                crate::config::toml_path(&plug)
            ),
        )
        .unwrap();

        let out = render_list(dir.path(), dir.path());
        assert!(out.contains("acme.tools:mem"), "{out}");
        assert!(out.contains("screens on first use"), "{out}");
        // Discovery created the plugin's data dir (it has a stdio server).
        assert!(dir.path().join("plugin-data/acme.tools").is_dir());
    }

    #[test]
    fn list_is_clean_when_nothing_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let out = render_list(dir.path(), dir.path());
        assert!(out.contains("no MCP servers configured"), "{out}");
    }

    #[test]
    fn show_renders_the_screen_text_and_errors_helpfully() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), "docs");
        let out = show(dir.path(), dir.path(), "docs").unwrap();
        assert!(out.contains("binary:") && out.contains("sha256:"), "{out}");
        assert!(out.contains("key: fp3:"), "{out}");
        // Errors are prompts: name the known servers and the next command.
        let err = show(dir.path(), dir.path(), "nope").unwrap_err();
        assert!(
            err.contains("docs") && err.contains("hotl mcp list"),
            "{err}"
        );
    }

    #[test]
    fn add_prints_a_pasteable_block() {
        let out = add("docs", "npx", &["-y".into(), "@acme/docs-mcp".into()]).unwrap();
        assert!(out.contains("[[mcp]]"), "{out}");
        assert!(out.contains("name = \"docs\""), "{out}");
        assert!(out.contains("\"@acme/docs-mcp\""), "{out}");
        assert!(out.contains("does not write this for you"), "{out}");
        assert!(add("bad name", "x", &[]).is_err());
        assert!(add("-leading", "x", &[]).is_err());
        assert!(add("", "x", &[]).is_err());
    }

    /// The security invariant: every handler, including the failing ones,
    /// leaves config.toml byte-identical — and none conjures one that was
    /// absent. Handlers are called directly rather than through `mcp_main`,
    /// which resolves the *real* config dir.
    #[test]
    fn no_verb_ever_writes_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fixture(dir.path(), "docs");
        let path = dir.path().join("config.toml");
        let before = std::fs::read(&path).unwrap();
        TrustStore::load(dir.path())
            .record("docs", &Fingerprint::of(&cfg_for("docs", &bin)))
            .unwrap();

        let _ = render_list(dir.path(), dir.path());
        let _ = show(dir.path(), dir.path(), "docs");
        let _ = show(dir.path(), dir.path(), "nope");
        let _ = add("new", "npx", &["-y".into()]);
        let _ = add("bad name", "npx", &[]);
        let _ = add_url_args(
            "gh",
            "https://x.com/mcp",
            &["--bearer-env".into(), "T".into()],
        );
        let _ = add_url_args("gh", "http://example.com/mcp", &[]);
        let _ = test_with(dir.path(), dir.path(), "docs", false);
        let _ = untrust(dir.path(), "docs");
        let _ = untrust(dir.path(), "--all");
        let _ = untrust(dir.path(), "ghost");
        assert_eq!(std::fs::read(&path).unwrap(), before, "config.toml changed");

        // Nothing conjures a config.toml where none existed.
        let empty = tempfile::tempdir().unwrap();
        let _ = render_list(empty.path(), empty.path());
        let _ = add("docs", "npx", &[]);
        let _ = untrust(empty.path(), "--all");
        assert!(!empty.path().join("config.toml").exists());
    }

    #[test]
    fn untrust_revokes_and_reports_the_empty_case() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fixture(dir.path(), "docs");
        let fp = Fingerprint::of(&cfg_for("docs", &bin));

        assert!(untrust(dir.path(), "docs").is_err(), "no grant yet");
        assert!(untrust(dir.path(), "--all")
            .unwrap()
            .contains("nothing to revoke"));

        TrustStore::load(dir.path()).record("docs", &fp).unwrap();
        assert!(TrustStore::load(dir.path()).is_trusted("docs", &fp));
        assert!(untrust(dir.path(), "docs").unwrap().contains("revoked"));
        assert!(!TrustStore::load(dir.path()).is_trusted("docs", &fp));

        TrustStore::load(dir.path()).record("docs", &fp).unwrap();
        assert!(untrust(dir.path(), "--all").unwrap().contains("1 grant"));
        assert!(!TrustStore::load(dir.path()).is_trusted("docs", &fp));
    }

    /// `test` on an untrusted server must never reach a spawn without a
    /// terminal, and must record nothing.
    #[test]
    fn test_records_no_trust() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), "docs");
        let err = test_with(dir.path(), dir.path(), "docs", false).unwrap_err();
        assert!(err.contains("needs a terminal"), "{err}");
        assert!(
            !dir.path().join("trust.toml").exists(),
            "a refused screen records nothing"
        );
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert_eq!(mcp_main(&["mcp".into(), "frobnicate".into()]), 2);
        assert_eq!(mcp_main(&["mcp".into(), "show".into()]), 2);
        assert_eq!(
            mcp_main(&["mcp".into(), "add".into(), "only-name".into()]),
            2
        );
        assert_eq!(mcp_main(&["mcp".into(), "untrust".into()]), 2);
    }

    #[test]
    fn valid_name_matches_the_documented_charset() {
        assert!(valid_name("docs") && valid_name("a.b_c-1") && valid_name("9x"));
        assert!(!valid_name("") && !valid_name("-x") && !valid_name("a b") && !valid_name("a/b"));
        // `:` is reserved for plugin server names (`<plugin>:<server>`),
        // so a hand-added `[[mcp]]` entry can never collide with one.
        assert!(!valid_name("acme:mem"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}
