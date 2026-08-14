//! `hotl plugins` — inspect and register Agent Plugins (exec-plan 0032),
//! the function-for-function sibling of `skills_cli`.
//!
//! `list` shows every loaded plugin with its components, per-server trust
//! state, and load reports. Registration management (`add` / `update` /
//! `remove`) runs git only on those explicit commands — discovery never
//! touches the network. `add` validates the fresh checkout and prints its
//! reports, so a broken plugin is visible when installed, not at first
//! run. `remove` deletes a managed checkout but always preserves the
//! plugin's data directory (the spec MAY delete it; hotl keeps it and
//! prints the path).

use std::path::Path;

use hotl_mcp::trust::{Fingerprint, TrustStore};

pub fn plugins_main(args: &[String]) -> i32 {
    let config_dir = crate::agent::config_dir();
    let data_dir = crate::agent::data_dir();
    match args.get(1).map(String::as_str) {
        None | Some("list") => {
            print!("{}", render_list(&config_dir, &data_dir));
            0
        }
        Some("add") => match (args.get(2), args.get(3)) {
            (Some(handle), Some(source)) => report(add(&config_dir, &data_dir, handle, source)),
            _ => usage(),
        },
        Some("update") => report(update(&config_dir, args.get(2).map(String::as_str))),
        Some("remove") => match args.get(2) {
            Some(handle) => report(remove(&config_dir, &data_dir, handle)),
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
            eprintln!("hotl plugins: {e}");
            1
        }
    }
}

fn usage() -> i32 {
    eprintln!(
        "usage: hotl plugins [list] | add <handle> <git-url|path> | update [handle] \
         | remove <handle>"
    );
    2
}

/// One line per plugin (name, handle, version, component counts), its
/// servers with trust state, then load reports and a warning per
/// registered git plugin whose managed checkout is missing.
fn render_list(config_dir: &Path, data_dir: &Path) -> String {
    let cfg = crate::config::Config::load(config_dir);
    let (entries, warnings) = cfg.plugins.load(config_dir, data_dir);
    let store = TrustStore::load(config_dir);
    let workspace = hotl_tools::workspace_root();
    let mut out = String::new();
    for e in &entries {
        let version = e
            .manifest
            .version
            .as_deref()
            .unwrap_or("unversioned")
            .to_string();
        out.push_str(&format!(
            "{} ({version}, handle `{}`): {} skill(s), {} MCP server(s)\n",
            e.name,
            e.handle,
            e.skill_dirs.len(),
            e.servers.len()
        ));
        // Trust state through the same composition a turn loads, so the
        // state shown is the state the gate will apply.
        for server in crate::config::all_mcp_servers(&cfg, std::slice::from_ref(e)) {
            if !server.name.starts_with(&format!("{}:", e.name)) {
                continue; // a [[mcp]] row, not this plugin's
            }
            let state = store.state(&server.name, &Fingerprint::of(&server), workspace);
            out.push_str(&format!("  {}  {}\n", server.name, state.label()));
        }
    }
    if entries.is_empty() {
        out.push_str("no plugins loaded — run: hotl plugins add <handle> <git-url|path>\n");
    }
    // Load reports (broken components, skipped transports, mismatches).
    for w in &warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    for (handle, source) in &cfg.plugins.sources {
        if crate::config::is_git_url(source) && !config_dir.join("plugins").join(handle).is_dir() {
            out.push_str(&format!(
                "warning: plugin `{handle}` is registered but not fetched — \
                 run: hotl plugins update {handle}\n"
            ));
        }
    }
    out
}

/// Register a plugin: validate the handle, clone first when the source is
/// a git URL (config is written only after a successful clone), write the
/// entry preserving the document's text — then load the fresh checkout
/// and print what it contains, reports included.
fn add(config_dir: &Path, data_dir: &Path, handle: &str, source: &str) -> Result<String, String> {
    let handle = hotl_tools::skills::normalize_marketplace_name(handle).ok_or_else(|| {
        format!(
            "`{handle}` is not a valid plugin handle (letters, digits, \
             `.`/`_`/`-`, alphanumeric first char, ≤ 64 chars)"
        )
    })?;
    let path = config_dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    let registered = doc
        .get("plugins")
        .and_then(|p| p.get("sources"))
        .and_then(|s| s.get(&handle))
        .is_some();
    if registered {
        return Err(format!("plugin `{handle}` is already registered"));
    }
    let mut note = String::new();
    let root = if crate::config::is_git_url(source) {
        let dest = config_dir.join("plugins").join(&handle);
        if dest.exists() {
            return Err(format!("{} already exists", dest.display()));
        }
        std::fs::create_dir_all(config_dir.join("plugins"))
            .map_err(|e| format!("cannot create plugins dir: {e}"))?;
        git(&["clone", source, &dest.to_string_lossy()])?;
        note = format!(" (cloned to {})", dest.display());
        dest
    } else {
        crate::config::expand_home(source)
    };
    let plugins = doc.entry("plugins").or_insert(toml_edit::table());
    let plugins = plugins
        .as_table_mut()
        .ok_or("[plugins] in config.toml is not a table")?;
    plugins.set_implicit(true);
    let sources = plugins.entry("sources").or_insert(toml_edit::table());
    let sources = sources
        .as_table_mut()
        .ok_or("[plugins.sources] in config.toml is not a table")?;
    sources.insert(&handle, toml_edit::value(source));
    std::fs::create_dir_all(config_dir).map_err(|e| format!("cannot create config dir: {e}"))?;
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("cannot write config.toml: {e}"))?;

    // Validate what was just installed, so a broken plugin is visible now.
    let loaded = hotl_tools::plugins::load_plugin(&handle, &root, &data_dir.join("plugin-data"));
    let mut msg = format!("registered plugin `{handle}`{note}");
    match &loaded.entry {
        Some(e) => msg.push_str(&format!(
            "\nloaded: {} ({}) — {} skill(s), {} MCP server(s)",
            e.name,
            e.manifest.version.as_deref().unwrap_or("unversioned"),
            e.skill_dirs.len(),
            e.servers.len()
        )),
        None => msg.push_str("\nthe plugin did not load:"),
    }
    for r in &loaded.reports {
        msg.push_str(&format!("\n  report: {r}"));
    }
    Ok(msg)
}

/// `git pull --ff-only` each managed checkout (or just `only`); a
/// registered-but-missing checkout is cloned; local-path sources are
/// skipped with a note.
fn update(config_dir: &Path, only: Option<&str>) -> Result<String, String> {
    let cfg = crate::config::Config::load(config_dir);
    let mut lines = Vec::new();
    let mut matched = false;
    for (handle, source) in &cfg.plugins.sources {
        if only.is_some_and(|o| o != handle) {
            continue;
        }
        matched = true;
        if !crate::config::is_git_url(source) {
            lines.push(format!("{handle}: local path — skipped"));
            continue;
        }
        let dest = config_dir.join("plugins").join(handle);
        if dest.is_dir() {
            git(&["-C", &dest.to_string_lossy(), "pull", "--ff-only"])?;
            lines.push(format!("{handle}: updated"));
        } else {
            std::fs::create_dir_all(config_dir.join("plugins"))
                .map_err(|e| format!("cannot create plugins dir: {e}"))?;
            git(&["clone", source, &dest.to_string_lossy()])?;
            lines.push(format!("{handle}: cloned"));
        }
    }
    match (matched, only) {
        (false, Some(o)) => Err(format!("no plugin named `{o}` is registered")),
        (false, None) => Ok("no plugins registered".into()),
        _ => Ok(lines.join("\n")),
    }
}

/// Unregister a plugin. A managed checkout under `<config_dir>/plugins/`
/// is deleted (it is re-fetchable); a local-path source is never touched;
/// the plugin's data directory is always preserved (spec §9.1 MAY delete
/// — hotl keeps state across reinstalls and says where it lives).
fn remove(config_dir: &Path, data_dir: &Path, handle: &str) -> Result<String, String> {
    let path = config_dir.join("config.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "no config.toml — nothing is registered".to_string())?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    let source = doc
        .get("plugins")
        .and_then(|p| p.get("sources"))
        .and_then(|s| s.get(handle))
        .and_then(|v| v.as_str())
        .map(String::from);
    let Some(source) = source else {
        return Err(format!("no plugin named `{handle}` is registered"));
    };
    // The data dir is keyed by manifest name; read it before the checkout
    // goes away.
    let root = if crate::config::is_git_url(&source) {
        config_dir.join("plugins").join(handle)
    } else {
        crate::config::expand_home(&source)
    };
    let data = hotl_tools::plugins::load_plugin(handle, &root, &data_dir.join("plugin-data"))
        .entry
        .map(|e| e.plugin_data);

    doc["plugins"]["sources"]
        .as_table_mut()
        .ok_or("[plugins.sources] in config.toml is not a table")?
        .remove(handle);
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("cannot write config.toml: {e}"))?;
    let mut note = String::new();
    if crate::config::is_git_url(&source) {
        let dest = config_dir.join("plugins").join(handle);
        if dest.is_dir() {
            std::fs::remove_dir_all(&dest).map_err(|e| format!("checkout not removed: {e}"))?;
            note = format!(" (checkout {} deleted)", dest.display());
        }
    }
    let mut msg = format!("removed plugin `{handle}`{note}");
    if let Some(data) = data.filter(|d| d.is_dir()) {
        msg.push_str(&format!(
            "\nits data directory is preserved at {} — delete it yourself if \
             you want it gone",
            data.display()
        ));
    }
    Ok(msg)
}

/// Run git with output passing through; actionable error when git itself
/// is missing from PATH.
fn git(args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("git")
        .args(args)
        .status()
        .map_err(|e| format!("cannot run git ({e}) — is git on your PATH?"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`git {}` failed (see output above)",
            args.join(" ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_plugin(root: &Path, name: &str, with_mcp: bool) {
        std::fs::create_dir_all(root.join("skills/how-to")).unwrap();
        std::fs::write(
            root.join("skills/how-to/SKILL.md"),
            "---\nname: how-to\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("plugin.json"),
            format!(
                r#"{{"$schema": "{}", "name": "{name}", "version": "1.2.0"}}"#,
                hotl_tools::plugins::MANIFEST_SCHEMA
            ),
        )
        .unwrap();
        if with_mcp {
            std::fs::write(
                root.join("mcp.json"),
                format!(
                    r#"{{"$schema": "{}", "mcpServers": {{
                        "mem": {{"type": "stdio", "command": "npx"}},
                        "remote": {{"type": "sse", "url": "https://x.com/sse"}}}}}}"#,
                    hotl_tools::plugins::MCP_SCHEMA
                ),
            )
            .unwrap();
        }
    }

    #[test]
    fn add_local_path_writes_config_without_cloning_and_reports_contents() {
        let dir = tempfile::tempdir().unwrap();
        let plug = dir.path().join("team-plugin");
        write_plugin(&plug, "team-plugin", true);
        let out = add(
            dir.path(),
            dir.path(),
            "team-plugin",
            &plug.to_string_lossy(),
        )
        .unwrap();
        assert!(out.contains("registered plugin `team-plugin`"), "{out}");
        // Install-time validation: contents and the skipped transport are
        // visible now, not at first run.
        assert!(out.contains("1 skill(s), 1 MCP server(s)"), "{out}");
        assert!(out.contains("unsupported transport"), "{out}");
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("[plugins.sources]"), "{text}");
        assert!(!dir.path().join("plugins").exists(), "no clone for a path");
        // Duplicate registration and invalid handles error.
        assert!(add(dir.path(), dir.path(), "team-plugin", "/elsewhere").is_err());
        assert!(add(dir.path(), dir.path(), "bad:name", "/x").is_err());
    }

    #[test]
    fn add_preserves_existing_config_text_and_quotes_dotted_handles() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "# my config\n[skills]\nclaude = false   # keep\n",
        )
        .unwrap();
        let plug = dir.path().join("p");
        write_plugin(&plug, "acme.tools", false);
        add(
            dir.path(),
            dir.path(),
            "acme.tools",
            &plug.to_string_lossy(),
        )
        .unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(
            text.contains("# my config") && text.contains("claude = false   # keep"),
            "{text}"
        );
        // A dotted handle must round-trip as ONE key, not a nested table —
        // and the loader must see it again.
        let cfg = crate::config::Config::load(dir.path());
        assert!(cfg.plugins.sources.contains_key("acme.tools"), "{text}");
        let (entries, _) = cfg.plugins.load(dir.path(), dir.path());
        assert_eq!(entries.len(), 1, "{text}");
    }

    #[test]
    fn list_shows_components_reports_and_unfetched_warning() {
        let dir = tempfile::tempdir().unwrap();
        let plug = dir.path().join("p");
        write_plugin(&plug, "acme", true);
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "[plugins.sources]\nacme = {}\n\
                 ghost = \"https://example.com/ghost.git\"\n",
                crate::config::toml_path(&plug)
            ),
        )
        .unwrap();
        let out = render_list(dir.path(), dir.path());
        assert!(
            out.contains("acme (1.2.0, handle `acme`): 1 skill(s), 1 MCP server(s)"),
            "{out}"
        );
        assert!(out.contains("acme:mem"), "{out}");
        assert!(
            out.contains("skipped — unsupported transport `sse`"),
            "{out}"
        );
        assert!(
            out.contains("`ghost` is registered but not fetched"),
            "{out}"
        );
        assert!(out.contains("hotl plugins update ghost"), "{out}");
    }

    #[test]
    fn remove_preserves_local_sources_and_plugin_data() {
        let dir = tempfile::tempdir().unwrap();
        let plug = dir.path().join("team");
        write_plugin(&plug, "team", true);
        add(dir.path(), dir.path(), "team", &plug.to_string_lossy()).unwrap();
        // Simulate accumulated server state.
        let data = dir.path().join("plugin-data/team");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("cache.db"), b"state").unwrap();

        let out = remove(dir.path(), dir.path(), "team").unwrap();
        assert!(plug.is_dir(), "a local source is never touched");
        assert!(
            data.join("cache.db").is_file(),
            "plugin data survives removal"
        );
        assert!(out.contains("preserved"), "{out}");
        assert!(out.contains(&data.display().to_string()), "{out}");
        assert!(remove(dir.path(), dir.path(), "team").is_err());
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} in {}", dir.display());
    }

    /// A local origin repo at a path ending in `.git` containing a full
    /// valid plugin, so `add` treats it as a managed (cloned) source.
    fn make_origin(root: &Path) -> PathBuf {
        let origin = root.join("origin.git");
        write_plugin(&origin, "acme", true);
        git_in(&origin, &["init"]);
        git_in(&origin, &["add", "."]);
        git_in(&origin, &["commit", "-m", "init", "--no-gpg-sign"]);
        origin
    }

    #[test]
    fn add_update_remove_manage_a_git_checkout() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let origin = make_origin(dir.path());
        let source = origin.to_string_lossy().to_string();

        let out = add(dir.path(), dir.path(), "acme", &source).unwrap();
        assert!(out.contains("cloned to"), "{out}");
        assert!(out.contains("1 skill(s), 1 MCP server(s)"), "{out}");
        let checkout = dir.path().join("plugins/acme");
        assert!(checkout.join("plugin.json").is_file());

        // A new skill lands in origin; update fast-forwards the checkout.
        std::fs::create_dir_all(origin.join("skills/second")).unwrap();
        std::fs::write(
            origin.join("skills/second/SKILL.md"),
            "---\nname: second\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        git_in(&origin, &["add", "."]);
        git_in(&origin, &["commit", "-m", "second", "--no-gpg-sign"]);
        update(dir.path(), Some("acme")).unwrap();
        assert!(checkout.join("skills/second/SKILL.md").is_file());

        // Unknown name errors; remove deletes the managed checkout but
        // keeps the data dir the stdio server earned at add-time load.
        assert!(update(dir.path(), Some("nope")).is_err());
        let data = dir.path().join("plugin-data/acme");
        std::fs::create_dir_all(&data).unwrap();
        let out = remove(dir.path(), dir.path(), "acme").unwrap();
        assert!(!checkout.exists());
        assert!(data.is_dir(), "plugin data survives: {out}");
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!text.contains("acme"), "{text}");
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        let args: Vec<String> = vec!["plugins".into(), "frobnicate".into()];
        assert_eq!(plugins_main(&args), 2);
    }
}
