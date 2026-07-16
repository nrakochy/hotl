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
            agents: vec!["claude".into(), "codex".into()],
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub active: String,
    pub blocked: String,
    pub idle: String,
    pub ink: String,
    pub muted: String,
    pub faint: String,
    pub accent: String,
    pub band: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            active: "#f2c14e".into(),
            blocked: "#e06c6c".into(),
            idle: "#7ee07e".into(),
            ink: "#e6e9f0".into(),
            muted: "#8a92a6".into(),
            faint: "#596072".into(),
            accent: "#6c8cff".into(),
            band: "#1b2233".into(),
        }
    }
}

/// Theme selection: an optional `preset` base palette plus optional per-slot
/// color overrides. Absent fields fall back to the preset (or default).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: Option<String>,
    pub active: Option<String>,
    pub blocked: Option<String>,
    pub idle: Option<String>,
    pub ink: Option<String>,
    pub muted: Option<String>,
    pub faint: Option<String>,
    pub accent: Option<String>,
    pub band: Option<String>,
}

/// Parse a `#rrggbb` (or `rrggbb`) hex color into its RGB bytes. Returns `None`
/// for any string that isn't exactly six hex digits — the single source of
/// truth for what counts as a valid color, shared by config validation and the
/// TUI renderer.
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.strip_prefix('#').unwrap_or(s);
    // Guard on the hex-digit charset first: this also guarantees ASCII, so the
    // byte slices below always land on char boundaries (no panic on multi-byte
    // input like "aéaé", which is 6 bytes but not 6 hex digits).
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&s[0..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..6], 16).ok()?,
    ])
}

impl ThemeConfig {
    /// Resolve to a concrete palette. Unknown preset name -> default base + a
    /// warning; set color fields override the base slot-by-slot. Any override
    /// that isn't a valid `#rrggbb` color is ignored (base kept) and reported
    /// in the warning rather than silently swallowed downstream.
    pub fn resolve(&self) -> (Theme, Option<String>) {
        let (mut base, preset_warn) = match &self.preset {
            None => (Theme::default(), None),
            Some(n) => match preset(n) {
                Some(t) => (t, None),
                None => (Theme::default(), Some(format!("unknown theme '{n}' — using default"))),
            },
        };

        let mut bad: Vec<&'static str> = Vec::new();
        let mut apply = |slot: &mut String, name: &'static str, value: &Option<String>| {
            if let Some(c) = value {
                if parse_hex(c).is_some() {
                    *slot = c.clone();
                } else {
                    bad.push(name);
                }
            }
        };
        apply(&mut base.active, "active", &self.active);
        apply(&mut base.blocked, "blocked", &self.blocked);
        apply(&mut base.idle, "idle", &self.idle);
        apply(&mut base.ink, "ink", &self.ink);
        apply(&mut base.muted, "muted", &self.muted);
        apply(&mut base.faint, "faint", &self.faint);
        apply(&mut base.accent, "accent", &self.accent);
        apply(&mut base.band, "band", &self.band);

        let color_warn = (!bad.is_empty())
            .then(|| format!("ignoring invalid theme color(s): {}", bad.join(", ")));

        // Surface both problems if present; neither is silently dropped.
        let warn = match (preset_warn, color_warn) {
            (Some(p), Some(c)) => Some(format!("{p}; {c}")),
            (Some(w), None) | (None, Some(w)) => Some(w),
            (None, None) => None,
        };
        (base, warn)
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
            Err(e) => (HotlConfig::default(), Some(format!("config parse error: {e}"))),
        }
    }

    pub fn load() -> HotlConfig {
        HotlConfig::load_with_warning().0
    }

    pub fn load_with_warning() -> (HotlConfig, Option<String>) {
        let (cfg, parse_warn) = match dirs_config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
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
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("hotl").join("config.toml"))
}

/// Look up a built-in palette by name. `None` if the name is unknown.
pub fn preset(name: &str) -> Option<Theme> {
    Some(match name {
        "default" => Theme::default(),
        "tokyo-night" => Theme {
            active: "#e0af68".into(), blocked: "#f7768e".into(), idle: "#9ece6a".into(),
            ink: "#c0caf5".into(), muted: "#787c99".into(), faint: "#565f89".into(),
            accent: "#7aa2f7".into(), band: "#292e42".into(),
        },
        "catppuccin" => Theme {
            active: "#f9e2af".into(), blocked: "#f38ba8".into(), idle: "#a6e3a1".into(),
            ink: "#cdd6f4".into(), muted: "#a6adc8".into(), faint: "#6c7086".into(),
            accent: "#89b4fa".into(), band: "#313244".into(),
        },
        "gruvbox" => Theme {
            active: "#d79921".into(), blocked: "#cc241d".into(), idle: "#98971a".into(),
            ink: "#ebdbb2".into(), muted: "#a89984".into(), faint: "#665c54".into(),
            accent: "#458588".into(), band: "#3c3836".into(),
        },
        "nord" => Theme {
            active: "#ebcb8b".into(), blocked: "#bf616a".into(), idle: "#a3be8c".into(),
            ink: "#eceff4".into(), muted: "#d8dee9".into(), faint: "#4c566a".into(),
            accent: "#88c0d0".into(), band: "#3b4252".into(),
        },
        "dracula" => Theme {
            active: "#f1fa8c".into(), blocked: "#ff5555".into(), idle: "#50fa7b".into(),
            ink: "#f8f8f2".into(), muted: "#b8b8c0".into(), faint: "#6272a4".into(),
            accent: "#bd93f9".into(), band: "#44475a".into(),
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let c = HotlConfig::parse("");
        assert!(c.settings.ping_on_blocked);
        assert_eq!(c.settings.poll_interval_ms, 1000);
        assert_eq!(c.settings.agents, vec!["claude".to_string(), "codex".to_string()]);
        assert_eq!(c.settings.theme.resolve().0.active, "#f2c14e");
    }

    #[test]
    fn partial_settings_override_only_given_fields() {
        let c = HotlConfig::parse("[settings]\nping_on_blocked = false\npoll_interval_ms = 500\n");
        assert!(!c.settings.ping_on_blocked);
        assert_eq!(c.settings.poll_interval_ms, 500);
        assert_eq!(c.settings.agents, vec!["claude".to_string(), "codex".to_string()]);
    }

    #[test]
    fn theme_override() {
        let c = HotlConfig::parse("[settings.theme]\nblocked = \"#ff0000\"\n");
        let (theme, _) = c.settings.theme.resolve();
        assert_eq!(theme.blocked, "#ff0000");
        assert_eq!(theme.idle, "#7ee07e");
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
    fn preset_default_equals_theme_default() {
        assert_eq!(preset("default"), Some(Theme::default()));
    }
    #[test]
    fn preset_known_returns_palette() {
        let t = preset("tokyo-night").expect("exists");
        assert_eq!(t.blocked, "#f7768e");
        assert_eq!(t.accent, "#7aa2f7");
    }
    #[test]
    fn preset_unknown_is_none() {
        assert_eq!(preset("nope"), None);
    }
    #[test]
    fn all_named_presets_exist() {
        for name in ["default","tokyo-night","catppuccin","gruvbox","nord","dracula"] {
            assert!(preset(name).is_some(), "missing {name}");
        }
    }
    #[test]
    fn resolve_no_preset_is_default() {
        let (theme, warn) = ThemeConfig::default().resolve();
        assert_eq!(theme, Theme::default());
        assert!(warn.is_none());
    }
    #[test]
    fn resolve_known_preset() {
        let tc = ThemeConfig { preset: Some("gruvbox".into()), ..Default::default() };
        let (theme, warn) = tc.resolve();
        assert_eq!(theme, preset("gruvbox").unwrap());
        assert!(warn.is_none());
    }
    #[test]
    fn resolve_unknown_preset_defaults_with_warning() {
        let tc = ThemeConfig { preset: Some("typo".into()), ..Default::default() };
        let (theme, warn) = tc.resolve();
        assert_eq!(theme, Theme::default());
        assert!(warn.as_deref().unwrap().contains("typo"));
    }
    #[test]
    fn resolve_preset_with_override() {
        let tc = ThemeConfig { preset: Some("gruvbox".into()), blocked: Some("#ff0000".into()), ..Default::default() };
        let (theme, _) = tc.resolve();
        assert_eq!(theme.blocked, "#ff0000");
        assert_eq!(theme.idle, preset("gruvbox").unwrap().idle);
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

    #[test]
    fn parse_hex_accepts_valid_with_and_without_hash() {
        assert_eq!(parse_hex("#7ee07e"), Some([0x7e, 0xe0, 0x7e]));
        assert_eq!(parse_hex("7ee07e"), Some([0x7e, 0xe0, 0x7e]));
    }

    #[test]
    fn parse_hex_rejects_non_hex_and_wrong_length() {
        assert_eq!(parse_hex("nonsense"), None);
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#gggggg"), None);
    }

    #[test]
    fn parse_hex_rejects_six_byte_multibyte_without_panicking() {
        // "aéaé" is 6 bytes but not char-aligned at 2/4; must reject, not panic.
        assert_eq!(parse_hex("aéaé"), None);
    }

    #[test]
    fn resolve_ignores_invalid_color_and_warns() {
        let tc = ThemeConfig { active: Some("notacolor".into()), ..Default::default() };
        let (theme, warn) = tc.resolve();
        assert_eq!(theme.active, Theme::default().active, "bad color keeps base");
        let w = warn.expect("invalid color should warn");
        assert!(w.contains("active"), "warning names the offending slot: {w}");
    }

    #[test]
    fn resolve_reports_both_unknown_preset_and_bad_color() {
        let tc = ThemeConfig {
            preset: Some("typo".into()),
            blocked: Some("xyz".into()),
            ..Default::default()
        };
        let w = tc.resolve().1.expect("should warn");
        assert!(w.contains("typo"), "mentions preset: {w}");
        assert!(w.contains("blocked"), "mentions color: {w}");
    }
}
