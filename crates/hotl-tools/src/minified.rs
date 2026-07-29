//! The guarded-IO glue for `read --minified` / `edit --minified`.
//!
//! `hotl-minify` is pure text-in/text-out and cannot open a file (D-B2). This
//! module opens through `fsguard`, hands the bytes over, and writes back through
//! the same plumbing the plain `edit` uses.
//!
//! The config lives here rather than in `hotl-minify` because it survives the
//! `minify` feature being off: the flag governs the *parsing stack*, not whether
//! the harness can read a `[minify]` section.

/// The `[minify]` section. Both keys default on.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MinifyConfig {
    #[serde(default = "yes")]
    pub enable: bool,
    /// Comments are meaning; stripping them is the lossy mode, so keeping is the
    /// default. This inverts vix's choice deliberately (D-B6).
    #[serde(default = "yes")]
    pub keep_comments: bool,
}

fn yes() -> bool {
    true
}

impl Default for MinifyConfig {
    fn default() -> Self {
        toml::from_str("").expect("an empty table is all-defaults")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minify_config_defaults_to_enabled_with_comments_kept() {
        let cfg: MinifyConfig = toml::from_str("").unwrap();
        assert!(cfg.enable && cfg.keep_comments);
        let cfg: MinifyConfig = toml::from_str("enable = false\nkeep_comments = false").unwrap();
        assert!(!cfg.enable && !cfg.keep_comments);
    }

    #[test]
    fn an_unknown_key_does_not_sink_the_section() {
        // Forward-compat: a config written by a newer hotl still parses.
        let cfg: MinifyConfig = toml::from_str("enable = false\nfuture_key = 3").unwrap();
        assert!(!cfg.enable && cfg.keep_comments);
    }

    #[test]
    fn the_default_impl_agrees_with_serde() {
        let d = MinifyConfig::default();
        let s: MinifyConfig = toml::from_str("").unwrap();
        assert_eq!((d.enable, d.keep_comments), (s.enable, s.keep_comments));
    }
}
