//! Agent Plugins 1.0.0 conformance suite (exec-plan 0032, T7).
//!
//! One test per Appendix A checklist row, named `a_<row>`, asserting only
//! through public seams: `hotl_tools::plugins::{load_plugin,
//! parse_mcp_json, validate_plugin_name, expand}`,
//! `hotl_tools::skills::SkillTool::with_roots`,
//! `hotl_mcp::config::ServerConfig::build_command`, and the built `hotl`
//! binary where the composed roster and discovery-time side effects are
//! the claim. Appendix A is non-normative; each test's doc comment cites
//! the governing normative section. This suite is the proof artifact —
//! the per-module unit tests are the development artifact.
//!
//! Checklist map — every Appendix A row → its test:
//!
//! Plugin loader
//! - Parse and validate `plugin.json` ............ `a_parse_and_validate_plugin_json`
//! - Validate required `$schema`/`name` .......... `a_validate_required_fields`
//! - Validate plugin name constraints ............ `a_validate_plugin_name_constraints`
//! - Report and ignore unknown fields ............ `a_report_and_ignore_unknown_fields`
//! - Ignore unimplemented `extensions` ........... `a_ignore_unimplemented_extension_namespaces`
//! - Reject escaping package paths ............... `a_reject_escaping_package_paths` (unix)
//! - Discover implemented file-based extensions .. `a_extension_dirs_are_ignored_untouched`
//!   (hotl implements no namespace, so the obligation inverts: an
//!   extension directory must affect nothing)
//!
//! Component discovery
//! - Scan fixed component locations .............. `a_scan_fixed_component_locations`
//! - Ignore missing fixed locations .............. `a_ignore_missing_fixed_locations`
//!
//! MCP configuration
//! - Select `$schema`, validate closed schema .... `a_validate_mcp_schema_and_variants`
//! - Implement stdio or streamable-http .......... `a_stdio_transport_is_implemented`
//! - Use the declared transport .................. `a_declared_transport_is_used`
//! - Remote URL and header requirements .......... `a_remote_url_and_header_requirements`
//!
//! Environment and expansion
//! - Provide `PLUGIN_ROOT` + writable `PLUGIN_DATA` `a_plugin_root_and_data_are_provided`
//! - `command` is a single token ................. `a_command_resolves_as_a_single_token`
//! - Default cwd is the plugin root .............. `a_cwd_defaults_to_plugin_root`
//! - Explicit cwd forms + containment ............ `a_cwd_forms_and_containment`
//! - Overlay env on a base environment ........... `a_env_overlays_the_base_environment`
//! - Reserved vars set last, replacing ........... `a_reserved_vars_set_last_replace_configured`
//! - Configured PATH need not affect bare cmds ... `a_configured_path_does_not_affect_bare_commands`
//! - Expand only the two placeholders ............ `a_expand_only_the_two_placeholders`
//!
//! Resilience
//! - Ignore unsupported component types .......... `a_unsupported_component_types_are_ignored`
//! - Skip unsupported transports cleanly ......... `a_unsupported_transport_skips_only_that_server`
//! - Continue past independent failures .......... `a_component_failures_are_isolated`
//! - Support at least one component type ......... `a_both_component_types_are_supported`

use std::path::{Path, PathBuf};

use hotl_mcp::config::ServerConfig;
use hotl_tools::plugins::{
    expand, load_plugin, parse_mcp_json, validate_plugin_name, MANIFEST_SCHEMA, MCP_SCHEMA,
};

fn manifest(name: &str) -> String {
    format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "{name}"}}"#)
}

/// A plugin root with a valid manifest, canonicalized (macOS tempdirs
/// resolve through `/private`).
fn plugin_root(dir: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("plugin.json"), manifest(name)).unwrap();
    dunce::canonicalize(dir).unwrap()
}

fn write_skill(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    // No `name:` — the directory name is the skill name (§7.1 example).
    std::fs::write(dir.join("SKILL.md"), "---\ndescription: d\n---\nbody\n").unwrap();
}

fn mcp_doc(servers: &str) -> String {
    format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": {{{servers}}}}}"#)
}

fn data_base(dir: &Path) -> PathBuf {
    dir.join("plugin-data")
}

/// The `ServerConfig` a loaded plugin server becomes in the roster —
/// the same composition `all_mcp_servers` applies (pinned by the
/// `all_mcp_servers_appends_qualified_plugin_servers` unit test; the
/// binary-level test below crosses the real seam).
fn to_server_config(plugin: &str, s: &hotl_tools::plugins::PluginServer) -> ServerConfig {
    ServerConfig {
        name: format!("{plugin}:{}", s.name),
        command: s.command.clone(),
        args: s.args.clone(),
        description: String::new(),
        env: s.env.clone(),
        cwd: Some(s.cwd.clone()),
    }
}

fn envs_of(cfg: &ServerConfig) -> Vec<(String, String)> {
    let cmd = cfg.build_command();
    cmd.as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.unwrap_or_default().to_string_lossy().into_owned(),
            )
        })
        .collect()
}

// ───────────────────────── Plugin loader ─────────────────────────

/// §5.1/§5.2: the manifest loads from `plugin.json` at the root and is
/// validated against the closed schema; a broken one rejects the plugin.
#[test]
fn a_parse_and_validate_plugin_json() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "reports-plugin");
    let loaded = load_plugin("reports-plugin", &root, &data_base(dir.path()));
    assert_eq!(loaded.entry.unwrap().name, "reports-plugin");

    std::fs::write(root.join("plugin.json"), "not json").unwrap();
    let loaded = load_plugin("reports-plugin", &root, &data_base(dir.path()));
    assert!(loaded.entry.is_none());
    assert!(loaded.reports[0].contains("JSON"), "{:?}", loaded.reports);
}

/// §5.3: a missing/invalid required field rejects the plugin, and the
/// report names it.
#[test]
fn a_validate_required_fields() {
    let dir = tempfile::tempdir().unwrap();
    for (json, field) in [
        (r#"{"name": "a"}"#.to_string(), "$schema"),
        (format!(r#"{{"$schema": "{MANIFEST_SCHEMA}"}}"#), "name"),
        (
            format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": ""}}"#),
            "name",
        ),
    ] {
        let root = dir.path().join(field.trim_start_matches('$'));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("plugin.json"), &json).unwrap();
        let loaded = load_plugin("p", &root, &data_base(dir.path()));
        assert!(loaded.entry.is_none(), "{json}");
        assert!(loaded.reports[0].contains(field), "{:?}", loaded.reports);
    }
}

/// §5.5, the spec's own examples verbatim.
#[test]
fn a_validate_plugin_name_constraints() {
    for good in ["my-plugin", "acme.tools", "lint3r", "a"] {
        assert!(validate_plugin_name(good).is_ok(), "{good}");
    }
    for bad in ["My-Plugin", "-start", "has--double", "too.many..dots", ""] {
        assert!(validate_plugin_name(bad).is_err(), "{bad}");
    }
}

/// §5.2: an unknown top-level field is reported and ignored — the plugin
/// still loads.
#[test]
fn a_report_and_ignore_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    std::fs::write(
        root.join("plugin.json"),
        format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "p", "commands": ["x"]}}"#),
    )
    .unwrap();
    let loaded = load_plugin("p", &root, &data_base(dir.path()));
    assert!(loaded.entry.is_some());
    assert_eq!(loaded.reports.len(), 1);
    assert!(
        loaded.reports[0].contains("commands"),
        "{:?}",
        loaded.reports
    );
}

/// §8.1: unimplemented namespaces pass without content validation.
#[test]
fn a_ignore_unimplemented_extension_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    std::fs::write(
        root.join("plugin.json"),
        format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "p",
                "extensions": {{"com.example.client": {{"anything": [1, null, {{"x": {{}}}}]}}}}}}"#
        ),
    )
    .unwrap();
    let loaded = load_plugin("p", &root, &data_base(dir.path()));
    assert!(loaded.entry.is_some());
    assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
}

/// §4.1: package paths that resolve outside the plugin root are
/// rejected, at the narrowest applicable boundary.
#[cfg(unix)]
#[test]
fn a_reject_escaping_package_paths() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "---\nname: evil\n---\nx").unwrap();

    // Boundary 3: an escaping SKILL.md skips that skill only.
    let root = plugin_root(&dir.path().join("p1"), "p1");
    write_skill(&root.join("skills/good"));
    std::os::unix::fs::symlink(&outside, root.join("skills/evil")).unwrap();
    let loaded = load_plugin("p1", &root, &data_base(dir.path()));
    let entry = loaded.entry.unwrap();
    assert_eq!(entry.skill_dirs, vec![root.join("skills/good")]);
    assert_eq!(loaded.reports.len(), 1, "{:?}", loaded.reports);

    // Boundary 1: an escaping plugin.json rejects the plugin.
    std::fs::write(outside.join("plugin.json"), manifest("evil")).unwrap();
    let root2 = dir.path().join("p2");
    std::fs::create_dir_all(&root2).unwrap();
    std::os::unix::fs::symlink(outside.join("plugin.json"), root2.join("plugin.json")).unwrap();
    assert!(load_plugin("p2", &root2, &data_base(dir.path()))
        .entry
        .is_none());

    // Boundary 4 (via the entry boundary): an escaping command
    // invalidates that server entry.
    let root3 = plugin_root(&dir.path().join("p3"), "p3");
    std::fs::create_dir_all(root3.join("bin")).unwrap();
    std::fs::write(outside.join("bin"), "#!/bin/sh\n").unwrap();
    std::os::unix::fs::symlink(outside.join("bin"), root3.join("bin/server")).unwrap();
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "./bin/server"}"#),
        &root3,
        &data_base(dir.path()),
    );
    assert!(out.servers.is_empty());
    assert!(out.reports[0].contains("command"), "{:?}", out.reports);
}

/// §8.2 (inverted for a client with no implemented namespace): an
/// extension directory assigns no portable semantics — it must not add
/// components, produce reports, or be modified.
#[test]
fn a_extension_dirs_are_ignored_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    // Even a SKILL.md inside an extension dir is not a component: skills
    // discovery is fixed to `skills/` (§6.1).
    write_skill(&root.join("com.example.client/hooks"));
    std::fs::write(root.join("com.example.client/hooks.json"), "{}").unwrap();

    let loaded = load_plugin("p", &root, &data_base(dir.path()));
    let entry = loaded.entry.unwrap();
    assert!(entry.skill_dirs.is_empty());
    assert!(entry.servers.is_empty());
    assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
    assert!(
        root.join("com.example.client/hooks.json").is_file(),
        "the extension dir is untouched"
    );
}

// ─────────────────────── Component discovery ───────────────────────

/// §6.1: skills come from `skills/` and MCP servers from `mcp.json`, and
/// a discovered skill is actually loadable (the §6.1 `reports-plugin`
/// example, end to end through the skill tool).
#[test]
fn a_scan_fixed_component_locations() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "reports-plugin");
    write_skill(&root.join("skills/summarize"));
    std::fs::write(
        root.join("mcp.json"),
        mcp_doc(r#""mem": {"type": "stdio", "command": "npx"}"#),
    )
    .unwrap();

    let loaded = load_plugin("reports-plugin", &root, &data_base(dir.path()));
    let entry = loaded.entry.unwrap();
    assert_eq!(entry.skill_dirs, vec![root.join("skills/summarize")]);
    assert_eq!(entry.servers[0].name, "mem");

    let none = dir.path().join("none");
    let plugins = vec![("reports-plugin".to_string(), entry.skill_dirs.clone())];
    let tool = hotl_tools::skills::SkillTool::with_roots(&none, &[], &none, &none, false, &plugins)
        .expect("the plugin skill is discovered");
    let names: Vec<&str> = tool.names().collect();
    assert!(names.contains(&"summarize"), "{names:?}");
    assert!(names.contains(&"reports-plugin:summarize"), "{names:?}");
}

/// §6.2: absent fixed locations are not errors.
#[test]
fn a_ignore_missing_fixed_locations() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "solo");
    let loaded = load_plugin("solo", &root, &data_base(dir.path()));
    assert!(loaded.entry.is_some());
    assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
}

// ─────────────────────── MCP configuration ───────────────────────

/// §7.2.1/§7.2.2 rule 2 + §10.1: `$schema` selects the rules (versions
/// compared, not strings); the closed top level and closed variants are
/// enforced at their own failure boundaries.
#[test]
fn a_validate_mcp_schema_and_variants() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let data = data_base(dir.path());
    let out = parse_mcp_json(
        r#"{"$schema": "https://agent-plugins.org/schemas/2.0.0/mcp.schema.json",
            "mcpServers": {}}"#,
        &root,
        &data,
    );
    assert!(out.disabled, "a version hotl does not support disables MCP");
    let out = parse_mcp_json(
        &format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": {{}}, "x": 1}}"#),
        &root,
        &data,
    );
    assert!(out.disabled, "the top level is closed");
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "npx", "url": "https://x.com"}"#),
        &root,
        &data,
    );
    assert!(!out.disabled, "a bad entry is not a bad file");
    assert!(out.servers.is_empty(), "cross-variant field invalidates");
    let out = parse_mcp_json(&mcp_doc(""), &root, &data);
    assert!(!out.disabled && out.servers.is_empty() && out.reports.is_empty());
}

/// Transport support: hotl implements stdio (one of the two required
/// options), all the way to a prepared subprocess command.
#[test]
fn a_stdio_transport_is_implemented() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let out = parse_mcp_json(
        &mcp_doc(r#""mem": {"type": "stdio", "command": "npx", "args": ["-y", "@x/mem"]}"#),
        &root,
        &data_base(dir.path()),
    );
    let cfg = to_server_config("p", &out.servers[0]);
    let cmd = cfg.build_command();
    assert_eq!(cmd.as_std().get_program(), "npx");
    let args: Vec<_> = cmd.as_std().get_args().collect();
    assert_eq!(args, ["-y", "@x/mem"]);
}

/// Transport support: the declared transport decides the initial
/// attempt — a stdio entry becomes a subprocess spec, and a remote entry
/// never does (it is skipped whole, not downgraded).
#[test]
fn a_declared_transport_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let out = parse_mcp_json(
        &mcp_doc(
            r#""local": {"type": "stdio", "command": "npx"},
               "remote": {"type": "streamable-http", "url": "https://x.com/mcp"}"#,
        ),
        &root,
        &data_base(dir.path()),
    );
    let names: Vec<&str> = out.servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["local"], "{:?}", out.reports);
    assert!(out.reports[0].contains("unsupported transport"));
}

/// §7.2.1's remote requirements are enforced even though the transport
/// is then skipped: absolute http(s) only, no userinfo, no fragment,
/// https off-loopback, well-formed non-duplicated headers.
#[test]
fn a_remote_url_and_header_requirements() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let data = data_base(dir.path());
    for bad in [
        r#""r": {"type": "streamable-http", "url": "ftp://x.com/m"}"#,
        r#""r": {"type": "streamable-http", "url": "https://x.com/m#f"}"#,
        r#""r": {"type": "streamable-http", "url": "https://u@x.com/m"}"#,
        r#""r": {"type": "streamable-http", "url": "http://x.com/m"}"#,
        r#""r": {"type": "sse", "url": "https://x.com/s", "headers": {"A": "1", "a": "2"}}"#,
    ] {
        let out = parse_mcp_json(&mcp_doc(bad), &root, &data);
        assert!(
            out.reports[0].contains("invalid") && !out.reports[0].contains("unsupported"),
            "{bad} → {:?}",
            out.reports
        );
    }
    for good in [
        r#""r": {"type": "streamable-http", "url": "http://localhost:1234/m"}"#,
        r#""r": {"type": "streamable-http", "url": "http://127.0.0.1/m"}"#,
    ] {
        let out = parse_mcp_json(&mcp_doc(good), &root, &data);
        assert!(
            out.reports[0].contains("unsupported transport"),
            "{good} → {:?}",
            out.reports
        );
    }
}

// ──────────────────── Environment and expansion ────────────────────

/// §9.1: both reserved variables reach the subprocess env as absolute
/// paths, and discovery (through the real binary) creates the dedicated
/// `PLUGIN_DATA` directory, writable.
#[test]
fn a_plugin_root_and_data_are_provided() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(&dir.path().join("plug"), "acme");
    std::fs::write(
        root.join("mcp.json"),
        mcp_doc(r#""mem": {"type": "stdio", "command": "npx"}"#),
    )
    .unwrap();
    let loaded = load_plugin("acme", &root, &data_base(dir.path()));
    let server = &loaded.entry.unwrap().servers[0];
    let envs = envs_of(&to_server_config("acme", server));
    let get = |k: &str| {
        envs.iter()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{k} missing: {envs:?}"))
    };
    assert_eq!(get("PLUGIN_ROOT"), root.to_string_lossy());
    assert_eq!(
        get("PLUGIN_DATA"),
        data_base(dir.path()).join("acme").to_string_lossy()
    );
    assert!(Path::new(&get("PLUGIN_ROOT")).is_absolute());

    // Discovery-time creation, through the shipped binary: `hotl plugins
    // list` loads the registered plugin and must leave a writable data
    // dir behind (§9.1: created before any launch; hotl-mcp connects
    // lazily, so discovery is the creation point).
    let home = dir.path().join("home");
    let cfg_home = dir.path().join("xdg-config");
    let data_home = dir.path().join("xdg-data");
    std::fs::create_dir_all(cfg_home.join("hotl")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        cfg_home.join("hotl/config.toml"),
        format!("[plugins.sources]\nacme = '{}'\n", root.display()),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hotl"))
        .args(["plugins", "list"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &cfg_home)
        .env("XDG_DATA_HOME", &data_home)
        .output()
        .expect("hotl plugins list runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("acme:mem"), "{stdout}");
    let data = data_home.join("hotl/plugin-data/acme");
    assert!(data.is_dir(), "{stdout}");
    std::fs::write(data.join("probe"), b"w").expect("PLUGIN_DATA is writable");
}

/// §7.2.1: `command` is one executable token — a bare name passes
/// through whole (never shell-parsed), a `./` path resolves against the
/// root, and args travel separately.
#[test]
fn a_command_resolves_as_a_single_token() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let data = data_base(dir.path());
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "name with spaces", "args": ["--x y"]}"#),
        &root,
        &data,
    );
    let cfg = to_server_config("p", &out.servers[0]);
    let cmd = cfg.build_command();
    assert_eq!(
        cmd.as_std().get_program(),
        "name with spaces",
        "one token, no shell splitting"
    );
    assert_eq!(cmd.as_std().get_args().collect::<Vec<_>>(), ["--x y"]);

    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "./bin/server"}"#),
        &root,
        &data,
    );
    assert_eq!(
        out.servers[0].command,
        root.join("bin/server").to_string_lossy(),
        "plugin-relative resolves against the root"
    );
    for bad in ["../bin/server", "bin/server", "${PLUGIN_ROOT}/bin"] {
        let out = parse_mcp_json(
            &mcp_doc(&format!(r#""s": {{"type": "stdio", "command": "{bad}"}}"#)),
            &root,
            &data,
        );
        assert!(out.servers.is_empty(), "{bad}");
    }
}

/// §7.2.1: an omitted `cwd` means the plugin root, all the way into the
/// prepared command.
#[test]
fn a_cwd_defaults_to_plugin_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "npx"}"#),
        &root,
        &data_base(dir.path()),
    );
    let cfg = to_server_config("p", &out.servers[0]);
    assert_eq!(
        cfg.build_command().as_std().get_current_dir(),
        Some(root.as_path())
    );
}

/// §7.2.1: exactly three `cwd` forms, expanded before resolution, each
/// contained post-resolution.
#[test]
fn a_cwd_forms_and_containment() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let data = data_base(dir.path()).join("p");
    let base = data_base(dir.path());
    let case = |cwd: &str| {
        parse_mcp_json(
            &mcp_doc(&format!(
                r#""s": {{"type": "stdio", "command": "npx", "cwd": "{cwd}"}}"#
            )),
            &root,
            &base.join("p"),
        )
    };
    assert_eq!(case("./sub").servers[0].cwd, root.join("sub"));
    assert_eq!(case("${PLUGIN_ROOT}").servers[0].cwd, root);
    assert_eq!(
        case("${PLUGIN_DATA}/state").servers[0].cwd,
        data.join("state"),
        "a not-yet-created data target is valid"
    );
    for bad in ["sub", "../x", "./../x", "${PLUGIN_ROOT}/../x", "/abs"] {
        assert!(case(bad).servers.is_empty(), "{bad}");
    }
}

/// §9.1: configured entries overlay the client-selected base environment
/// — they are additions to it, not a replacement (the base is inherited,
/// so `get_envs` carries only the overlay).
#[test]
fn a_env_overlays_the_base_environment() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "npx", "env": {"MODE": "prod"}}"#),
        &root,
        &data_base(dir.path()),
    );
    let envs = envs_of(&to_server_config("p", &out.servers[0]));
    assert!(envs.contains(&("MODE".into(), "prod".into())), "{envs:?}");
    // Only the overlay is explicit; the base environment is not cleared.
    assert!(
        !envs.iter().any(|(k, _)| k == "HOME" || k == "PATH"),
        "the base env is inherited, not rebuilt: {envs:?}"
    );
}

/// §9.1/§9.2 — the test the plan marks never-skip: the reserved pair is
/// appended after every configured entry (plugins.rs) and the launcher
/// applies pairs last-wins (hotl-mcp), so client-set values always
/// replace equivalent names; a config that tries to set them itself is
/// invalid.
#[test]
fn a_reserved_vars_set_last_replace_configured() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let data = data_base(dir.path());
    // §9.2: naming a reserved variable invalidates the entry outright.
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "npx", "env": {"PLUGIN_ROOT": "/x"}}"#),
        &root,
        &data,
    );
    assert!(out.servers.is_empty());

    // The ordering mechanism itself, across both crates: the pair sits
    // last in the loaded server's env…
    let out = parse_mcp_json(
        &mcp_doc(r#""s": {"type": "stdio", "command": "npx", "env": {"A": "1", "Z": "2"}}"#),
        &root,
        &data,
    );
    let env = &out.servers[0].env;
    assert_eq!(env[env.len() - 2].0, "PLUGIN_ROOT");
    assert_eq!(env[env.len() - 1].0, "PLUGIN_DATA");
    // …and `build_command` gives the last duplicate the final word, so
    // appended-last means replaces-configured by construction.
    let cfg = ServerConfig {
        name: "s".into(),
        command: "npx".into(),
        args: vec![],
        description: String::new(),
        env: vec![
            ("PLUGIN_ROOT".into(), "configured".into()),
            ("PLUGIN_ROOT".into(), "client".into()),
        ],
        cwd: None,
    };
    let envs = envs_of(&cfg);
    assert_eq!(envs, vec![("PLUGIN_ROOT".into(), "client".into())]);
}

/// §7.2.1: a configured `PATH` env entry does not participate in
/// resolving a bare `command` — the program stays the bare token.
#[test]
fn a_configured_path_does_not_affect_bare_commands() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/npx"), "#!/bin/sh\n").unwrap();
    let out = parse_mcp_json(
        &mcp_doc(
            r#""s": {"type": "stdio", "command": "npx",
                    "env": {"PATH": "${PLUGIN_ROOT}/bin"}}"#,
        ),
        &root,
        &data_base(dir.path()),
    );
    assert_eq!(out.servers[0].command, "npx");
    let cfg = to_server_config("p", &out.servers[0]);
    assert_eq!(
        cfg.build_command().as_std().get_program(),
        "npx",
        "resolution is left to the platform at spawn, not the configured PATH"
    );
}

/// §9.2: exactly `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` expand, in
/// `args`, `env` values, and `cwd` — never in env keys or `command`, and
/// never any other placeholder or env-var syntax.
#[test]
fn a_expand_only_the_two_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let data = data_base(dir.path()).join("p");
    let out = parse_mcp_json(
        &mcp_doc(
            r#""s": {"type": "stdio", "command": "npx",
                    "args": ["${PLUGIN_ROOT}/a", "${HOME}", "$PLUGIN_ROOT", "${PLUGIN_ROOTX}"],
                    "env": {"CONF": "${PLUGIN_DATA}/c", "${STAYS}": "literal-key"},
                    "cwd": "${PLUGIN_ROOT}"}"#,
        ),
        &root,
        &data,
    );
    let s = &out.servers[0];
    assert_eq!(s.args[0], format!("{}/a", root.display()));
    assert_eq!(&s.args[1..], ["${HOME}", "$PLUGIN_ROOT", "${PLUGIN_ROOTX}"]);
    assert!(s
        .env
        .contains(&("CONF".to_string(), format!("{}/c", data.display()))));
    assert!(
        s.env.iter().any(|(k, _)| k == "${STAYS}"),
        "env keys are never expanded: {:?}",
        s.env
    );
    assert_eq!(s.cwd, root);
    // The pure seam agrees on non-recursion: replacement text is not
    // rescanned.
    assert_eq!(
        expand("${PLUGIN_DATA}", "/r", "/d/${PLUGIN_ROOT}"),
        "/d/${PLUGIN_ROOT}"
    );
}

// ───────────────────────── Resilience ─────────────────────────

/// §11.3 rule 1 / §7: component types outside the v1 format (commands,
/// hooks, agents) do not affect conformance and are ignored without
/// reports.
#[test]
fn a_unsupported_component_types_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    write_skill(&root.join("skills/real"));
    for extra in ["commands", "hooks", "agents"] {
        std::fs::create_dir_all(root.join(extra)).unwrap();
        std::fs::write(root.join(extra).join("thing.md"), "content").unwrap();
    }
    let loaded = load_plugin("p", &root, &data_base(dir.path()));
    let entry = loaded.entry.unwrap();
    assert_eq!(entry.skill_dirs.len(), 1);
    assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
}

/// §7.2.2 rule 4: an unsupported transport skips that server and only
/// that server.
#[test]
fn a_unsupported_transport_skips_only_that_server() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    let out = parse_mcp_json(
        &mcp_doc(
            r#""a-local": {"type": "stdio", "command": "npx"},
               "b-remote": {"type": "sse", "url": "https://x.com/sse"}"#,
        ),
        &root,
        &data_base(dir.path()),
    );
    assert_eq!(out.servers.len(), 1);
    assert_eq!(out.servers[0].name, "a-local");
    assert_eq!(out.reports.len(), 1);
}

/// §11.3 rule 3, the plan's own resilience fixture: one plugin carrying
/// a broken skill, a broken server, a valid skill, and a valid server —
/// both valid components load, and both failures are reported.
#[test]
fn a_component_failures_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    write_skill(&root.join("skills/good"));
    // Broken skill: SKILL.md resolves to a directory, not a regular file.
    std::fs::create_dir_all(root.join("skills/broken/SKILL.md")).unwrap();
    std::fs::write(
        root.join("mcp.json"),
        mcp_doc(
            r#""bad": {"type": "quantum", "endpoint": "??"},
               "good": {"type": "stdio", "command": "npx"}"#,
        ),
    )
    .unwrap();

    let loaded = load_plugin("p", &root, &data_base(dir.path()));
    let entry = loaded.entry.unwrap();
    assert_eq!(entry.skill_dirs, vec![root.join("skills/good")]);
    assert_eq!(entry.servers.len(), 1);
    assert_eq!(entry.servers[0].name, "good");
    assert_eq!(loaded.reports.len(), 2, "{:?}", loaded.reports);
    assert!(loaded.reports.iter().any(|r| r.contains("broken")));
    assert!(loaded.reports.iter().any(|r| r.contains("bad")));
}

/// §11.1 rule 8: at least one component type is supported — hotl
/// supports both, from one package.
#[test]
fn a_both_component_types_are_supported() {
    let dir = tempfile::tempdir().unwrap();
    let root = plugin_root(dir.path(), "p");
    write_skill(&root.join("skills/s"));
    std::fs::write(
        root.join("mcp.json"),
        mcp_doc(r#""m": {"type": "stdio", "command": "npx"}"#),
    )
    .unwrap();
    let entry = load_plugin("p", &root, &data_base(dir.path()))
        .entry
        .unwrap();
    assert!(!entry.skill_dirs.is_empty() && !entry.servers.is_empty());
}
