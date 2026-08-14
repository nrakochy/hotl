//! Agent Plugins 1.0.0 (spec: agentplugins/agent-plugins-spec) — the
//! portable plugin package: a directory with a `plugin.json` manifest,
//! skills under `skills/*/SKILL.md`, and stdio MCP servers via `mcp.json`.
//!
//! The spec's failure boundaries are precise and load-bearing here:
//! an unknown manifest field is *reported and ignored*; any other
//! `plugin.json` schema violation is *fatal to the plugin* (§5.2). That
//! split is why the manifest is hand-walked from `serde_json::Value`
//! rather than serde-derived — derive cannot report-and-continue on
//! unknown fields while failing on wrong-typed known ones.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The canonical 1.0.0 manifest schema identifier (§5.2). Exact match:
/// a near-miss (`http:`, trailing space) is an unsupported version, and
/// clients MUST NOT retrieve schemas while loading.
pub const MANIFEST_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

/// The manifest fields hotl carries forward. Everything else is
/// validated (§5.2–§5.4) and dropped.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// The plugin identity (§5.5 grammar): skill qualifier, MCP server
    /// prefix, and PLUGIN_DATA key. The config handle is only where the
    /// checkout lives.
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
}

/// §5.5. The character set is lowercase-only, so "alphanumeric first and
/// last" reduces to `a-z0-9` at both ends.
pub fn validate_plugin_name(name: &str) -> Result<(), String> {
    let ok_ends = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let valid = (1..=64).contains(&name.len())
        && name.chars().all(|c| ok_ends(c) || matches!(c, '-' | '.'))
        && name.chars().next().is_some_and(ok_ends)
        && name.chars().last().is_some_and(ok_ends)
        && !name.contains("--")
        && !name.contains("..");
    if valid {
        Ok(())
    } else {
        Err(format!(
            "`{name}` is not a valid plugin name (1-64 chars of a-z 0-9 `.` `-`, \
             alphanumeric first and last, no `--` or `..`)"
        ))
    }
}

/// Parse and validate `plugin.json` (§5). `Ok` carries the manifest plus
/// the non-fatal reports (unknown top-level fields, a non-object
/// `extensions`); `Err` is fatal — the plugin is rejected and none of its
/// components may be discovered (§5.2, §11.3).
pub fn parse_manifest(text: &str) -> Result<(Manifest, Vec<String>), String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("plugin.json is not valid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err("plugin.json must contain a top-level JSON object".into());
    };
    let mut reports = Vec::new();

    // The closed schema (§5.2). Unknown fields are the one non-fatal
    // violation besides a non-object `extensions`.
    const KNOWN: [&str; 10] = [
        "$schema",
        "name",
        "version",
        "description",
        "author",
        "homepage",
        "repository",
        "license",
        "keywords",
        "extensions",
    ];
    for key in obj.keys() {
        if !KNOWN.contains(&key.as_str()) {
            reports.push(format!("plugin.json: unknown field `{key}` ignored"));
        }
    }

    // Required fields (§5.3). `$schema` selects the validation rules; an
    // unrecognized identifier is an unsupported version, not a typo to
    // forgive (§5.2).
    let schema = match obj.get("$schema") {
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err("plugin.json: `$schema` must be a string".into()),
        None => return Err("plugin.json: required field `$schema` is missing".into()),
    };
    if schema != MANIFEST_SCHEMA {
        return Err(format!(
            "plugin.json targets an unsupported Agent Plugins version \
             (`$schema` is `{schema}`; hotl supports `{MANIFEST_SCHEMA}`)"
        ));
    }
    let name = match obj.get("name") {
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err("plugin.json: `name` must be a string".into()),
        None => return Err("plugin.json: required field `name` is missing".into()),
    };
    validate_plugin_name(name).map_err(|e| format!("plugin.json: {e}"))?;

    // Metadata (§5.4): validated by JSON type only — `version` need not be
    // semver, `homepage` need not be a URL.
    for key in [
        "version",
        "description",
        "homepage",
        "repository",
        "license",
    ] {
        if obj.get(key).is_some_and(|v| !v.is_string()) {
            return Err(format!("plugin.json: `{key}` must be a string"));
        }
    }
    if let Some(v) = obj.get("author") {
        let Some(author) = v.as_object() else {
            return Err("plugin.json: `author` must be an object".into());
        };
        for (key, val) in author {
            if !matches!(key.as_str(), "name" | "email" | "url") {
                return Err(format!(
                    "plugin.json: `author` may only contain `name`, `email`, and \
                     `url` (found `{key}`)"
                ));
            }
            if !val.is_string() {
                return Err(format!("plugin.json: `author.{key}` must be a string"));
            }
        }
    }
    if let Some(v) = obj.get("keywords") {
        let ok = v
            .as_array()
            .is_some_and(|arr| arr.iter().all(Value::is_string));
        if !ok {
            return Err("plugin.json: `keywords` must be an array of strings".into());
        }
    }

    // `extensions` (§8.1): a non-object field is reported and ignored; an
    // object's member values must themselves be objects, but their
    // *contents* are never validated — hotl implements no namespace.
    match obj.get("extensions") {
        None => {}
        Some(Value::Object(map)) => {
            for (ns, val) in map {
                if !val.is_object() {
                    return Err(format!("plugin.json: `extensions.{ns}` must be an object"));
                }
            }
        }
        Some(_) => reports.push("plugin.json: `extensions` is not an object — ignored".into()),
    }

    let get_str = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_string);
    Ok((
        Manifest {
            name: name.to_string(),
            version: get_str("version"),
            description: get_str("description"),
        },
        reports,
    ))
}

/// One loaded (or rejected) plugin. `entry: None` means the plugin was
/// rejected outright — fatal manifest, escaping `plugin.json`, unreadable
/// root; the reports say why. Component-level failures leave the entry in
/// place with the failing component skipped (§11.3).
#[derive(Debug)]
pub struct LoadedPlugin {
    pub entry: Option<PluginEntry>,
    pub reports: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PluginEntry {
    /// The manifest `name` — the plugin identity (skill qualifier, MCP
    /// prefix, PLUGIN_DATA key).
    pub name: String,
    /// The `[plugins.sources]` key — only where the checkout lives.
    pub handle: String,
    /// The filesystem-resolved plugin root (§4.1) — the `PLUGIN_ROOT`
    /// value. Every containment check below compares against this.
    pub root: PathBuf,
    pub manifest: Manifest,
    /// `skills/<name>` dirs whose `SKILL.md` resolved to a regular file
    /// inside the root — immediate children only (§7.1), sorted.
    pub skill_dirs: Vec<PathBuf>,
    /// The per-plugin `PLUGIN_DATA` directory (`<data_base>/<name>`).
    /// Not created here — the config layer creates it at discovery for
    /// plugins with at least one valid stdio server (§9.1).
    pub plugin_data: PathBuf,
    /// Valid stdio servers from `mcp.json`, post-validation and
    /// post-expansion, the reserved env pair already appended last.
    pub servers: Vec<PluginServer>,
}

/// Load one plugin from a directory (§11.1 rule 1). `data_base` is the
/// parent of every per-plugin `PLUGIN_DATA` dir; the plugin's own is
/// `<data_base>/<manifest-name>`. Containment is symlink-resolving
/// canonicalize + prefix — deliberately not `fsguard::resolve_beneath`,
/// whose no-follow descent rejects the in-root symlinks §4.1 explicitly
/// permits.
pub fn load_plugin(handle: &str, root: &Path, data_base: &Path) -> LoadedPlugin {
    let mut reports = Vec::new();
    let Ok(root) = dunce::canonicalize(root) else {
        return LoadedPlugin {
            entry: None,
            reports: vec![format!(
                "plugin `{handle}`: {} is not a readable directory",
                root.display()
            )],
        };
    };

    // §4.1 boundary 1: an escaping plugin.json rejects the whole plugin.
    let manifest_path = root.join("plugin.json");
    let contained = dunce::canonicalize(&manifest_path)
        .ok()
        .filter(|p| p.starts_with(&root) && p.is_file());
    if contained.is_none() {
        return LoadedPlugin {
            entry: None,
            reports: vec![format!(
                "plugin `{handle}`: plugin.json is missing or does not resolve \
                 to a regular file inside the plugin root"
            )],
        };
    }
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(e) => {
            return LoadedPlugin {
                entry: None,
                reports: vec![format!("plugin `{handle}`: plugin.json unreadable: {e}")],
            }
        }
    };
    // A fatal manifest rejects the plugin: no component of it may be
    // discovered or executed (§5.2), so this returns before any scan.
    let (manifest, manifest_reports) = match parse_manifest(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            return LoadedPlugin {
                entry: None,
                reports: vec![format!("plugin `{handle}`: {e}")],
            }
        }
    };
    reports.extend(
        manifest_reports
            .into_iter()
            .map(|r| format!("plugin `{handle}`: {r}")),
    );
    if manifest.name != handle {
        reports.push(format!(
            "plugin `{handle}`: manifest name is `{}` — the manifest name is \
             the plugin's identity; the config key is only the install handle",
            manifest.name
        ));
    }

    let skill_dirs = discover_skills(handle, &root, &mut reports);
    let plugin_data = data_base.join(&manifest.name);
    let servers = discover_mcp(handle, &root, &plugin_data, &mut reports);
    LoadedPlugin {
        entry: Some(PluginEntry {
            name: manifest.name.clone(),
            handle: handle.to_string(),
            root,
            manifest,
            skill_dirs,
            plugin_data,
            servers,
        }),
        reports,
    }
}

/// The `skills/` fixed location (§6.1, §7.1): immediate child directories
/// whose `SKILL.md` resolves to a regular file inside the plugin root.
/// Absent is not an error (§6.2); present-but-not-a-directory (or
/// resolving outside the root, §4.1 boundary 2) invalidates the skills
/// component; an escaping `SKILL.md` skips that one skill (§4.1
/// boundary 3) while its siblings load.
fn discover_skills(handle: &str, root: &Path, reports: &mut Vec<String>) -> Vec<PathBuf> {
    let location = root.join("skills");
    // `symlink_metadata` so a dangling symlink counts as present (and then
    // fails resolution below) rather than as the benign missing case.
    if std::fs::symlink_metadata(&location).is_err() {
        return Vec::new();
    }
    let resolved = dunce::canonicalize(&location)
        .ok()
        .filter(|p| p.starts_with(root) && p.is_dir());
    if resolved.is_none() {
        reports.push(format!(
            "plugin `{handle}`: `skills` does not resolve to a directory inside \
             the plugin root — skills component skipped"
        ));
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(&location) else {
        reports.push(format!(
            "plugin `{handle}`: `skills` is not readable — skills component skipped"
        ));
        return Vec::new();
    };
    let mut children: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
    children.sort();
    let mut out = Vec::new();
    for child in children {
        // Immediate children only, and only directories (a loose
        // `skills/x.md` is not a skill). `is_dir` follows symlinks: an
        // in-root symlinked dir is permitted, and an escaping one is
        // caught by the SKILL.md resolution check next.
        if !child.is_dir() {
            continue;
        }
        let skill_md = child.join("SKILL.md");
        match dunce::canonicalize(&skill_md) {
            Ok(p) if p.starts_with(root) && p.is_file() => out.push(child),
            // No SKILL.md at all: just not a skill dir.
            Err(_) => {}
            Ok(_) => {
                let name = child.file_name().unwrap_or_default().to_string_lossy();
                reports.push(format!(
                    "plugin `{handle}`: skill `{name}` skipped — its SKILL.md \
                     resolves outside the plugin root"
                ));
            }
        }
    }
    out
}

/// The canonical 1.0.0 MCP configuration schema identifier (§7.2.1).
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

/// One valid stdio server from a plugin's `mcp.json`, after validation,
/// containment, and placeholder expansion. `env` carries the configured
/// pairs in order with the reserved (`PLUGIN_ROOT`, `PLUGIN_DATA`) pair
/// appended **last** — §9.1's overlay order holds by construction when a
/// launcher applies pairs first-to-last with last-wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginServer {
    /// The `mcpServers` member name, unqualified — the config layer
    /// composes `<plugin>:<server>`.
    pub name: String,
    /// Bare executable name, or the absolute root-joined path of a `./`
    /// plugin-relative command.
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Always set: the validated `cwd`, defaulting to the plugin root
    /// (§7.2.1).
    pub cwd: PathBuf,
    pub description: String,
}

/// The result of validating one `mcp.json` (§7.2.2). `disabled` is rule
/// 2's boundary: the whole MCP component is off for this plugin (bad
/// JSON, wrong/mismatched `$schema`, extra top-level fields), while other
/// component types keep loading.
#[derive(Debug)]
pub struct McpOutcome {
    pub servers: Vec<PluginServer>,
    pub reports: Vec<String>,
    pub disabled: bool,
}

/// §9.2: single, non-recursive textual replacement of every exact
/// `${PLUGIN_ROOT}` / `${PLUGIN_DATA}` occurrence. Only the original
/// string is scanned, so replacement text is never re-expanded, and
/// unrecognized placeholder-like text (`${HOME}`, `$PLUGIN_ROOT`,
/// `${PLUGIN_ROOTX}`) stays literal.
pub fn expand(s: &str, root: &str, data: &str) -> String {
    const ROOT_VAR: &str = "${PLUGIN_ROOT}";
    const DATA_VAR: &str = "${PLUGIN_DATA}";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let hit = match (rest.find(ROOT_VAR), rest.find(DATA_VAR)) {
            (None, None) => break,
            (Some(i), None) => (i, root),
            (None, Some(j)) => (j, data),
            (Some(i), Some(j)) => {
                if i < j {
                    (i, root)
                } else {
                    (j, data)
                }
            }
        };
        out.push_str(&rest[..hit.0]);
        out.push_str(hit.1);
        rest = &rest[hit.0 + ROOT_VAR.len()..];
    }
    out.push_str(rest);
    out
}

/// Parse and validate one `mcp.json` against the plugin root and its
/// (possibly not-yet-created) `PLUGIN_DATA` dir. `root` must be the
/// filesystem-resolved plugin root.
pub fn parse_mcp_json(text: &str, root: &Path, data: &Path) -> McpOutcome {
    let off = |report: String| McpOutcome {
        servers: Vec::new(),
        reports: vec![format!("{report} — MCP disabled for this plugin")],
        disabled: true,
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return off(format!("mcp.json is not valid JSON ({e})")),
    };
    let Some(obj) = value.as_object() else {
        return off("mcp.json must contain a top-level JSON object".into());
    };
    // Exactly `$schema` + `mcpServers`, nothing else (§7.2.1).
    for key in obj.keys() {
        if key != "$schema" && key != "mcpServers" {
            return off(format!("mcp.json: unexpected top-level field `{key}`"));
        }
    }
    let schema = match obj.get("$schema") {
        Some(Value::String(s)) => s.as_str(),
        _ => return off("mcp.json: required field `$schema` is missing or not a string".into()),
    };
    // §10.1: compare declared *versions*, not raw strings — a recognizable
    // identifier at another version reports as a mismatch with the
    // manifest (always 1.0.0 here, the one version hotl supports), while
    // an unrecognizable one is simply invalid.
    let version = schema
        .strip_prefix("https://agent-plugins.org/schemas/")
        .and_then(|s| s.strip_suffix("/mcp.schema.json"));
    match version {
        Some("1.0.0") => {}
        Some(v) => {
            return off(format!(
                "mcp.json targets Agent Plugins {v} while plugin.json targets 1.0.0"
            ))
        }
        None => return off(format!("mcp.json: `$schema` `{schema}` is not recognized")),
    }
    let Some(servers_obj) = obj.get("mcpServers").and_then(Value::as_object) else {
        return off("mcp.json: required field `mcpServers` is missing or not an object".into());
    };

    let mut servers = Vec::new();
    let mut reports = Vec::new();
    for (name, entry) in servers_obj {
        // Client-native constraint: the name becomes half of hotl's
        // `<plugin>:<server>` roster key, so it gets the same charset the
        // `[[mcp]]` lane enforces at `hotl mcp add`.
        if !valid_server_name(name) {
            reports.push(format!(
                "server `{name}` entry invalid — server names are letters, digits, \
                 `.`/`_`/`-` with an alphanumeric first char, ≤ 64 chars; entry skipped"
            ));
            continue;
        }
        match validate_server(entry, root, data) {
            Ok(ServerEntry::Stdio(mut server)) => {
                server.name = name.clone();
                servers.push(server);
            }
            // §7.2.2 rule 4 — a *valid* remote entry, distinct wording
            // from the invalid case so Appendix A's URL/header rows stay
            // honest when a transport plan lands later.
            Ok(ServerEntry::Unsupported(transport)) => reports.push(format!(
                "server `{name}` skipped — unsupported transport `{transport}` \
                 (hotl connects over stdio only)"
            )),
            Err(e) => reports.push(format!(
                "server `{name}` entry invalid — {e}; entry skipped"
            )),
        }
    }
    McpOutcome {
        servers,
        reports,
        disabled: false,
    }
}

enum ServerEntry {
    Stdio(PluginServer),
    /// A fully valid entry whose declared transport hotl does not
    /// support; carries the transport name for the report.
    Unsupported(&'static str),
}

/// One server configuration object: a closed union over `type` (§7.2.1).
/// An unknown field, unknown `type`, or cross-variant field is invalid.
fn validate_server(entry: &Value, root: &Path, data: &Path) -> Result<ServerEntry, String> {
    let Some(obj) = entry.as_object() else {
        return Err("a server entry must be a JSON object".into());
    };
    let ty = match obj.get("type") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err("`type` is required and must be a string".into()),
    };
    match ty {
        "stdio" => validate_stdio(obj, root, data).map(ServerEntry::Stdio),
        "streamable-http" => {
            validate_remote(obj).map(|()| ServerEntry::Unsupported("streamable-http"))
        }
        "sse" => validate_remote(obj).map(|()| ServerEntry::Unsupported("sse")),
        other => Err(format!("unknown `type` `{other}`")),
    }
}

fn validate_stdio(
    obj: &serde_json::Map<String, Value>,
    root: &Path,
    data: &Path,
) -> Result<PluginServer, String> {
    for key in obj.keys() {
        if !matches!(key.as_str(), "type" | "command" | "args" | "env" | "cwd") {
            return Err(format!("unknown field `{key}` on a stdio entry"));
        }
    }
    let command = match obj.get("command") {
        Some(Value::String(s)) => validate_command(s, root)?,
        _ => return Err("`command` is required and must be a string".into()),
    };
    let root_str = root.to_string_lossy();
    let data_str = data.to_string_lossy();
    let mut args = Vec::new();
    if let Some(v) = obj.get("args") {
        let Some(arr) = v.as_array() else {
            return Err("`args` must be an array of strings".into());
        };
        for a in arr {
            let Some(a) = a.as_str() else {
                return Err("`args` must be an array of strings".into());
            };
            args.push(expand(a, &root_str, &data_str));
        }
    }
    let mut env = Vec::new();
    if let Some(v) = obj.get("env") {
        let Some(map) = v.as_object() else {
            return Err("`env` must be an object of strings".into());
        };
        for (k, val) in map {
            let Some(val) = val.as_str() else {
                return Err(format!("`env.{k}` must be a string"));
            };
            // §9.2: the reserved names are the client's to set.
            if k == "PLUGIN_ROOT" || k == "PLUGIN_DATA" {
                return Err(format!("`env` must not define the reserved `{k}`"));
            }
            env.push((k.clone(), expand(val, &root_str, &data_str)));
        }
    }
    // The reserved pair, last: applied first-to-last with last-wins,
    // these replace any equivalently-named configured entry (§9.1).
    env.push(("PLUGIN_ROOT".into(), root_str.into_owned()));
    env.push(("PLUGIN_DATA".into(), data_str.into_owned()));
    let cwd = match obj.get("cwd") {
        None => root.to_path_buf(),
        Some(Value::String(s)) => validate_cwd(s, root, data)?,
        Some(_) => return Err("`cwd` must be a string".into()),
    };
    Ok(PluginServer {
        name: String::new(),
        command,
        args,
        env,
        cwd,
        description: String::new(),
    })
}

/// §7.2.1: a single executable token — a bare name (platform search
/// rules) or a `./` plugin-relative path resolved against the root. No
/// placeholder expansion, ever.
fn validate_command(raw: &str, root: &Path) -> Result<String, String> {
    if let Some(rest) = raw.strip_prefix("./") {
        let path = root.join(rest);
        contain(&path, root, "the plugin root").map_err(|e| format!("`command` {e}"))?;
        return Ok(path.to_string_lossy().into_owned());
    }
    if raw.is_empty() || raw.contains(['/', '\\']) || raw.contains("${") {
        return Err(format!(
            "`command` must be a bare executable name or a `./` plugin-relative \
             path (got `{raw}`; placeholders do not expand in `command`)"
        ));
    }
    Ok(raw.to_string())
}

/// §7.2.1's three explicit `cwd` forms, expanded before resolution, each
/// contained in its declared base. The target may not exist yet (a
/// `${PLUGIN_DATA}/sub` is created by the server itself), so containment
/// is lexical `..`-rejection plus canonicalize-when-exists.
fn validate_cwd(raw: &str, root: &Path, data: &Path) -> Result<PathBuf, String> {
    let expanded = expand(raw, &root.to_string_lossy(), &data.to_string_lossy());
    let (path, base, what) = if raw == "${PLUGIN_ROOT}" {
        (root.to_path_buf(), root, "the plugin root")
    } else if raw == "${PLUGIN_DATA}" {
        (data.to_path_buf(), data, "the plugin data directory")
    } else if raw.starts_with("./") {
        (root.join(&expanded[2..]), root, "the plugin root")
    } else if raw.starts_with("${PLUGIN_ROOT}/") {
        (PathBuf::from(&expanded), root, "the plugin root")
    } else if raw.starts_with("${PLUGIN_DATA}/") {
        (PathBuf::from(&expanded), data, "the plugin data directory")
    } else {
        return Err(format!(
            "`cwd` must be a `./` plugin-relative path, `${{PLUGIN_ROOT}}`[/…], \
             or `${{PLUGIN_DATA}}`[/…] (got `{raw}`)"
        ));
    };
    contain(&path, base, what).map_err(|e| format!("`cwd` {e}"))?;
    Ok(path)
}

/// Lexical containment (`..`-free and prefix-anchored) plus a
/// canonicalize-when-exists symlink check. The lexical prefix check is
/// load-bearing: an expanded absolute path smuggled into a `./` value
/// would otherwise replace the base in `Path::join`.
fn contain(path: &Path, base: &Path, what: &str) -> Result<(), String> {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) || !path.starts_with(base) {
        return Err(format!("escapes {what}"));
    }
    if let Ok(resolved) = dunce::canonicalize(path) {
        let base = dunce::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
        if !resolved.starts_with(&base) {
            return Err(format!("resolves outside {what}"));
        }
    }
    Ok(())
}

/// The `hotl mcp add` charset, so a plugin server name can always live in
/// the shared roster.
fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// An HTTP field-name token (RFC 9110 tchar).
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A remote (`streamable-http` / `sse`) entry: fully validated (§7.2.1's
/// URL and header requirements) even though hotl then skips it — so a
/// future transport plan changes one match arm, not the validation.
fn validate_remote(obj: &serde_json::Map<String, Value>) -> Result<(), String> {
    for key in obj.keys() {
        if !matches!(key.as_str(), "type" | "url" | "headers") {
            return Err(format!("unknown field `{key}` on a remote entry"));
        }
    }
    let url = match obj.get("url") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err("`url` is required and must be a string".into()),
    };
    validate_remote_url(url)?;
    if let Some(v) = obj.get("headers") {
        let Some(map) = v.as_object() else {
            return Err("`headers` must be an object of strings".into());
        };
        let mut seen: Vec<String> = Vec::new();
        for (name, val) in map {
            if !valid_header_name(name) {
                return Err(format!("`{name}` is not a valid HTTP header name"));
            }
            let folded = name.to_ascii_lowercase();
            if seen.contains(&folded) {
                return Err(format!(
                    "`headers` contains `{name}` more than once under different casing"
                ));
            }
            seen.push(folded);
            let ok = val
                .as_str()
                .is_some_and(|v| v.bytes().all(|b| b == b'\t' || (0x20..0x7f).contains(&b)));
            if !ok {
                return Err(format!("`headers.{name}` is not a valid HTTP header value"));
            }
        }
    }
    Ok(())
}

/// §7.2.1: absolute http(s), no user information, no fragment; plain
/// `http` only for `localhost` or a loopback IP literal.
fn validate_remote_url(url: &str) -> Result<(), String> {
    let (plain_http, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (false, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (true, rest)
    } else {
        return Err("`url` must be an absolute http(s) URL".into());
    };
    if url.contains('#') {
        return Err("`url` must not contain a fragment".into());
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("`url` has no host".into());
    }
    if authority.contains('@') {
        return Err("`url` must not contain user information".into());
    }
    if plain_http {
        let host = if let Some(v6) = authority.strip_prefix('[') {
            v6.split(']').next().unwrap_or("")
        } else {
            authority.rsplit_once(':').map_or(authority, |(h, _)| h)
        };
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err("`url` must use https for non-loopback endpoints".into());
        }
    }
    Ok(())
}

/// The `mcp.json` fixed location (§6.1): absent is fine (§6.2); an
/// escaping or wrong-kind location invalidates the MCP component (§4.1
/// boundary 2); everything else is `parse_mcp_json`'s problem.
fn discover_mcp(
    handle: &str,
    root: &Path,
    plugin_data: &Path,
    reports: &mut Vec<String>,
) -> Vec<PluginServer> {
    let location = root.join("mcp.json");
    if std::fs::symlink_metadata(&location).is_err() {
        return Vec::new();
    }
    let resolved = dunce::canonicalize(&location)
        .ok()
        .filter(|p| p.starts_with(root) && p.is_file());
    if resolved.is_none() {
        reports.push(format!(
            "plugin `{handle}`: mcp.json does not resolve to a regular file inside \
             the plugin root — MCP component skipped"
        ));
        return Vec::new();
    }
    let text = match std::fs::read_to_string(&location) {
        Ok(text) => text,
        Err(e) => {
            reports.push(format!(
                "plugin `{handle}`: mcp.json unreadable ({e}) — MCP component skipped"
            ));
            return Vec::new();
        }
    };
    let outcome = parse_mcp_json(&text, root, plugin_data);
    reports.extend(
        outcome
            .reports
            .into_iter()
            .map(|r| format!("plugin `{handle}`: {r}")),
    );
    outcome.servers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> (Manifest, Vec<String>) {
        parse_manifest(text).expect("manifest must parse")
    }

    fn fatal(text: &str) -> String {
        parse_manifest(text).expect_err("manifest must be rejected")
    }

    #[test]
    fn a_minimal_manifest_parses_with_zero_reports() {
        let (m, reports) = ok(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "minimal-plugin"}}"#
        ));
        assert_eq!(m.name, "minimal-plugin");
        assert_eq!(m.version, None);
        assert!(reports.is_empty(), "{reports:?}");
    }

    #[test]
    fn the_specs_full_manifest_example_parses() {
        let (m, reports) = ok(&format!(
            r#"{{
              "$schema": "{MANIFEST_SCHEMA}",
              "name": "plugin-name",
              "version": "1.2.0",
              "description": "Brief plugin description",
              "author": {{"name": "Author Name", "email": "author@example.com",
                          "url": "https://example.com"}},
              "homepage": "https://docs.example.com/plugin",
              "repository": "https://github.com/example/plugin",
              "license": "MIT",
              "keywords": ["keyword1", "keyword2"],
              "extensions": {{"com.example.client": {{"setting": true}}}}
            }}"#
        ));
        assert_eq!(m.name, "plugin-name");
        assert_eq!(m.version.as_deref(), Some("1.2.0"));
        assert_eq!(m.description.as_deref(), Some("Brief plugin description"));
        assert!(reports.is_empty(), "{reports:?}");
    }

    /// §5.2: an unknown top-level field is the non-fatal violation — the
    /// plugin still loads, and the report names the field.
    #[test]
    fn an_unknown_top_level_field_is_reported_and_ignored() {
        let (m, reports) = ok(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a", "commands": ["x"]}}"#
        ));
        assert_eq!(m.name, "a");
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(reports[0].contains("`commands`"), "{}", reports[0]);
    }

    /// §5.2/§5.3: everything that is not an unknown field or a non-object
    /// `extensions` is fatal.
    #[test]
    fn wrong_typed_known_fields_are_fatal() {
        for (json, needle) in [
            (
                format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a", "keywords": "x"}}"#),
                "keywords",
            ),
            (
                format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": 42}}"#),
                "name",
            ),
            (
                format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": ""}}"#),
                "name",
            ),
            (
                format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "version": 2, "name": "a"}}"#),
                "version",
            ),
            (r#"{"name": "a"}"#.to_string(), "$schema"),
            (format!(r#"{{"$schema": {MANIFEST_SCHEMA:?}}}"#), "name"),
            ("[]".to_string(), "object"),
            ("not json".to_string(), "JSON"),
        ] {
            let err = fatal(&json);
            assert!(err.contains(needle), "`{json}` → {err}");
        }
    }

    /// §5.2: `$schema` is an exact identifier match — a near miss is an
    /// unsupported version, never fuzzy-matched (and never fetched).
    #[test]
    fn a_near_miss_schema_is_an_unsupported_version() {
        for schema in [
            "http://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
            "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json ",
            "https://agent-plugins.org/schemas/2.0.0/plugin.schema.json",
            "",
        ] {
            let err = fatal(&format!(r#"{{"$schema": {schema:?}, "name": "a"}}"#));
            assert!(err.contains("unsupported"), "{schema:?} → {err}");
        }
    }

    /// §5.5's own examples, verbatim.
    #[test]
    fn the_name_grammar_matches_the_spec_table() {
        for good in ["my-plugin", "acme.tools", "lint3r", "a"] {
            assert!(validate_plugin_name(good).is_ok(), "{good}");
        }
        for bad in [
            "My-Plugin",      // uppercase
            "-start",         // leading hyphen
            "has--double",    // consecutive hyphens
            "too.many..dots", // consecutive periods
            "",               // empty
            "ends.",          // non-alphanumeric last
            "a_b",            // underscore not in the set
        ] {
            assert!(validate_plugin_name(bad).is_err(), "{bad}");
        }
        assert!(validate_plugin_name(&"a".repeat(64)).is_ok());
        assert!(validate_plugin_name(&"a".repeat(65)).is_err());
    }

    /// §5.4: the author object is closed — `name`/`email`/`url` only, all
    /// strings.
    #[test]
    fn the_author_object_is_closed() {
        let err = fatal(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a",
                "author": {{"name": "x", "twitter": "@x"}}}}"#
        ));
        assert!(err.contains("twitter"), "{err}");
        let err = fatal(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a", "author": "someone"}}"#
        ));
        assert!(err.contains("author"), "{err}");
        let err = fatal(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a", "author": {{"url": 1}}}}"#
        ));
        assert!(err.contains("author.url"), "{err}");
    }

    /// §5.4: metadata is validated by JSON type only — `version` need not
    /// be semver, URLs and SPDX ids are not checked.
    #[test]
    fn metadata_is_not_semantically_validated() {
        let (m, reports) = ok(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a",
                "version": "not-semver", "homepage": "not a url",
                "license": "not-spdx"}}"#
        ));
        assert_eq!(m.version.as_deref(), Some("not-semver"));
        assert!(reports.is_empty(), "{reports:?}");
    }

    /// §8.1: a non-object `extensions` is the second non-fatal violation.
    #[test]
    fn a_non_object_extensions_is_reported_and_ignored() {
        let (m, reports) = ok(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a", "extensions": []}}"#
        ));
        assert_eq!(m.name, "a");
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(reports[0].contains("extensions"), "{}", reports[0]);
    }

    /// Root with a valid `plugin.json` for `name`, returned canonicalized
    /// (macOS tempdirs live under the `/private` symlink).
    fn plugin_root(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "{name}"}}"#),
        )
        .unwrap();
        dunce::canonicalize(dir).unwrap()
    }

    fn write_skill(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: s\ndescription: d\n---\nbody\n",
        )
        .unwrap();
    }

    fn data_dir() -> PathBuf {
        PathBuf::from("/nonexistent-plugin-data")
    }

    /// §6.2: a manifest-only plugin is complete — missing fixed locations
    /// are not errors and produce no reports.
    #[test]
    fn a_manifest_only_plugin_loads_with_zero_reports() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(dir.path(), "solo");
        let loaded = load_plugin("solo", &root, &data_dir());
        let entry = loaded.entry.expect("must load");
        assert_eq!(entry.name, "solo");
        assert_eq!(entry.root, root);
        assert!(entry.skill_dirs.is_empty());
        assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
    }

    /// §7.1: immediate children of `skills/` only — no recursive descent,
    /// and a loose `skills/x.md` is not a skill.
    #[test]
    fn skills_discovery_is_immediate_children_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(dir.path(), "p");
        write_skill(&root.join("skills/real"));
        write_skill(&root.join("skills/deep/nested"));
        std::fs::write(root.join("skills/loose.md"), "not a skill\n").unwrap();

        let loaded = load_plugin("p", &root, &data_dir());
        let entry = loaded.entry.expect("must load");
        assert_eq!(entry.skill_dirs, vec![root.join("skills/real")]);
        assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
    }

    /// §4.1 boundary 3: an escaping `SKILL.md` skips that skill, with a
    /// report, while its sibling loads. An in-root symlink is permitted.
    #[cfg(unix)]
    #[test]
    fn an_escaping_skill_is_skipped_while_siblings_and_in_root_links_load() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("SKILL.md"), "---\nname: evil\n---\nx\n").unwrap();
        let root = plugin_root(&dir.path().join("plugin"), "p");
        write_skill(&root.join("skills/good"));
        // Escaping: the skill dir is a symlink out of the root.
        std::os::unix::fs::symlink(&outside, root.join("skills/evil")).unwrap();
        // In-root: a symlinked dir whose target stays inside the root.
        write_skill(&root.join("bundled"));
        std::os::unix::fs::symlink(root.join("bundled"), root.join("skills/alias")).unwrap();

        let loaded = load_plugin("p", &root, &data_dir());
        let entry = loaded.entry.expect("plugin itself still loads");
        assert_eq!(
            entry.skill_dirs,
            vec![root.join("skills/alias"), root.join("skills/good")],
            "sibling and in-root symlink load; the escape does not"
        );
        assert_eq!(loaded.reports.len(), 1, "{:?}", loaded.reports);
        assert!(loaded.reports[0].contains("evil"), "{}", loaded.reports[0]);
    }

    /// §4.1 boundary 1: an escaping `plugin.json` rejects the plugin.
    #[cfg(unix)]
    #[test]
    fn an_escaping_plugin_json_rejects_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("manifest.json");
        std::fs::write(
            &outside,
            format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "evil"}}"#),
        )
        .unwrap();
        let root = dir.path().join("plugin");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("plugin.json")).unwrap();
        write_skill(&root.join("skills/s"));

        let loaded = load_plugin("p", &root, &data_dir());
        assert!(loaded.entry.is_none(), "rejected, components undiscovered");
        assert!(
            loaded.reports[0].contains("plugin.json"),
            "{:?}",
            loaded.reports
        );
    }

    /// §6.2: a fixed location of the wrong filesystem kind invalidates
    /// that component type only.
    #[test]
    fn skills_as_a_regular_file_invalidates_only_that_component() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(dir.path(), "p");
        std::fs::write(root.join("skills"), "not a directory\n").unwrap();

        let loaded = load_plugin("p", &root, &data_dir());
        let entry = loaded.entry.expect("the plugin still loads");
        assert!(entry.skill_dirs.is_empty());
        assert_eq!(loaded.reports.len(), 1, "{:?}", loaded.reports);
        assert!(
            loaded.reports[0].contains("skills"),
            "{}",
            loaded.reports[0]
        );
    }

    /// §5.2: a fatal manifest rejects the plugin — its valid `skills/` is
    /// never discovered.
    #[test]
    fn a_fatal_manifest_prevents_all_component_discovery() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.json"),
            format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "NOT-VALID"}}"#),
        )
        .unwrap();
        write_skill(&dir.path().join("skills/s"));

        let loaded = load_plugin("p", dir.path(), &data_dir());
        assert!(loaded.entry.is_none());
        assert!(
            loaded.reports[0].contains("plugin name"),
            "{:?}",
            loaded.reports
        );
    }

    /// A missing manifest or an unreadable root is a rejection with a
    /// legible report, and a handle≠name mismatch is reported (decision 4:
    /// the manifest name is the identity, the key only the handle).
    #[test]
    fn missing_manifest_missing_root_and_handle_mismatch_report() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_plugin("p", &dir.path().join("nope"), &data_dir());
        assert!(loaded.entry.is_none());
        assert!(loaded.reports[0].contains("not a readable directory"));

        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        let loaded = load_plugin("p", &dir.path().join("empty"), &data_dir());
        assert!(loaded.entry.is_none());
        assert!(loaded.reports[0].contains("plugin.json is missing"));

        let root = plugin_root(&dir.path().join("named"), "acme.tools");
        let loaded = load_plugin("other", &root, &data_dir());
        let entry = loaded.entry.expect("a mismatch is a warning, not fatal");
        assert_eq!(entry.name, "acme.tools");
        assert_eq!(entry.handle, "other");
        assert!(
            loaded.reports[0].contains("install handle"),
            "{:?}",
            loaded.reports
        );
    }

    /// §8.1: unimplemented namespaces pass with their contents unvalidated
    /// — hotl implements none, so arbitrary nested content is fine. A
    /// member value that is not an object at all is still a schema
    /// violation (fatal).
    #[test]
    fn unimplemented_extension_namespaces_pass_without_content_validation() {
        let (_, reports) = ok(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a",
                "extensions": {{"com.example.client": {{"weird": [1, {{"deep": null}}]}},
                               "io.github.nrakochy.hotl": {{}}}}}}"#
        ));
        assert!(reports.is_empty(), "{reports:?}");
        let err = fatal(&format!(
            r#"{{"$schema": "{MANIFEST_SCHEMA}", "name": "a",
                "extensions": {{"com.example.client": "not-an-object"}}}}"#
        ));
        assert!(err.contains("com.example.client"), "{err}");
    }

    fn mcp_doc(servers: &str) -> String {
        format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": {{{servers}}}}}"#)
    }

    /// One-server outcome against throwaway root/data dirs.
    fn one_server(server_json: &str) -> (McpOutcome, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(&dir.path().join("plugin"), "p");
        let data = dir.path().join("plugin-data/p");
        let out = parse_mcp_json(&mcp_doc(&format!(r#""srv": {server_json}"#)), &root, &data);
        (out, root, data)
    }

    /// §7.2.1's own example, verbatim shapes: the stdio server loads with
    /// expansion applied and the reserved pair appended last; both remote
    /// entries validate and then skip as unsupported transports (§7.2.2
    /// rule 4, owner decision 3).
    #[test]
    fn the_specs_mcp_example_loads_stdio_and_skips_remote_transports() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(&dir.path().join("plugin"), "p");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/validator"), "#!/bin/sh\n").unwrap();
        let data = dir.path().join("plugin-data/p");
        let text = format!(
            r#"{{
              "$schema": "{MCP_SCHEMA}",
              "mcpServers": {{
                "local-validator": {{
                  "type": "stdio",
                  "command": "./bin/validator",
                  "args": ["--data", "${{PLUGIN_DATA}}/validator"],
                  "env": {{"CONFIG": "${{PLUGIN_ROOT}}/config.json"}},
                  "cwd": "${{PLUGIN_ROOT}}"
                }},
                "deployment-api": {{
                  "type": "streamable-http",
                  "url": "https://deploy.example.com/mcp",
                  "headers": {{"X-Tenant": "public-tenant"}}
                }},
                "legacy-events": {{"type": "sse", "url": "https://legacy.example.com/sse"}}
              }}
            }}"#
        );
        let out = parse_mcp_json(&text, &root, &data);
        assert!(!out.disabled);
        assert_eq!(out.servers.len(), 1, "{:?}", out.reports);
        let s = &out.servers[0];
        assert_eq!(s.name, "local-validator");
        assert_eq!(s.command, root.join("bin/validator").to_string_lossy());
        assert_eq!(
            s.args,
            vec![
                "--data".to_string(),
                format!("{}/validator", data.display())
            ]
        );
        assert_eq!(s.cwd, root);
        assert_eq!(
            s.env,
            vec![
                (
                    "CONFIG".to_string(),
                    format!("{}/config.json", root.display())
                ),
                (
                    "PLUGIN_ROOT".to_string(),
                    root.to_string_lossy().into_owned()
                ),
                (
                    "PLUGIN_DATA".to_string(),
                    data.to_string_lossy().into_owned()
                ),
            ],
            "configured pairs first, reserved pair last (§9.1)"
        );
        assert_eq!(out.reports.len(), 2, "{:?}", out.reports);
        assert!(
            out.reports
                .iter()
                .all(|r| r.contains("unsupported transport")),
            "{:?}",
            out.reports
        );
    }

    /// §9.2: one non-recursive textual pass over the original string.
    #[test]
    fn expansion_is_single_pass_exact_and_literal_preserving() {
        assert_eq!(expand("${PLUGIN_ROOT}/x", "/r", "/d"), "/r/x");
        assert_eq!(
            expand("a${PLUGIN_DATA}b${PLUGIN_ROOT}c", "/r", "/d"),
            "a/db/rc"
        );
        // Replacement text is never rescanned: a data path carrying the
        // literal placeholder survives.
        assert_eq!(
            expand("${PLUGIN_DATA}/x", "/r", "/d/${PLUGIN_ROOT}"),
            "/d/${PLUGIN_ROOT}/x"
        );
        for lit in [
            "${HOME}",
            "$PLUGIN_ROOT",
            "${PLUGIN_ROOTX}",
            "${plugin_root}",
        ] {
            assert_eq!(expand(lit, "/r", "/d"), lit, "must stay literal");
        }
    }

    /// §9.2: a reserved env name invalidates only that entry; the sibling
    /// still loads.
    #[test]
    fn a_reserved_env_name_invalidates_only_that_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(&dir.path().join("plugin"), "p");
        let data = dir.path().join("data");
        let out = parse_mcp_json(
            &mcp_doc(
                r#""bad": {"type": "stdio", "command": "npx",
                          "env": {"PLUGIN_ROOT": "/somewhere"}},
                   "good": {"type": "stdio", "command": "npx"}"#,
            ),
            &root,
            &data,
        );
        assert!(!out.disabled);
        assert_eq!(out.servers.len(), 1);
        assert_eq!(out.servers[0].name, "good");
        assert_eq!(out.reports.len(), 1);
        assert!(
            out.reports[0].contains("PLUGIN_ROOT") && out.reports[0].contains("invalid"),
            "{}",
            out.reports[0]
        );
    }

    /// §7.2.1: the closed union — unknown fields, cross-variant fields,
    /// and unknown `type` values invalidate the entry.
    #[test]
    fn unknown_fields_cross_variant_fields_and_unknown_types_invalidate() {
        for bad in [
            r#"{"type": "stdio", "command": "npx", "url": "https://x.com"}"#,
            r#"{"type": "stdio", "command": "npx", "flags": []}"#,
            r#"{"type": "sse", "url": "https://x.com/sse", "command": "x"}"#,
            r#"{"type": "websocket", "url": "wss://x.com"}"#,
            r#"{"type": "stdio"}"#,
            r#"{"command": "npx"}"#,
            r#""just a string""#,
            r#"{"type": "stdio", "command": "npx", "args": "not-a-list"}"#,
            r#"{"type": "stdio", "command": "npx", "env": {"K": 1}}"#,
        ] {
            let (out, _, _) = one_server(bad);
            assert!(!out.disabled, "{bad}");
            assert!(out.servers.is_empty(), "{bad}");
            assert_eq!(out.reports.len(), 1, "{bad}: {:?}", out.reports);
            assert!(out.reports[0].contains("invalid"), "{}", out.reports[0]);
        }
    }

    /// The invalid-remote and unsupported-transport reports are distinct
    /// strings — Appendix A's URL/header rows stay honest while remote
    /// transports are out of scope.
    #[test]
    fn remote_entries_validate_before_they_skip() {
        for bad in [
            r#"{"type": "streamable-http", "url": "ftp://x.com/mcp"}"#,
            r#"{"type": "streamable-http", "url": "https://x.com/mcp#frag"}"#,
            r#"{"type": "streamable-http", "url": "https://user@x.com/mcp"}"#,
            r#"{"type": "streamable-http", "url": "http://example.com/mcp"}"#,
            r#"{"type": "sse", "url": "https://x.com/sse",
                "headers": {"X-A": "1", "x-a": "2"}}"#,
            r#"{"type": "sse", "url": "https://x.com/sse", "headers": {"bad name": "v"}}"#,
            r#"{"type": "sse", "url": "https://x.com/sse", "headers": {"X-A": "v\u0000"}}"#,
        ] {
            let (out, _, _) = one_server(bad);
            assert!(out.servers.is_empty(), "{bad}");
            assert!(
                out.reports[0].contains("invalid")
                    && !out.reports[0].contains("unsupported transport"),
                "{bad} → {}",
                out.reports[0]
            );
        }
        for good in [
            r#"{"type": "streamable-http", "url": "https://x.com/mcp"}"#,
            r#"{"type": "streamable-http", "url": "http://localhost:8080/mcp"}"#,
            r#"{"type": "streamable-http", "url": "http://127.0.0.1/mcp"}"#,
            r#"{"type": "streamable-http", "url": "http://[::1]:3000/mcp"}"#,
            r#"{"type": "sse", "url": "https://x.com/sse", "headers": {"X-A": "v"}}"#,
        ] {
            let (out, _, _) = one_server(good);
            assert!(
                out.reports[0].contains("unsupported transport")
                    && !out.reports[0].contains("invalid"),
                "{good} → {}",
                out.reports[0]
            );
        }
    }

    /// §10.1/§7.2.2 rule 2: a version mismatch or unrecognized `$schema`
    /// disables MCP for the plugin — versions are compared, not strings.
    #[test]
    fn a_schema_version_mismatch_disables_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(dir.path(), "p");
        let data = dir.path().join("data");
        let mismatched = r#"{"$schema":
            "https://agent-plugins.org/schemas/1.0.1/mcp.schema.json",
            "mcpServers": {}}"#;
        let out = parse_mcp_json(mismatched, &root, &data);
        assert!(out.disabled);
        assert!(
            out.reports[0].contains("1.0.1") && out.reports[0].contains("1.0.0"),
            "{}",
            out.reports[0]
        );
        let unrecognized = format!(r#"{{"$schema": "{MANIFEST_SCHEMA}", "mcpServers": {{}}}}"#);
        let out = parse_mcp_json(&unrecognized, &root, &data);
        assert!(out.disabled);
        assert!(
            out.reports[0].contains("not recognized"),
            "{}",
            out.reports[0]
        );
    }

    /// §7.2.1/§7.2.2 rule 2: the top level is exactly `$schema` +
    /// `mcpServers`; anything else (or a non-object) disables MCP. An
    /// empty `mcpServers` is valid.
    #[test]
    fn the_mcp_top_level_is_closed_and_empty_servers_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(dir.path(), "p");
        let data = dir.path().join("data");
        for bad in [
            format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": {{}}, "extra": 1}}"#),
            "[]".to_string(),
            "not json".to_string(),
            format!(r#"{{"$schema": "{MCP_SCHEMA}"}}"#),
            format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": []}}"#),
            r#"{"mcpServers": {}}"#.to_string(),
        ] {
            let out = parse_mcp_json(&bad, &root, &data);
            assert!(out.disabled, "{bad}");
            assert!(out.servers.is_empty());
        }
        let out = parse_mcp_json(&mcp_doc(""), &root, &data);
        assert!(!out.disabled);
        assert!(out.servers.is_empty() && out.reports.is_empty());
    }

    /// §7.2.1's `command` forms: one executable token, bare or
    /// `./`-relative, no expansion.
    #[test]
    fn command_forms_match_the_spec() {
        let (out, root, _) = one_server(r#"{"type": "stdio", "command": "./bin/server"}"#);
        assert_eq!(
            out.servers[0].command,
            root.join("bin/server").to_string_lossy(),
            "a plugin-relative command is root-joined even before it exists"
        );
        let (out, _, _) = one_server(r#"{"type": "stdio", "command": "npx"}"#);
        assert_eq!(out.servers[0].command, "npx", "a bare name stays bare");
        for bad in [
            r#"{"type": "stdio", "command": "../bin/server"}"#,
            r#"{"type": "stdio", "command": "bin/server"}"#,
            r#"{"type": "stdio", "command": "${PLUGIN_ROOT}/bin"}"#,
            r#"{"type": "stdio", "command": "./bin/../../escape"}"#,
            r#"{"type": "stdio", "command": ""}"#,
            r#"{"type": "stdio", "command": "/abs/path"}"#,
        ] {
            let (out, _, _) = one_server(bad);
            assert!(out.servers.is_empty(), "{bad}");
            assert!(out.reports[0].contains("command"), "{}", out.reports[0]);
        }
    }

    /// §7.2.1's `cwd` forms, including the omitted → plugin-root default
    /// and a not-yet-existing `${PLUGIN_DATA}` target.
    #[test]
    fn cwd_forms_match_the_spec() {
        let (out, root, _) = one_server(r#"{"type": "stdio", "command": "npx"}"#);
        assert_eq!(out.servers[0].cwd, root, "omitted cwd is the plugin root");
        let (out, root, _) = one_server(r#"{"type": "stdio", "command": "npx", "cwd": "./data"}"#);
        assert_eq!(out.servers[0].cwd, root.join("data"));
        let (out, root, _) =
            one_server(r#"{"type": "stdio", "command": "npx", "cwd": "${PLUGIN_ROOT}"}"#);
        assert_eq!(out.servers[0].cwd, root);
        let (out, _, data) =
            one_server(r#"{"type": "stdio", "command": "npx", "cwd": "${PLUGIN_DATA}/sub"}"#);
        assert_eq!(
            out.servers[0].cwd,
            data.join("sub"),
            "a not-yet-created data subdir is still a valid cwd"
        );
        for bad in [
            r#"{"type": "stdio", "command": "npx", "cwd": "data"}"#,
            r#"{"type": "stdio", "command": "npx", "cwd": "../x"}"#,
            r#"{"type": "stdio", "command": "npx", "cwd": "./../x"}"#,
            r#"{"type": "stdio", "command": "npx", "cwd": "${PLUGIN_ROOT}/../x"}"#,
            r#"{"type": "stdio", "command": "npx", "cwd": "/abs"}"#,
            r#"{"type": "stdio", "command": "npx", "cwd": "./${PLUGIN_DATA}"}"#,
            r#"{"type": "stdio", "command": "npx", "cwd": "${PLUGIN_ROOT}x"}"#,
        ] {
            let (out, _, _) = one_server(bad);
            assert!(out.servers.is_empty(), "{bad}");
            assert!(out.reports[0].contains("cwd"), "{bad} → {}", out.reports[0]);
        }
    }

    /// An escaping symlinked command invalidates the entry (§4.1
    /// boundary 4 via the entry boundary).
    #[cfg(unix)]
    #[test]
    fn an_escaping_symlinked_command_invalidates_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside-bin");
        std::fs::write(&outside, "#!/bin/sh\n").unwrap();
        let root = plugin_root(&dir.path().join("plugin"), "p");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("bin/server")).unwrap();
        let out = parse_mcp_json(
            &mcp_doc(r#""srv": {"type": "stdio", "command": "./bin/server"}"#),
            &root,
            &dir.path().join("data"),
        );
        assert!(out.servers.is_empty());
        assert!(out.reports[0].contains("command"), "{}", out.reports[0]);
    }

    /// `load_plugin` wires `mcp.json` in through the fixed location: an
    /// absent file is silent (§6.2), a present one populates `servers`,
    /// and an escaping one invalidates only the MCP component.
    #[test]
    fn load_plugin_discovers_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let root = plugin_root(dir.path(), "p");
        write_skill(&root.join("skills/s"));
        let loaded = load_plugin("p", &root, &data_dir());
        assert!(loaded.entry.unwrap().servers.is_empty());
        assert!(loaded.reports.is_empty());

        std::fs::write(
            root.join("mcp.json"),
            mcp_doc(r#""mem": {"type": "stdio", "command": "npx", "args": ["-y", "@x/mem"]}"#),
        )
        .unwrap();
        let loaded = load_plugin("p", &root, &data_dir());
        let entry = loaded.entry.unwrap();
        assert_eq!(entry.servers.len(), 1);
        assert_eq!(entry.servers[0].name, "mem");
        assert_eq!(entry.plugin_data, data_dir().join("p"));
        assert!(loaded.reports.is_empty(), "{:?}", loaded.reports);
    }

    #[cfg(unix)]
    #[test]
    fn an_escaping_mcp_json_invalidates_only_the_mcp_component() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside-mcp.json");
        std::fs::write(&outside, mcp_doc("")).unwrap();
        let root = plugin_root(&dir.path().join("plugin"), "p");
        write_skill(&root.join("skills/s"));
        std::os::unix::fs::symlink(&outside, root.join("mcp.json")).unwrap();

        let loaded = load_plugin("p", &root, &data_dir());
        let entry = loaded.entry.expect("plugin still loads");
        assert_eq!(entry.skill_dirs.len(), 1, "skills are unaffected");
        assert!(entry.servers.is_empty());
        assert!(
            loaded.reports[0].contains("mcp.json"),
            "{:?}",
            loaded.reports
        );
    }
}
