//! The hotl theme: config model, named presets, and (feature "ratatui") the
//! resolved terminal palette. One crate owns the pipeline from config string
//! to Color; both the watch dashboard and the execute TUI consume it.

use serde::Deserialize;

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
        // The out-of-the-box palette is tokyo-night; `preset("default")` is
        // its alias. One source of values — the preset table below.
        preset("tokyo-night").expect("built-in preset")
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
                None => (
                    Theme::default(),
                    Some(format!("unknown theme '{n}' — using default")),
                ),
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

/// Look up a built-in palette by name. `None` if the name is unknown.
pub fn preset(name: &str) -> Option<Theme> {
    Some(match name {
        "default" => Theme::default(),
        "tokyo-night" => Theme {
            active: "#e0af68".into(),
            blocked: "#f7768e".into(),
            idle: "#9ece6a".into(),
            ink: "#c0caf5".into(),
            muted: "#787c99".into(),
            faint: "#565f89".into(),
            accent: "#7aa2f7".into(),
            band: "#292e42".into(),
        },
        "catppuccin" => Theme {
            active: "#f9e2af".into(),
            blocked: "#f38ba8".into(),
            idle: "#a6e3a1".into(),
            ink: "#cdd6f4".into(),
            muted: "#a6adc8".into(),
            faint: "#6c7086".into(),
            accent: "#89b4fa".into(),
            band: "#313244".into(),
        },
        "gruvbox" => Theme {
            active: "#d79921".into(),
            blocked: "#cc241d".into(),
            idle: "#98971a".into(),
            ink: "#ebdbb2".into(),
            muted: "#a89984".into(),
            faint: "#665c54".into(),
            accent: "#458588".into(),
            band: "#3c3836".into(),
        },
        "nord" => Theme {
            active: "#ebcb8b".into(),
            blocked: "#bf616a".into(),
            idle: "#a3be8c".into(),
            ink: "#eceff4".into(),
            muted: "#d8dee9".into(),
            faint: "#4c566a".into(),
            accent: "#88c0d0".into(),
            band: "#3b4252".into(),
        },
        "dracula" => Theme {
            active: "#f1fa8c".into(),
            blocked: "#ff5555".into(),
            idle: "#50fa7b".into(),
            ink: "#f8f8f2".into(),
            muted: "#b8b8c0".into(),
            faint: "#6272a4".into(),
            accent: "#bd93f9".into(),
            band: "#44475a".into(),
        },
        // Warm, low-blue palette — paper-white ink on a soft brown band,
        // amber accent, terracotta "active". Deliberately the antidote to
        // the cool blue-grey default; opt in with `preset = "warm"`.
        "warm" => Theme {
            active: "#c4643c".into(),  // terracotta — a tool working
            blocked: "#c14a4a".into(), // brick red — blocked/failed
            idle: "#8a9a5b".into(),    // olive — done/idle
            ink: "#ece0cc".into(),     // warm paper white — body text
            muted: "#b39a7d".into(),   // tan — details, notices
            faint: "#8a7355".into(),   // soft brown — continuation bar
            accent: "#e0a458".into(),  // amber — the assistant marker, bullets
            band: "#3a2f28".into(),    // warm dark — strip/code background
        },
        _ => return None,
    })
}

#[cfg(feature = "ratatui")]
mod palette {
    use super::{parse_hex, Theme};
    use ratatui::style::Color;

    /// The resolved theme as terminal colors — what views consume.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Palette {
        pub active: Color,
        pub blocked: Color,
        pub idle: Color,
        pub ink: Color,
        pub muted: Color,
        pub faint: Color,
        pub accent: Color,
        pub band: Color,
    }

    impl From<&Theme> for Palette {
        fn from(t: &Theme) -> Self {
            // Fallback per slot is the default theme's color, so there is no
            // second hand-written RGB table to drift out of sync.
            let d = Theme::default();
            let slot = |value: &str, fallback: &str| {
                let [r, g, b] = parse_hex(value)
                    .or_else(|| parse_hex(fallback))
                    .expect("default theme slots are valid hex");
                Color::Rgb(r, g, b)
            };
            Palette {
                active: slot(&t.active, &d.active),
                blocked: slot(&t.blocked, &d.blocked),
                idle: slot(&t.idle, &d.idle),
                ink: slot(&t.ink, &d.ink),
                muted: slot(&t.muted, &d.muted),
                faint: slot(&t.faint, &d.faint),
                accent: slot(&t.accent, &d.accent),
                band: slot(&t.band, &d.band),
            }
        }
    }

    impl Default for Palette {
        fn default() -> Self {
            Palette::from(&Theme::default())
        }
    }
}
#[cfg(feature = "ratatui")]
pub use palette::Palette;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_default_equals_theme_default() {
        assert_eq!(preset("default"), Some(Theme::default()));
    }
    #[test]
    fn default_is_tokyo_night() {
        assert_eq!(Theme::default(), preset("tokyo-night").unwrap());
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
        for name in [
            "default",
            "tokyo-night",
            "catppuccin",
            "gruvbox",
            "nord",
            "dracula",
            "warm",
        ] {
            assert!(preset(name).is_some(), "missing {name}");
        }
    }
    #[test]
    fn warm_preset_is_low_blue() {
        // The whole point of "warm" is that its ink and accent are not the
        // cool blue-grey of the default — guard against a copy-paste that
        // reintroduces a blue.
        let w = preset("warm").expect("warm exists");
        assert_ne!(w.ink, Theme::default().ink);
        assert_eq!(w.accent, "#e0a458");
    }
    #[test]
    fn resolve_no_preset_is_default() {
        let (theme, warn) = ThemeConfig::default().resolve();
        assert_eq!(theme, Theme::default());
        assert!(warn.is_none());
    }
    #[test]
    fn resolve_known_preset() {
        let tc = ThemeConfig {
            preset: Some("gruvbox".into()),
            ..Default::default()
        };
        let (theme, warn) = tc.resolve();
        assert_eq!(theme, preset("gruvbox").unwrap());
        assert!(warn.is_none());
    }
    #[test]
    fn resolve_unknown_preset_defaults_with_warning() {
        let tc = ThemeConfig {
            preset: Some("typo".into()),
            ..Default::default()
        };
        let (theme, warn) = tc.resolve();
        assert_eq!(theme, Theme::default());
        assert!(warn.as_deref().unwrap().contains("typo"));
    }
    #[test]
    fn resolve_preset_with_override() {
        let tc = ThemeConfig {
            preset: Some("gruvbox".into()),
            blocked: Some("#ff0000".into()),
            ..Default::default()
        };
        let (theme, _) = tc.resolve();
        assert_eq!(theme.blocked, "#ff0000");
        assert_eq!(theme.idle, preset("gruvbox").unwrap().idle);
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
        let tc = ThemeConfig {
            active: Some("notacolor".into()),
            ..Default::default()
        };
        let (theme, warn) = tc.resolve();
        assert_eq!(
            theme.active,
            Theme::default().active,
            "bad color keeps base"
        );
        let w = warn.expect("invalid color should warn");
        assert!(
            w.contains("active"),
            "warning names the offending slot: {w}"
        );
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

#[cfg(all(test, feature = "ratatui"))]
mod palette_tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn palette_from_theme_converts_hex_to_rgb() {
        let p = Palette::from(&Theme::default());
        // tokyo-night, the default palette
        assert_eq!(p.active, Color::Rgb(0xe0, 0xaf, 0x68));
        assert_eq!(p.band, Color::Rgb(0x29, 0x2e, 0x42));
    }

    #[test]
    fn invalid_slot_falls_back_to_default_theme_color() {
        let t = Theme {
            accent: "notacolor".into(),
            ..Theme::default()
        };
        let p = Palette::from(&t);
        assert_eq!(p.accent, Color::Rgb(0x7a, 0xa2, 0xf7));
    }

    #[test]
    fn every_preset_resolves_to_a_palette() {
        for name in [
            "default",
            "tokyo-night",
            "catppuccin",
            "gruvbox",
            "nord",
            "dracula",
        ] {
            let _ = Palette::from(&preset(name).expect("known preset"));
        }
    }
}
