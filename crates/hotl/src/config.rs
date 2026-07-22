//! The single config file: `~/.config/hotl/config.toml`.
//!
//! One place for every hand-editable setting — provider, context/compaction,
//! behavior, retention, network egress — plus the domain sections (`[[allow]]`, `[[mcp]]`,
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
    #[serde(default)]
    pub network: NetworkCfg,
    #[serde(default)]
    pub permissions: PermissionsCfg,
    #[serde(default)]
    pub skills: SkillsCfg,
    /// Raw document, for reserializing the domain sections to their loaders.
    #[serde(skip)]
    raw: Option<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SkillsCfg {
    /// `false` stops reading Claude Code skill roots (`~/.claude/skills`,
    /// the plugin cache). Default: read them when present.
    pub claude: Option<bool>,
    /// `[skills.marketplaces]` — extra skill sources: name → local path
    /// (read in place) or git URL (managed checkout; `hotl skills add`).
    #[serde(default)]
    pub marketplaces: std::collections::BTreeMap<String, String>,
}

impl SkillsCfg {
    /// Resolve `[skills.marketplaces]` to discovery roots: a local path is
    /// read in place (`~` expanded), a git URL maps to its managed checkout
    /// under `<config_dir>/marketplaces/<name>`. An invalid name is skipped
    /// with a warning — fail-closed, a bad entry never loads skills.
    pub fn marketplace_roots(
        &self,
        config_dir: &Path,
    ) -> (Vec<(String, std::path::PathBuf)>, Vec<String>) {
        let mut roots = Vec::new();
        let mut warnings = Vec::new();
        for (name, source) in &self.marketplaces {
            let Some(name) = hotl_tools::skills::normalize_marketplace_name(name) else {
                warnings.push(format!(
                    "[skills.marketplaces] `{name}` is not a valid marketplace name \
                     (letters, digits, `.`/`_`/`-`, alphanumeric first char, ≤ 64 chars) \
                     — entry skipped"
                ));
                continue;
            };
            let dir = if is_git_url(source) {
                config_dir.join("marketplaces").join(&name)
            } else {
                expand_home(source)
            };
            roots.push((name, dir));
        }
        (roots, warnings)
    }
}

/// A git URL as opposed to a local path: a fetch scheme, an scp-style
/// `git@` prefix, or a trailing `.git`.
pub fn is_git_url(source: &str) -> bool {
    source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.ends_with(".git")
}

/// Expand a leading `~/` against `$HOME`.
fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

#[derive(Debug, Default, Deserialize)]
pub struct PermissionsCfg {
    /// `"auto"` (default — no ordinary prompts) | `"ask"`.
    pub mode: Option<String>,
}

impl PermissionsCfg {
    /// Resolve the mode: env (`HOTL_PERMISSIONS`) > config > default `auto`.
    /// An unknown value fails **closed** to `Ask` with a warning — a typo
    /// must never silently mean "don't prompt".
    pub fn resolve(
        &self,
        env: Option<&str>,
    ) -> (hotl_tools::rules::PermissionMode, Option<String>) {
        use hotl_tools::rules::PermissionMode;
        let source = env.or(self.mode.as_deref());
        match source {
            None | Some("auto") => (PermissionMode::Auto, None),
            Some("ask") => (PermissionMode::Ask, None),
            Some(other) => (
                PermissionMode::Ask,
                Some(format!(
                    "[permissions].mode = \"{other}\" is not a mode (auto | ask) — failing closed to \"ask\""
                )),
            ),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ProviderCfg {
    /// `provider/model`, e.g. `openai/gpt-5` or `anthropic/claude-opus-4-8`.
    pub model: Option<String>,
    /// OpenAI-compatible base URL (for the `openai` provider).
    pub base_url: Option<String>,
    /// Cheap model for compaction summaries.
    pub fast_model: Option<String>,
    /// Command whose stdout (trimmed) is the API key. When set, it beats the
    /// static key env vars: configuring a helper is a deliberate act.
    pub api_key_helper: Option<String>,
    /// Re-run the helper when the cached key is older than this. Absent =
    /// refresh only at startup and on auth failure.
    pub api_key_helper_ttl_secs: Option<u64>,
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
    /// `false` disables the bash sandbox floor.
    pub sandbox: Option<bool>,
    /// Vim-style keys in the TUI input editor (default on, matching watch).
    pub vim_mode: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RetentionCfg {
    pub max_age_days: Option<u64>,
    pub max_sessions: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NetworkCfg {
    /// `"open"` (default) | `"off"` | `"allowlist"`.
    pub egress: Option<String>,
    /// Hosts reachable in allowlist mode (`"github.com"`, `"*.crates.io"`).
    #[serde(default)]
    pub allow: Vec<String>,
}

impl NetworkCfg {
    /// Resolve `[network]` to the process egress policy. An unknown mode
    /// fails **closed** to `Off` with a loud warning — a typo must never
    /// mean "open".
    pub fn egress_policy(&self) -> (hotl_tools::net::EgressPolicy, Option<String>) {
        use hotl_tools::net::EgressPolicy;
        match self.egress.as_deref() {
            None | Some("open") => (EgressPolicy::Open, None),
            Some("off") => (EgressPolicy::Off, None),
            Some("allowlist") => (EgressPolicy::Allowlist(self.allow.clone()), None),
            Some(other) => (
                EgressPolicy::Off,
                Some(format!(
                    "config.toml [network].egress = \"{other}\" is not a mode \
                     (open | off | allowlist) — failing closed to \"off\""
                )),
            ),
        }
    }
}

impl Config {
    /// Load `config.toml`; a malformed file warns and yields defaults
    /// (fail-closed: a typo never silently changes a setting).
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
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
            vim_mode = false

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
        assert_eq!(cfg.behavior.vim_mode, Some(false));
        assert_eq!(cfg.retention.max_age_days, Some(30));
        assert_eq!(cfg.retention.max_sessions, Some(100));
        // Domain sections reserialize to their loaders' shapes.
        assert!(cfg.allow_toml().unwrap().contains("prefix = \"cargo \""));
        assert!(
            cfg.mcp_toml().unwrap().contains("[[server]]")
                && cfg.mcp_toml().unwrap().contains("docs")
        );
        let hooks = cfg.hooks_toml().unwrap();
        assert!(hooks.contains("pre_tool") && hooks.contains("cargo check"));
    }

    #[test]
    fn provider_helper_keys_parse() {
        let cfg = cfg_with(
            "[provider]\napi_key_helper = \"mint-key --gw\"\napi_key_helper_ttl_secs = 300\n",
        );
        assert_eq!(
            cfg.provider.api_key_helper.as_deref(),
            Some("mint-key --gw")
        );
        assert_eq!(cfg.provider.api_key_helper_ttl_secs, Some(300));
    }

    #[test]
    fn network_egress_parses_and_unknown_fails_closed() {
        use hotl_tools::net::EgressPolicy;
        // Absent section: Open, no warning (the default is today's behavior).
        let (policy, warning) = cfg_with("").network.egress_policy();
        assert_eq!(policy, EgressPolicy::Open);
        assert!(warning.is_none());
        // Explicit modes.
        let (policy, warning) = cfg_with("[network]\negress = \"off\"\n")
            .network
            .egress_policy();
        assert_eq!(policy, EgressPolicy::Off);
        assert!(warning.is_none());
        let cfg = cfg_with(
            "[network]\negress = \"allowlist\"\nallow = [\"github.com\", \"*.crates.io\"]\n",
        );
        let (policy, warning) = cfg.network.egress_policy();
        assert_eq!(
            policy,
            EgressPolicy::Allowlist(vec!["github.com".into(), "*.crates.io".into()])
        );
        assert!(warning.is_none());
        // Unknown value: fail closed to Off, loudly — never open.
        let (policy, warning) = cfg_with("[network]\negress = \"opne\"\n")
            .network
            .egress_policy();
        assert_eq!(policy, EgressPolicy::Off);
        assert!(warning.unwrap().contains("opne"));
    }

    #[test]
    fn permissions_mode_resolves_with_env_and_fails_closed() {
        use hotl_tools::rules::PermissionMode;
        // Absent → the product default: auto, no warning.
        let (m, w) = cfg_with("").permissions.resolve(None);
        assert_eq!(m, PermissionMode::Auto);
        assert!(w.is_none());
        // Explicit ask.
        let (m, _) = cfg_with("[permissions]\nmode = \"ask\"\n")
            .permissions
            .resolve(None);
        assert_eq!(m, PermissionMode::Ask);
        // Env beats config.
        let (m, _) = cfg_with("[permissions]\nmode = \"ask\"\n")
            .permissions
            .resolve(Some("auto"));
        assert_eq!(m, PermissionMode::Auto);
        // Typo fails closed to ask, loudly — never silently auto.
        let (m, w) = cfg_with("[permissions]\nmode = \"atuo\"\n")
            .permissions
            .resolve(None);
        assert_eq!(m, PermissionMode::Ask);
        assert!(w.unwrap().contains("atuo"));
    }

    #[test]
    fn vim_mode_parses_and_defaults() {
        let cfg = cfg_with("[behavior]\nvim_mode = false\n");
        assert_eq!(cfg.behavior.vim_mode, Some(false));
        assert_eq!(cfg_with("").behavior.vim_mode, None);
    }

    #[test]
    fn skills_marketplaces_parse_and_resolve() {
        let cfg = cfg_with(
            "[skills.marketplaces]\n\
             acme = \"https://github.com/acme/skills.git\"\n\
             team = \"/abs/team-skills\"\n\
             \"bad:name\" = \"/x\"\n",
        );
        let dir = std::path::Path::new("/cfg");
        let (roots, warnings) = cfg.skills.marketplace_roots(dir);
        assert_eq!(
            roots,
            vec![
                ("acme".to_string(), dir.join("marketplaces/acme")),
                (
                    "team".to_string(),
                    std::path::PathBuf::from("/abs/team-skills")
                ),
            ]
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("bad:name"), "{warnings:?}");

        // `~/` expands against HOME; absent section resolves empty.
        let cfg = cfg_with("[skills.marketplaces]\nhome = \"~/team-skills\"\n");
        let (roots, _) = cfg.skills.marketplace_roots(dir);
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(roots, vec![("home".to_string(), home.join("team-skills"))]);
        assert!(cfg_with("").skills.marketplace_roots(dir).0.is_empty());
    }

    #[test]
    fn git_url_detection() {
        for url in [
            "https://github.com/a/b.git",
            "http://host/repo",
            "git@github.com:a/b.git",
            "ssh://host/repo",
            "/local/path/origin.git",
        ] {
            assert!(is_git_url(url), "{url}");
        }
        for path in ["~/skills", "/abs/dir", "relative/dir"] {
            assert!(!is_git_url(path), "{path}");
        }
    }

    #[test]
    fn empty_or_absent_config_is_defaults_and_none_sections() {
        let cfg = Config::load(std::path::Path::new("/no/such/dir"));
        assert!(cfg.provider.model.is_none());
        assert!(
            cfg.allow_toml().is_none() && cfg.mcp_toml().is_none() && cfg.hooks_toml().is_none()
        );
        assert!(cfg.retention.max_age_days.is_none());
    }
}
