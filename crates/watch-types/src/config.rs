use hotl_theme::ThemeConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HotlConfig {
    pub settings: Settings,
    pub plugins: Plugins,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub ping_on_blocked: bool,
    pub poll_interval_ms: u64,
    pub agents: Vec<String>,
    pub vim_mode: bool,
    pub theme: ThemeConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ping_on_blocked: true,
            poll_interval_ms: 1000,
            agents: vec!["claude".into(), "codex".into(), "hotl".into()],
            vim_mode: true,
            theme: ThemeConfig::default(),
        }
    }
}

/// Lower bound on the poll interval, in milliseconds. A too-small (or zero)
/// value would busy-loop the event poll and peg a CPU core, so every consumer
/// must go through [`Settings::poll_interval_ms_clamped`] rather than reading
/// the raw field.
pub const MIN_POLL_INTERVAL_MS: u64 = 100;

impl Settings {
    /// The configured poll interval, floored at [`MIN_POLL_INTERVAL_MS`].
    pub fn poll_interval_ms_clamped(&self) -> u64 {
        self.poll_interval_ms.max(MIN_POLL_INTERVAL_MS)
    }
}

// Declared extension point; inert in v1 (parsed, not acted on).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Plugins {}

impl HotlConfig {
    pub fn parse(toml_str: &str) -> HotlConfig {
        HotlConfig::parse_checked(toml_str).0
    }

    // On a TOML parse error, returns defaults; caller may warn using the message.
    pub fn parse_checked(toml_str: &str) -> (HotlConfig, Option<String>) {
        match toml::from_str(toml_str) {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                HotlConfig::default(),
                Some(format!("config parse error: {e}")),
            ),
        }
    }

    pub fn load() -> HotlConfig {
        HotlConfig::load_with_warning().0
    }

    pub fn load_with_warning() -> (HotlConfig, Option<String>) {
        let (cfg, parse_warn) =
            match dirs_config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
                Some(s) => HotlConfig::parse_checked(&s),
                None => (HotlConfig::default(), None),
            };
        let warn = parse_warn.or_else(|| cfg.settings.theme.resolve().1);
        (cfg, warn)
    }
}

fn dirs_config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("hotl").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let c = HotlConfig::parse("");
        assert!(c.settings.ping_on_blocked);
        assert_eq!(c.settings.poll_interval_ms, 1000);
        assert_eq!(
            c.settings.agents,
            vec![
                "claude".to_string(),
                "codex".to_string(),
                "hotl".to_string()
            ]
        );
        assert_eq!(c.settings.theme.resolve().0.active, "#e0af68");
    }

    #[test]
    fn partial_settings_override_only_given_fields() {
        let c = HotlConfig::parse("[settings]\nping_on_blocked = false\npoll_interval_ms = 500\n");
        assert!(!c.settings.ping_on_blocked);
        assert_eq!(c.settings.poll_interval_ms, 500);
        assert_eq!(
            c.settings.agents,
            vec![
                "claude".to_string(),
                "codex".to_string(),
                "hotl".to_string()
            ]
        );
    }

    #[test]
    fn theme_override() {
        let c = HotlConfig::parse("[settings.theme]\nblocked = \"#ff0000\"\n");
        let (theme, _) = c.settings.theme.resolve();
        assert_eq!(theme.blocked, "#ff0000");
        assert_eq!(theme.idle, "#9ece6a");
    }

    #[test]
    fn agents_override() {
        let c = HotlConfig::parse("[settings]\nagents = [\"claude\"]\n");
        assert_eq!(c.settings.agents, vec!["claude".to_string()]);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        let c = HotlConfig::parse("this is not = = toml [[[");
        assert!(c.settings.ping_on_blocked);
    }

    #[test]
    fn malformed_toml_reports_a_warning() {
        let (c, warn) = HotlConfig::parse_checked("this is not = = toml [[[");
        assert!(c.settings.ping_on_blocked);
        assert!(warn.is_some(), "malformed config should yield a warning");
    }

    #[test]
    fn valid_toml_has_no_warning() {
        let (_, warn) = HotlConfig::parse_checked("[settings]\nping_on_blocked = false\n");
        assert!(warn.is_none());
    }

    #[test]
    fn plugins_section_parses_and_is_inert() {
        let c = HotlConfig::parse("[plugins]\n");
        assert!(c.settings.ping_on_blocked);
    }

    #[test]
    fn vim_mode_defaults_true() {
        assert!(HotlConfig::parse("").settings.vim_mode);
    }

    #[test]
    fn vim_mode_can_be_disabled() {
        let c = HotlConfig::parse("[settings]\nvim_mode = false\n");
        assert!(!c.settings.vim_mode);
        assert!(c.settings.ping_on_blocked);
    }

    #[test]
    fn settings_theme_parses_preset_and_override() {
        let c = HotlConfig::parse("[settings.theme]\npreset = \"nord\"\nblocked = \"#ff0000\"\n");
        assert_eq!(c.settings.theme.preset.as_deref(), Some("nord"));
        assert_eq!(c.settings.theme.blocked.as_deref(), Some("#ff0000"));
    }

    #[test]
    fn poll_interval_is_clamped_to_minimum() {
        let c = HotlConfig::parse("[settings]\npoll_interval_ms = 0\n");
        assert_eq!(c.settings.poll_interval_ms, 0, "raw value preserved");
        assert_eq!(c.settings.poll_interval_ms_clamped(), MIN_POLL_INTERVAL_MS);
    }

    #[test]
    fn poll_interval_above_minimum_is_unchanged() {
        let c = HotlConfig::parse("[settings]\npoll_interval_ms = 500\n");
        assert_eq!(c.settings.poll_interval_ms_clamped(), 500);
    }
}
