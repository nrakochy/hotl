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
}

/// Load one plugin from a directory (§11.1 rule 1). Containment is
/// symlink-resolving canonicalize + prefix — deliberately not
/// `fsguard::resolve_beneath`, whose no-follow descent rejects the
/// in-root symlinks §4.1 explicitly permits.
pub fn load_plugin(handle: &str, root: &Path, _plugin_data: &Path) -> LoadedPlugin {
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
    LoadedPlugin {
        entry: Some(PluginEntry {
            name: manifest.name.clone(),
            handle: handle.to_string(),
            root,
            manifest,
            skill_dirs,
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
}
