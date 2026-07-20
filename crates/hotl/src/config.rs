//! The single config file: `~/.config/hotl/config.toml`.
//!
//! One place for every hand-editable setting — provider, context/compaction,
//! behavior, retention — plus the domain sections (`[[allow]]`, `[[mcp]]`,
//! `[[hook]]`, `[diagnostics]`) that used to live in their own files. The
//! prose/data that isn't really "settings" stays separate: `system-prompt.md`,
//! `memory/`, `skills/`, and the machine-written `trust.toml`.
//!
//! Precedence for scalar settings: **environment variable > config.toml >
//! built-in default**, so existing `HOTL_*`-based setups and CI keep working.
//! For the domain sections, config.toml wins if it defines them; otherwise the
//! legacy standalone file (`permissions.toml`, `mcp.toml`, `hooks.toml`) is
//! read as a fallback so no one's setup breaks.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderCfg,
    #[serde(default)]
    pub context: ContextCfg,
    #[serde(default)]
    pub behavior: BehaviorCfg,
    #[serde(default)]
    pub retention: RetentionCfg,
    /// Raw document, for reserializing the domain sections to their loaders.
    #[serde(skip)]
    raw: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProviderCfg {
    /// `provider/model`, e.g. `openai/gpt-5` or `anthropic/claude-opus-4-8`.
    pub model: Option<String>,
    /// OpenAI-compatible base URL (for the `openai` provider).
    pub base_url: Option<String>,
    /// Cheap model for compaction summaries.
    pub fast_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContextCfg {
    pub window: Option<u64>,
    pub compaction_reset: Option<bool>,
    pub show_used_pct: Option<bool>,
    pub evict_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BehaviorCfg {
    /// Seconds an interactive permission ask waits before default-denying
    /// (`0` = wait forever).
    pub ask_timeout_secs: Option<u64>,
    /// `false` disables the bash sandbox floor.
    pub sandbox: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RetentionCfg {
    pub max_age_days: Option<u64>,
    pub max_sessions: Option<usize>,
}

impl Config {
    /// Load `config.toml`; a malformed file warns and yields defaults
    /// (fail-closed: a typo never silently changes a setting).
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        match text.parse::<toml::Value>() {
            Ok(raw) => {
                // Parse the typed settings from the same source string (no
                // deep clone of the raw document just to deserialize it).
                let mut cfg: Config = toml::from_str(&text).unwrap_or_default();
                cfg.raw = Some(raw);
                cfg
            }
            Err(e) => {
                eprintln!("hotl: config.toml ignored (parse error): {e}");
                Self::default()
            }
        }
    }

    /// The `[[allow]]` rules as a standalone TOML string (for `Rules::from_toml`),
    /// or `None` if config.toml has no `allow` section (→ fall back to the file).
    pub fn allow_toml(&self) -> Option<String> {
        self.section_as_toml("allow")
    }

    /// The `[[mcp]]` servers as a `[[server]]`-shaped TOML string (matching the
    /// legacy `mcp.toml` schema), or `None`.
    pub fn mcp_toml(&self) -> Option<String> {
        let servers = self.raw.as_ref()?.get("mcp")?;
        toml::to_string(&toml::toml! { server = (servers.clone()) }).ok()
    }

    /// The `[[hook]]` + `[diagnostics]` as a `hooks.toml`-shaped string, or None.
    pub fn hooks_toml(&self) -> Option<String> {
        let raw = self.raw.as_ref()?;
        let hooks = raw.get("hook");
        let diags = raw.get("diagnostics");
        if hooks.is_none() && diags.is_none() {
            return None;
        }
        let mut doc = toml::map::Map::new();
        if let Some(h) = hooks {
            doc.insert("hook".into(), h.clone());
        }
        if let Some(d) = diags {
            doc.insert("diagnostics".into(), d.clone());
        }
        toml::to_string(&toml::Value::Table(doc)).ok()
    }

    fn section_as_toml(&self, key: &str) -> Option<String> {
        let value = self.raw.as_ref()?.get(key)?;
        let mut doc = toml::map::Map::new();
        doc.insert(key.into(), value.clone());
        toml::to_string(&toml::Value::Table(doc)).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(toml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), toml).unwrap();
        Config::load(dir.path())
    }

    #[test]
    fn parses_settings_and_domain_sections() {
        let cfg = cfg_with(
            r#"
            [provider]
            model = "openai/gpt-5"
            base_url = "http://localhost:11434/v1"

            [context]
            window = 128000
            evict_tokens = 5000

            [behavior]
            ask_timeout_secs = 0

            [retention]
            max_age_days = 30
            max_sessions = 100

            [[allow]]
            tool = "bash"
            prefix = "cargo "

            [[mcp]]
            name = "docs"
            command = "/bin/docs"
            description = "d"

            [[hook]]
            event = "pre_tool"
            command = "/bin/guard"

            [diagnostics]
            rs = "cargo check"
            "#,
        );
        assert_eq!(cfg.provider.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(cfg.context.window, Some(128_000));
        assert_eq!(cfg.behavior.ask_timeout_secs, Some(0));
        assert_eq!(cfg.retention.max_age_days, Some(30));
        assert_eq!(cfg.retention.max_sessions, Some(100));
        // Domain sections reserialize to their loaders' shapes.
        assert!(cfg.allow_toml().unwrap().contains("prefix = \"cargo \""));
        assert!(cfg.mcp_toml().unwrap().contains("[[server]]") && cfg.mcp_toml().unwrap().contains("docs"));
        let hooks = cfg.hooks_toml().unwrap();
        assert!(hooks.contains("pre_tool") && hooks.contains("cargo check"));
    }

    #[test]
    fn empty_or_absent_config_is_defaults_and_none_sections() {
        let cfg = Config::load(std::path::Path::new("/no/such/dir"));
        assert!(cfg.provider.model.is_none());
        assert!(cfg.allow_toml().is_none() && cfg.mcp_toml().is_none() && cfg.hooks_toml().is_none());
        assert!(cfg.retention.max_age_days.is_none());
    }
}
