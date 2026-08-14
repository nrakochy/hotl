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
