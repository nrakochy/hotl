use std::io::{self, Write};
use std::path::{Path, PathBuf};

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub(crate) fn theme_is_unset(config_path: &Path) -> bool {
    let text = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(e) => return e.kind() == io::ErrorKind::NotFound,
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    match doc.get("settings") {
        None => true,
        Some(settings) => match settings.as_table_like() {
            Some(t) => t.get("theme").is_none(),
            None => false,
        },
    }
}

pub(crate) fn set_theme_preset(config_path: &Path, name: &str) -> io::Result<()> {
    let target = dunce::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    if std::fs::metadata(&target).is_ok_and(|m| m.permissions().readonly()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "config.toml is read-only",
        ));
    }
    let original = match std::fs::read_to_string(&target) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc: toml_edit::DocumentMut = original
        .parse()
        .map_err(|e| invalid(format!("config.toml is not valid TOML: {e}")))?;

    let settings_item = doc.entry("settings").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    let settings_inline = settings_item.is_inline_table();
    let settings_dotted = settings_item.as_table().is_some_and(|t| t.is_dotted());
    let settings = settings_item
        .as_table_like_mut()
        .ok_or_else(|| invalid("settings in config.toml is not a table"))?;
    if settings.get("theme").is_none() {
        let item = if settings_inline {
            toml_edit::Item::Value(toml_edit::Value::InlineTable(Default::default()))
        } else {
            let mut t = toml_edit::Table::new();
            t.set_dotted(settings_dotted);
            toml_edit::Item::Table(t)
        };
        settings.insert("theme", item);
    }
    let theme = settings
        .get_mut("theme")
        .and_then(|t| t.as_table_like_mut())
        .ok_or_else(|| invalid("settings.theme in config.toml is not a table"))?;
    if let Some(existing) = theme.get("preset") {
        if existing.as_str().is_none() {
            return Err(invalid(
                "settings.theme.preset in config.toml is not a string",
            ));
        }
    }
    if let Some(v) = theme.get_mut("preset").and_then(|i| i.as_value_mut()) {
        let decor = v.decor().clone();
        *v = toml_edit::Value::from(name);
        *v.decor_mut() = decor;
    } else {
        theme.insert(
            "preset",
            toml_edit::Item::Value(toml_edit::Value::from(name)),
        );
    }

    let mut out = doc.to_string();
    if original.contains("\r\n") && !original.replace("\r\n", "").contains('\n') {
        out = out.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    let check: toml_edit::DocumentMut = out
        .parse()
        .map_err(|e| invalid(format!("edited config.toml is not valid TOML: {e}")))?;
    let saved = check
        .get("settings")
        .and_then(|s| s.as_table_like())
        .and_then(|s| s.get("theme"))
        .and_then(|t| t.as_table_like())
        .and_then(|t| t.get("preset"))
        .and_then(|p| p.as_str());
    if saved != Some(name) {
        return Err(invalid("could not set settings.theme.preset"));
    }
    write_atomic(&target, &out)
}

pub(crate) fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub(crate) fn claim_first_palette_ask(data_dir: &Path) -> bool {
    if std::fs::create_dir_all(data_dir).is_err() {
        return false;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(data_dir.join("palette-asked"))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &tempfile::TempDir, text: &str) -> PathBuf {
        let p = dir.path().join("config.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    fn set(text: &str, name: &str) -> String {
        let d = tempfile::tempdir().unwrap();
        let p = cfg(&d, text);
        set_theme_preset(&p, name).unwrap();
        std::fs::read_to_string(&p).unwrap()
    }

    fn preset_of(text: &str) -> Option<String> {
        let v: toml::Value = toml::from_str(text).unwrap();
        v.get("settings")?
            .get("theme")?
            .get("preset")?
            .as_str()
            .map(String::from)
    }

    #[test]
    fn marker_first_true_second_false() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("nested").join("data");
        assert!(claim_first_palette_ask(&dir));
        assert!(!claim_first_palette_ask(&dir));
        assert!(dir.join("palette-asked").exists());
    }

    #[test]
    fn marker_false_when_dir_unusable() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(!claim_first_palette_ask(&file.join("sub")));
    }

    #[test]
    fn marker_race_gives_exactly_one_true() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().to_path_buf();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || claim_first_palette_ask(&dir))
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|w| *w)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn unset_when_file_missing() {
        let d = tempfile::tempdir().unwrap();
        assert!(theme_is_unset(&d.path().join("nope.toml")));
    }

    #[test]
    fn unset_when_no_theme_table() {
        let d = tempfile::tempdir().unwrap();
        assert!(theme_is_unset(&cfg(&d, "")));
        assert!(theme_is_unset(&cfg(
            &d,
            "[settings]\ndensity = \"compact\"\n"
        )));
    }

    #[test]
    fn theme_exists_in_every_toml_spelling() {
        let d = tempfile::tempdir().unwrap();
        for text in [
            "[settings.theme]\npreset = \"nord\"\n",
            "[settings.theme]\nbg = \"#000000\"\n",
            "settings = { theme = { preset = \"nord\" } }\n",
            "[settings]\ntheme.preset = \"nord\"\n",
            "settings.theme.preset = \"nord\"\n",
            "[settings]\ntheme = {}\n",
        ] {
            assert!(!theme_is_unset(&cfg(&d, text)), "{text}");
        }
    }

    #[test]
    fn unreadable_or_broken_file_is_not_unset() {
        let d = tempfile::tempdir().unwrap();
        assert!(!theme_is_unset(&cfg(&d, "[settings\nbroken")));
        assert!(!theme_is_unset(&cfg(&d, "settings = 3\n")));
        assert!(!theme_is_unset(d.path()));
    }

    #[test]
    fn creates_dir_and_file_when_missing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("hotl").join("config.toml");
        set_theme_preset(&p, "nord").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[settings.theme]\npreset = \"nord\"\n"
        );
    }

    #[test]
    fn adds_settings_to_file_without_it() {
        let out = set("[plugins]\nx = 1\n", "gruvbox");
        assert!(out.starts_with("[plugins]\nx = 1\n"));
        assert_eq!(preset_of(&out).as_deref(), Some("gruvbox"));
    }

    #[test]
    fn comments_and_overrides_survive() {
        let text = "# top\n[settings]\ndensity = \"compact\" # keep\n\n# theme below\n[settings.theme]\n# slot\nbg = \"#101010\"  # mine\npreset = \"nord\"   # old\nfg = \"#eeeeee\"\n\n[other]\na = 1\n";
        let out = set(text, "dracula");
        assert_eq!(out, text.replace("\"nord\"", "\"dracula\""));
    }

    #[test]
    fn theme_table_without_preset_gets_one() {
        let text = "[settings.theme]\n# c\nbg = \"#101010\"\n";
        let out = set(text, "warm");
        assert!(out.starts_with(text.trim_end_matches('\n')));
        assert_eq!(preset_of(&out).as_deref(), Some("warm"));
        assert!(out.contains("bg = \"#101010\""));
    }

    #[test]
    fn crlf_file_stays_crlf() {
        let text = "# c\r\n[settings.theme]\r\npreset = \"nord\"\r\nbg = \"#101010\"\r\n";
        let out = set(text, "warm");
        assert_eq!(out, text.replace("nord", "warm"));
        let out = set("# c\r\n[plugins]\r\nx = 1\r\n", "warm");
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert!(out.starts_with("# c\r\n[plugins]\r\nx = 1\r\n"));
        assert_eq!(preset_of(&out).as_deref(), Some("warm"));
    }

    #[test]
    fn comment_above_preset_survives() {
        let text = "[settings.theme]\n# my favourite\npreset = \"nord\"\nbg = \"#101010\"\n";
        assert_eq!(set(text, "warm"), text.replace("nord", "warm"));
        let crlf = text.replace('\n', "\r\n");
        assert_eq!(set(&crlf, "warm"), crlf.replace("nord", "warm"));
    }

    #[test]
    fn indented_spaced_preset_keeps_layout() {
        let text = "[settings.theme]\n  preset   =   \"nord\"  # c\n  bg = \"#101010\"\n";
        assert_eq!(set(text, "warm"), text.replace("nord", "warm"));
        let crlf = text.replace('\n', "\r\n");
        assert_eq!(set(&crlf, "warm"), crlf.replace("nord", "warm"));
    }

    #[test]
    fn comment_above_dotted_preset_survives() {
        let text = "[settings]\n# pick one\ntheme.preset = \"nord\"\ntheme.bg = \"#101010\"\n";
        assert_eq!(set(text, "warm"), text.replace("nord", "warm"));
        let crlf = text.replace('\n', "\r\n");
        assert_eq!(set(&crlf, "warm"), crlf.replace("nord", "warm"));
    }

    #[test]
    fn inline_settings_table() {
        let text = "settings = { density = \"compact\", theme = { preset = \"nord\", bg = \"#101010\" } } # t\n";
        let out = set(text, "warm");
        assert_eq!(out, text.replace("\"nord\"", "\"warm\""));
        let out = set("settings = { density = \"compact\" }\n", "warm");
        assert_eq!(preset_of(&out).as_deref(), Some("warm"));
        assert!(out.contains("density = \"compact\""));
    }

    #[test]
    fn dotted_theme_key() {
        let text =
            "[settings]\ndensity = \"compact\"\ntheme.preset = \"nord\"\ntheme.bg = \"#101010\"\n";
        let out = set(text, "warm");
        assert_eq!(out, text.replace("\"nord\"", "\"warm\""));
        let out = set("[settings]\ntheme.bg = \"#101010\"\n", "warm");
        assert_eq!(preset_of(&out).as_deref(), Some("warm"));
        assert!(out.contains("bg = \"#101010\""));
        let out = set("settings.density = \"compact\"\n", "warm");
        assert_eq!(preset_of(&out).as_deref(), Some("warm"));
        assert!(out.contains("density = \"compact\""));
    }

    #[test]
    fn non_table_settings_is_an_error_and_file_untouched() {
        let d = tempfile::tempdir().unwrap();
        for text in ["settings = 3\n", "[settings]\ntheme = 3\n"] {
            let p = cfg(&d, text);
            assert!(set_theme_preset(&p, "nord").is_err());
            assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        }
    }

    #[test]
    fn unparseable_file_is_an_error_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        let text = "[settings\nbroken = \n";
        let p = cfg(&d, text);
        assert!(set_theme_preset(&p, "nord").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
    }

    #[test]
    fn no_temp_file_left_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = cfg(&d, "");
        set_theme_preset(&p, "nord").unwrap();
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn read_only_config_is_an_error_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        let text = "[settings.theme]\npreset = \"nord\"\n";
        let p = cfg(&d, text);
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&p, perms).unwrap();
        let err = set_theme_preset(&p, "warm").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
    }

    #[test]
    fn non_string_preset_is_an_error_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        for text in [
            "[settings.theme.preset]\na = 1\n",
            "[settings.theme]\npreset = [\"a\", \"b\"]\n",
            "[settings.theme]\npreset = 3\n",
            "[settings.theme]\npreset = { a = 1 }\n",
        ] {
            let p = cfg(&d, text);
            let err = set_theme_preset(&p, "warm").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{text}");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        }
    }
}
