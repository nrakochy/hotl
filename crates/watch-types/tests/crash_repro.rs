// Regression: a 6-byte, non-char-boundary color must not panic and must warn.
// This is the exact input that crashed the TUI via view::hex before the fix.
use watch_types::{parse_hex, HotlConfig};

#[test]
fn multibyte_color_config_does_not_panic_and_warns() {
    let toml = "[settings.theme]\nactive = \"aéaé\"\n"; // 6 bytes, 4 chars
    let cfg = HotlConfig::parse(toml);
    // The exact operation that used to panic in view::hex:
    assert_eq!(
        parse_hex(cfg.settings.theme.active.as_deref().unwrap()),
        None
    );
    let (theme, warn) = cfg.settings.theme.resolve();
    assert_eq!(
        theme.active,
        watch_types::Theme::default().active,
        "bad color keeps base"
    );
    assert!(
        warn.unwrap().contains("active"),
        "invalid color is reported"
    );
}
