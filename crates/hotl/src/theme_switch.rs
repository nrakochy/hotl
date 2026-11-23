use std::io;
use std::path::Path;

use hotl_theme::{Palette, ThemeConfig};
use hotl_tui::app::{State, TranscriptItem};

pub(crate) const DEFAULT_PRESET: &str = "tokyo-night";

pub(crate) fn active_name(theme: &ThemeConfig) -> String {
    theme
        .preset
        .clone()
        .unwrap_or_else(|| DEFAULT_PRESET.to_string())
}

pub(crate) fn palette_with_preset(theme: &ThemeConfig, name: &str) -> Palette {
    let mut with_preset = theme.clone();
    with_preset.preset = Some(name.to_string());
    Palette::from(&with_preset.resolve().0)
}

pub(crate) fn first_run(config_path: Option<&Path>, data_dir: &Path) -> bool {
    config_path.is_some_and(|path| {
        crate::settings_write::theme_is_unset(path)
            && crate::settings_write::claim_first_palette_ask(data_dir)
    })
}

pub(crate) fn apply(
    state: &mut State,
    theme: &ThemeConfig,
    config_path: Option<&Path>,
    name: &str,
    save: bool,
) -> Palette {
    state.theme_name = name.to_string();
    if save {
        let saved = match config_path {
            Some(path) => crate::settings_write::set_theme_preset(path, name),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no config file location (HOME is not set)",
            )),
        };
        if let Err(e) = saved {
            state.transcript.push(TranscriptItem::Notice {
                text: format!("theme: {name} applied for this session, not saved: {e}").into(),
            });
        }
    }
    palette_with_preset(theme, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_notice(state: &State) -> Option<String> {
        state.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Notice { text } => Some(text.to_string()),
            _ => None,
        })
    }

    fn overridden() -> ThemeConfig {
        ThemeConfig {
            preset: Some("gruvbox".into()),
            accent: Some("#123456".into()),
            ..Default::default()
        }
    }

    #[test]
    fn the_active_name_is_the_preset_or_the_default() {
        assert_eq!(active_name(&ThemeConfig::default()), "tokyo-night");
        assert_eq!(active_name(&overridden()), "gruvbox");
    }

    #[test]
    fn a_preview_keeps_per_slot_overrides_on_top_of_the_new_preset() {
        let nord = palette_with_preset(&overridden(), "nord");
        let plain_nord = Palette::from(&hotl_theme::preset("nord").unwrap());
        assert_eq!(nord.accent, ratatui::style::Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(nord.ink, plain_nord.ink);
        assert_ne!(nord.accent, plain_nord.accent);
    }

    #[test]
    fn previewing_the_original_restores_the_exact_loaded_palette() {
        let theme = overridden();
        let loaded = Palette::from(&theme.resolve().0);
        assert_eq!(palette_with_preset(&theme, "gruvbox"), loaded);
    }

    #[test]
    fn a_preview_names_the_theme_and_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let mut state = State::new(false, "m".into());
        let palette = apply(&mut state, &overridden(), Some(&path), "nord", false);
        assert_eq!(palette, palette_with_preset(&overridden(), "nord"));
        assert_eq!(state.theme_name, "nord");
        assert!(!path.exists());
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn a_set_saves_the_preset() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        std::fs::write(&path, "# mine\n[settings.theme]\naccent = \"#123456\"\n").unwrap();
        let mut state = State::new(false, "m".into());
        apply(&mut state, &overridden(), Some(&path), "dracula", true);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("preset = \"dracula\""), "{text}");
        assert!(text.contains("# mine"), "{text}");
        assert!(text.contains("accent = \"#123456\""), "{text}");
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn a_failed_save_still_applies_and_says_why() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        std::fs::write(&path, "not = [valid").unwrap();
        let mut state = State::new(false, "m".into());
        let palette = apply(
            &mut state,
            &ThemeConfig::default(),
            Some(&path),
            "nord",
            true,
        );
        assert_eq!(palette, Palette::from(&hotl_theme::preset("nord").unwrap()));
        assert_eq!(state.theme_name, "nord");
        let notice = last_notice(&state).expect("a notice");
        assert!(
            notice.starts_with(
                "theme: nord applied for this session, not saved: config.toml is not valid TOML"
            ),
            "{notice}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid");
    }

    #[test]
    fn a_set_with_no_config_path_applies_and_says_why() {
        let mut state = State::new(false, "m".into());
        apply(&mut state, &ThemeConfig::default(), None, "warm", true);
        assert_eq!(state.theme_name, "warm");
        assert_eq!(
            last_notice(&state).as_deref(),
            Some("theme: warm applied for this session, not saved: no config file location (HOME is not set)")
        );
    }

    #[test]
    fn first_run_needs_an_unset_theme_then_claims_the_marker_once() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        let data = d.path().join("data");
        assert!(first_run(Some(&path), &data));
        assert!(!first_run(Some(&path), &data));
    }

    #[test]
    fn a_configured_theme_never_asks_and_never_writes_the_marker() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        std::fs::write(&path, "[settings.theme]\npreset = \"nord\"\n").unwrap();
        let data = d.path().join("data");
        assert!(!first_run(Some(&path), &data));
        assert!(!data.join("palette-asked").exists());
    }

    #[test]
    fn no_config_path_never_asks() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        assert!(!first_run(None, &data));
        assert!(!data.join("palette-asked").exists());
    }
}
