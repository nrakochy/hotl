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

/// The whole feature, when the parsing stack is compiled out (Decision D1).
/// Kept as a module rather than scattered `cfg`s so the on and off shapes of the
/// API sit side by side.
#[cfg(not(feature = "minify"))]
mod disabled {
    use super::MinifyConfig;
    use crate::ToolOutcome;
    use serde_json::Value;
    use std::path::Path;

    fn refuse(verb: &str) -> crate::builtins::ToolResult {
        Err(ToolOutcome::err(format!(
            "this build has no minify support, so `minified: true` cannot be honored. \
             Re-issue the {verb} without `minified`."
        )))
    }

    pub(crate) async fn read_minified_in(
        _root: &Path,
        _input: &Value,
        _cfg: &MinifyConfig,
    ) -> crate::builtins::ToolResult {
        refuse("read")
    }

    pub(crate) async fn edit_minified_in(
        _root: &Path,
        _extras: &[std::path::PathBuf],
        _input: &Value,
        _cfg: &MinifyConfig,
    ) -> crate::builtins::ToolResult {
        refuse("edit")
    }

    /// Whether the schema advertises the `minified` arg. Never advertise what
    /// the build cannot do.
    pub(crate) fn available() -> bool {
        false
    }
}

#[cfg(not(feature = "minify"))]
pub(crate) use disabled::{available, edit_minified_in, read_minified_in};

#[cfg(feature = "minify")]
mod enabled {
    use super::MinifyConfig;
    use crate::builtins::{self, ToolResult};
    use crate::ToolOutcome;
    use hotl_minify::{language_for_path, minify, Lang};
    use serde_json::Value;
    use std::io::Read;
    use std::path::Path;

    pub(crate) fn available() -> bool {
        true
    }

    /// A minified read serves the whole file or none of it.
    ///
    /// Whole-file-only is a correctness decision, not a simplification (D-B4):
    /// `offset`/`limit` are *raw-file line numbers*, and the minified view has no
    /// lines the model can count, so paging in that coordinate system would be
    /// asking the model to name positions it cannot see.
    pub(crate) async fn read_minified_in(
        root: &Path,
        input: &Value,
        cfg: &MinifyConfig,
    ) -> ToolResult {
        let path = builtins::str_arg(input, "path")?;
        if input.get("offset").is_some() || input.get("limit").is_some() {
            return Err(ToolOutcome::err(
                "minified reads return the whole file, so `offset`/`limit` do not apply \
                 (the view has no line numbers to page by). For paged access use a plain read \
                 (omit `minified`) with offset/limit.",
            ));
        }
        let mut file = builtins::open_for_read(root, path)?;
        let mut source = String::new();
        file.read_to_string(&mut source).map_err(|e| {
            ToolOutcome::err(format!(
                "Could not read `{path}` as text: {e}. Use a plain read, or `bash` for binary \
                 files."
            ))
        })?;
        match minified_view(path, &source, cfg) {
            Ok(body) => Ok(ToolOutcome::ok(body)),
            // Every failure degrades to the plain view rather than to nothing:
            // an imperfect minifier costs savings, never access.
            Err(reason) => plain_with_note(root, path, input, &reason).await,
        }
    }

    /// The served text: a header the model can act on, the view, and what it
    /// saved.
    fn minified_view(path: &str, source: &str, cfg: &MinifyConfig) -> Result<String, String> {
        let lang = gate(path, cfg)?;
        let m = minify(lang, source, cfg.keep_comments).map_err(|e| e.to_string())?;
        if m.text.len() > builtins::READ_MAX_BYTES {
            return Err(format!(
                "the minified view is {} bytes, over the {} byte cap; minified reads are \
                 whole-file only",
                m.text.len(),
                builtins::READ_MAX_BYTES
            ));
        }
        let saved = hotl_context::tokens::estimate_text(source)
            .saturating_sub(hotl_context::tokens::estimate_text(&m.text));
        let comments = if cfg.keep_comments {
            "kept"
        } else {
            "stripped"
        };
        Ok(format!(
            "[minified view of {path}: formatting whitespace removed, structure intact; \
             line numbers unavailable — pass minified:true to edit, or use a plain read for \
             offset/limit]\n{}\n[minified view: {} -> {} bytes (~{saved} estimated tokens \
             saved); comments {comments}]",
            m.text,
            source.len(),
            m.text.len()
        ))
    }

    /// Why this file cannot be minified, phrased for the trailer.
    fn gate(path: &str, cfg: &MinifyConfig) -> Result<Lang, String> {
        if !cfg.enable {
            return Err("minified reads are disabled by config ([minify] enable = false)".into());
        }
        language_for_path(path).ok_or_else(|| {
            "no grammar for this file type (supported: .rs, .go, .py, .js/.mjs/.cjs/.jsx, \
             .ts/.mts/.cts, .tsx)"
                .into()
        })
    }

    /// The raw fallback: the plain view plus a note naming the reason, so a
    /// silent degradation is still a visible one.
    async fn plain_with_note(root: &Path, path: &str, input: &Value, reason: &str) -> ToolResult {
        let mut out = builtins::read_in(root, input).await?;
        out.content.push_str(&format!(
            "\n[minified unavailable for `{path}`: {reason}; served the plain view]"
        ));
        Ok(out)
    }

    /// Edit through the minified view: match in the minified domain, project the
    /// match back to source bytes, splice into the **raw** file.
    ///
    /// The disk file keeps its comments, indentation and formatting — it is never
    /// written in minified form. What makes that safe is the position map's
    /// invariant: a segment's minified bytes are a verbatim copy of its source
    /// bytes, so the projection is arithmetic rather than a guess.
    ///
    /// Matching is exact-only (D-B5). The domain is already whitespace-normalized,
    /// so the plain matcher's tolerant levels have nothing left to tolerate and
    /// would only blur uniqueness — a tolerant match across two near-identical
    /// functions is precisely the false positive the exact rule prevents.
    pub(crate) async fn edit_minified_in(
        root: &Path,
        extras: &[std::path::PathBuf],
        input: &Value,
        cfg: &MinifyConfig,
    ) -> ToolResult {
        let path = builtins::str_arg(input, "path")?;
        let old = builtins::str_arg(input, "old_string")?;
        let new = builtins::str_arg(input, "new_string")?;
        if old.is_empty() {
            return Err(ToolOutcome::err(
                "`old_string` is empty. Use `write` to create a file, or provide the exact text \
                 to replace.",
            ));
        }
        let lang = gate(path, cfg).map_err(|reason| {
            ToolOutcome::err(format!(
                "minified edits are not available for `{path}` ({reason}). Use a plain edit \
                 (omit `minified`)."
            ))
        })?;
        let target = builtins::write_target(root, extras, path);
        let content = builtins::read_guarded_to_string(root, &target, path)?;
        let m = minify(lang, &content, cfg.keep_comments).map_err(|e| {
            ToolOutcome::err(format!(
                "`{path}` cannot be served as a minified view ({e}), so a minified edit has \
                 nothing to match against. Use a plain edit (omit `minified`)."
            ))
        })?;
        let (start, end) = locate(&m.text, old, path)?;
        let (src_start, src_end) = m.project_span(start, end).map_err(|_| {
            ToolOutcome::err(format!(
                "the matched text in `{path}` is only formatting the minifier inserted, which \
                 exists nowhere in the file. Include a real token in `old_string`."
            ))
        })?;
        let updated = format!("{}{new}{}", &content[..src_start], &content[src_end..]);
        // Same re-read-and-compare-then-write sequence as the plain edit, so
        // both routes inherit one concurrency posture rather than two (D-B10).
        let now = builtins::read_guarded_to_string(root, &target, path)?;
        if now != content {
            return Err(ToolOutcome::err(format!(
                "`{path}` changed on disk between the match and the write, so the edit was not \
                 applied (nothing was lost). Re-read the file and re-issue the edit against the \
                 current text."
            )));
        }
        builtins::write_guarded(root, &target, path, updated.as_bytes())?;
        Ok(ToolOutcome::ok(format!(
            "Edited {path} (minified match projected to source bytes; the file's formatting was \
             preserved)."
        )))
    }

    /// The one match level: exact, exactly once. Error wordings mirror the plain
    /// edit's so the model learns one vocabulary.
    fn locate(text: &str, old: &str, path: &str) -> Result<(usize, usize), ToolOutcome> {
        let mut hits = text.match_indices(old);
        let first = hits.next().ok_or_else(|| {
            ToolOutcome::err(format!(
                "`old_string` was not found in the minified view of `{path}`. Minified matching \
                 is exact — read the file with minified:true and copy the text from that view, \
                 not from a plain read."
            ))
        })?;
        if hits.next().is_some() {
            let n = text.matches(old).count();
            return Err(ToolOutcome::err(format!(
                "`old_string` matches {n} places in `{path}`. Add surrounding tokens so it \
                 matches exactly once."
            )));
        }
        Ok((first.0, first.0 + old.len()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::fsguard;
        use serde_json::json;

        fn write(root: &Path, rel: &str, body: &str) {
            std::fs::write(root.join(rel), body).unwrap();
        }

        #[tokio::test]
        async fn a_minified_read_returns_the_token_stream_with_a_savings_trailer() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(
                &root,
                "src/big.rs",
                "fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n",
            );
            let out = read_minified_in(
                &root,
                &json!({"path": "src/big.rs", "minified": true}),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            assert!(out.content.contains("fn add"));
            assert!(!out.content.contains("\n    "), "indentation must be gone");
            assert!(
                out.content.contains("[minified view"),
                "trailer present: {}",
                out.content
            );
            assert!(
                !out.content.contains("     1\t"),
                "no line-number prefixes: {}",
                out.content
            );
        }

        #[tokio::test]
        async fn an_unsupported_extension_falls_back_to_the_plain_read_with_a_note() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(&root, "notes.md", "# hi\n");
            let out = read_minified_in(
                &root,
                &json!({"path": "notes.md", "minified": true}),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            assert!(out.content.contains("     1\t# hi"), "plain cat -n output");
            assert!(
                out.content.contains("[minified unavailable"),
                "the model learns why: {}",
                out.content
            );
        }

        #[tokio::test]
        async fn a_file_with_syntax_errors_falls_back_to_the_plain_read() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(&root, "src/broken.rs", "fn broken( {\n");
            let out = read_minified_in(
                &root,
                &json!({"path": "src/broken.rs", "minified": true}),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            assert!(out.content.contains("[minified unavailable"));
            assert!(out.content.contains("does not parse"), "{}", out.content);
        }

        #[tokio::test]
        async fn offset_and_limit_are_refused_in_minified_mode_with_advice() {
            let (_o, root, _home) = fsguard::tests::fixture();
            for extra in [json!({"offset": 5}), json!({"limit": 5})] {
                let mut input = json!({"path": "src/lib.rs", "minified": true});
                let (k, v) = extra.as_object().unwrap().iter().next().unwrap();
                input[k] = v.clone();
                let err = read_minified_in(&root, &input, &MinifyConfig::default())
                    .await
                    .unwrap_err();
                assert!(
                    err.content.contains("plain read"),
                    "error tells the model the way out: {}",
                    err.content
                );
            }
        }

        #[tokio::test]
        async fn disabling_the_feature_by_config_serves_the_plain_view() {
            let (_o, root, _home) = fsguard::tests::fixture();
            let cfg = MinifyConfig {
                enable: false,
                keep_comments: true,
            };
            let out = read_minified_in(&root, &json!({"path": "src/lib.rs"}), &cfg)
                .await
                .unwrap();
            assert!(out.content.contains("     1\tfn a() {}"), "{}", out.content);
            assert!(
                out.content.contains("disabled by config"),
                "{}",
                out.content
            );
        }

        #[tokio::test]
        async fn stripping_comments_saves_more_than_keeping_them() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(
                &root,
                "src/c.rs",
                "/// doc\nfn f() -> u32 {\n    // inner\n    1\n}\n",
            );
            let input = json!({"path": "src/c.rs", "minified": true});
            let kept = read_minified_in(&root, &input, &MinifyConfig::default())
                .await
                .unwrap();
            let stripped = read_minified_in(
                &root,
                &input,
                &MinifyConfig {
                    enable: true,
                    keep_comments: false,
                },
            )
            .await
            .unwrap();
            assert!(kept.content.contains("/// doc"), "{}", kept.content);
            assert!(
                !stripped.content.contains("/// doc"),
                "{}",
                stripped.content
            );
            assert!(stripped.content.len() < kept.content.len());
        }

        #[tokio::test]
        async fn a_minified_read_refuses_to_leave_the_workspace_through_a_symlink() {
            // The guard is shared with the plain read, and this pins that it is
            // actually on the minified path too.
            let (_o, root, home) = fsguard::tests::fixture();
            std::os::unix::fs::symlink(home.join("id_rsa"), root.join("src/link.rs")).unwrap();
            let err = read_minified_in(
                &root,
                &json!({"path": "src/link.rs", "minified": true}),
                &MinifyConfig::default(),
            )
            .await
            .unwrap_err();
            assert!(!err.content.contains("PRIVATE KEY"), "{}", err.content);
        }

        #[tokio::test]
        async fn a_minified_edit_splices_exact_bytes_and_preserves_all_formatting() {
            let (_o, root, _home) = fsguard::tests::fixture();
            let src = "fn add(a: u32, b: u32) -> u32 {\n    // sum\n    a + b\n}\n";
            write(&root, "src/m.rs", src);
            let out = edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/m.rs", "minified": true,
                    "old_string": "a+b", "new_string": "a.wrapping_add(b)"
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            assert!(out.content.starts_with("Edited"));
            let after = std::fs::read_to_string(root.join("src/m.rs")).unwrap();
            assert_eq!(
                after, "fn add(a: u32, b: u32) -> u32 {\n    // sum\n    a.wrapping_add(b)\n}\n",
                "comment and indentation untouched; only the matched bytes changed"
            );
        }

        #[tokio::test]
        async fn an_ambiguous_minified_match_is_refused_with_the_count() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(
                &root,
                "src/m.rs",
                "fn f() -> u32 { 1 }\nfn g() -> u32 { 1 }\n",
            );
            let err = edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/m.rs", "minified": true,
                    "old_string": "->u32{1}", "new_string": "->u64{1}"
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap_err();
            assert!(err.content.contains("matches 2 places"), "{}", err.content);
        }

        #[tokio::test]
        async fn a_miss_tells_the_model_to_take_a_minified_read_first() {
            let (_o, root, _home) = fsguard::tests::fixture();
            let err = edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/lib.rs", "minified": true,
                    "old_string": "nope", "new_string": "x"
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap_err();
            assert!(
                err.content.contains("minified:true"),
                "the way out is named: {}",
                err.content
            );
        }

        #[tokio::test]
        async fn a_match_covering_only_invented_formatting_is_refused() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(&root, "src/m.py", "def f(a):\n    return a\n");
            let err = edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/m.py", "minified": true,
                    "old_string": "\n ", "new_string": "\n  "
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap_err();
            assert!(
                err.content
                    .contains("only formatting the minifier inserted"),
                "{}",
                err.content
            );
        }

        #[tokio::test]
        async fn a_span_over_several_tokens_replaces_the_source_region_between_them() {
            // The projected range is a superset in source space: the newline and
            // indentation *between* the matched tokens go too. That is what makes
            // the replacement one contiguous splice.
            let (_o, root, _home) = fsguard::tests::fixture();
            write(
                &root,
                "src/m.rs",
                "fn f() -> u32 {\n    let x = 1;\n    x\n}\n",
            );
            edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/m.rs", "minified": true,
                    "old_string": "let x=1;x", "new_string": "1"
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            let after = std::fs::read_to_string(root.join("src/m.rs")).unwrap();
            assert_eq!(after, "fn f() -> u32 {\n    1\n}\n");
        }

        #[tokio::test]
        async fn an_edit_quoted_from_a_comment_stripped_view_still_lands_correctly() {
            // The view the model quoted from had no comments; the file does. The
            // segments are the same source bytes either way.
            let (_o, root, _home) = fsguard::tests::fixture();
            write(
                &root,
                "src/m.rs",
                "/// doc\nfn f() -> u32 {\n    // why\n    41\n}\n",
            );
            let cfg = MinifyConfig {
                enable: true,
                keep_comments: false,
            };
            edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/m.rs", "minified": true,
                    "old_string": "41", "new_string": "42"
                }),
                &cfg,
            )
            .await
            .unwrap();
            let after = std::fs::read_to_string(root.join("src/m.rs")).unwrap();
            assert_eq!(after, "/// doc\nfn f() -> u32 {\n    // why\n    42\n}\n");
        }

        #[tokio::test]
        async fn a_utf8_splice_lands_on_character_boundaries() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(
                &root,
                "src/m.rs",
                "fn f() -> &'static str {\n    \"héllo wörld\"\n}\n",
            );
            edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/m.rs", "minified": true,
                    "old_string": "\"héllo wörld\"", "new_string": "\"日本語\""
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            let after = std::fs::read_to_string(root.join("src/m.rs")).unwrap();
            assert_eq!(after, "fn f() -> &'static str {\n    \"日本語\"\n}\n");
        }

        #[tokio::test]
        async fn a_b_a_round_trips_to_a_byte_identical_file() {
            let (_o, root, _home) = fsguard::tests::fixture();
            let src = "fn add(a: u32, b: u32) -> u32 {\n    // sum\n    a + b\n}\n";
            write(&root, "src/m.rs", src);
            let edit = |old: &'static str, new: &'static str| {
                let root = root.clone();
                async move {
                    edit_minified_in(
                        &root,
                        &[],
                        &json!({
                            "path": "src/m.rs", "minified": true,
                            "old_string": old, "new_string": new
                        }),
                        &MinifyConfig::default(),
                    )
                    .await
                    .unwrap();
                }
            };
            edit("a+b", "b+a").await;
            edit("b+a", "a + b").await;
            assert_eq!(
                std::fs::read_to_string(root.join("src/m.rs")).unwrap(),
                src,
                "A -> B -> A is byte-identical"
            );
        }

        #[tokio::test]
        async fn an_unsupported_extension_refuses_the_edit_rather_than_guessing() {
            let (_o, root, _home) = fsguard::tests::fixture();
            write(&root, "notes.md", "# hi\n");
            let err = edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "notes.md", "minified": true,
                    "old_string": "hi", "new_string": "bye"
                }),
                &MinifyConfig::default(),
            )
            .await
            .unwrap_err();
            assert!(err.content.contains("plain edit"), "{}", err.content);
            assert_eq!(
                std::fs::read_to_string(root.join("notes.md")).unwrap(),
                "# hi\n",
                "a refused edit writes nothing"
            );
        }

        #[tokio::test]
        async fn a_minified_edit_refuses_to_follow_a_symlink_out_of_the_workspace() {
            let (_o, root, home) = fsguard::tests::fixture();
            let secret = home.join("secret.rs");
            std::fs::write(&secret, "fn s() -> u32 { 1 }\n").unwrap();
            std::os::unix::fs::symlink(&secret, root.join("src/link.rs")).unwrap();
            let _ = edit_minified_in(
                &root,
                &[],
                &json!({
                    "path": "src/link.rs", "minified": true,
                    "old_string": "1", "new_string": "2"
                }),
                &MinifyConfig::default(),
            )
            .await;
            assert_eq!(
                std::fs::read_to_string(&secret).unwrap(),
                "fn s() -> u32 { 1 }\n",
                "the link's target was not touched"
            );
        }

        #[tokio::test]
        async fn a_view_over_the_byte_cap_falls_back_to_the_paged_plain_read() {
            let (_o, root, _home) = fsguard::tests::fixture();
            let body: String = (0..9000)
                .map(|i| format!("fn f{i}(a: u32) -> u32 {{\n    a + {i}\n}}\n"))
                .collect();
            write(&root, "src/huge.rs", &body);
            let out = read_minified_in(
                &root,
                &json!({"path": "src/huge.rs", "minified": true}),
                &MinifyConfig::default(),
            )
            .await
            .unwrap();
            assert!(out.content.contains("over the"), "{}", &out.content[..200]);
            assert!(out.content.contains("     1\tfn f0"), "paged plain view");
        }
    }
}

#[cfg(feature = "minify")]
pub(crate) use enabled::{available, edit_minified_in, read_minified_in};

#[cfg(test)]
mod tests {
    use super::*;

    /// INVARIANT: the schema advertises `minified` only when the build can honor
    /// it (Decision D1). A model that sees the arg can always use it.
    #[test]
    fn the_read_schema_advertises_minified_exactly_when_the_build_supports_it() {
        use crate::Tool;
        let schema = crate::ReadTool::default().schema();
        let advertised = schema["properties"].get("minified").is_some();
        assert_eq!(advertised, available());
        assert_eq!(advertised, cfg!(feature = "minify"));
    }

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
