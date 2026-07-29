//! Token-stream minification with a byte-exact position map.
//!
//! Pure text in → text out. This crate does no IO *by design*: `hotl-tools`
//! owns the `fsguard` containment boundary and it is `pub(crate)` there, so a
//! minifier that could open files would have to widen a security boundary for
//! a convenience (D-B2). The caller opens the file through the guard and hands
//! the bytes over.
//!
//! The mechanism: parse with the language grammar, collect leaf tokens, re-join
//! them with the smallest separators that preserve meaning, and record one
//! `Segment` per emitted token so a range of the minified text can be mapped
//! back to the range of source bytes it came from.

mod join;
mod lang;
mod tokens;

pub use lang::{language_for_path, Lang};
pub use tokens::{extract, parses_clean, Extraction, Token, TokenKind};

/// A minified view plus the map back to the source it came from.
#[derive(Debug)]
pub struct Minified {
    /// The minified view. Handed to the model verbatim.
    pub text: String,
    /// One entry per emitted token, sorted by `out_start`. The gaps between
    /// entries are synthetic — separators that exist nowhere in the source.
    segments: Vec<Segment>,
}

/// A run of `len` bytes at `text[out_start..]` that is a **verbatim copy** of
/// `source[src_start..]`. Mapping inside a segment is exact arithmetic; that is
/// the whole basis of the edit projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub out_start: usize,
    pub len: usize,
    pub src_start: usize,
}

impl Minified {
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }
}

/// Re-serialize `source` as a token stream, then prove the result.
///
/// INVARIANT: `minify(minify(x).text) == minify(x).text`. An `old_string` the
/// model quoted from one minified read must still match after the file is edited
/// and re-minified, so the second pass has to be a fixed point. Enforced by
/// `minification_is_idempotent`.
///
/// INVARIANT: the output's named-node structure equals the source's. A bare
/// re-parse is not enough — the failure mode that matters is output that parses
/// *clean* and means something else (JS ASI, Python re-nesting), and comparing
/// the pre-order named-kind sequences catches it whether or not we predicted the
/// case. Enforced by `minified_output_has_the_same_named_node_structure_as_the_source`.
pub fn minify(lang: Lang, source: &str, keep_comments: bool) -> Result<Minified, MinifyError> {
    let ex = tokens::extract(lang, source)?;
    let m = join::join(lang, source, &ex, keep_comments);
    tokens::verify(lang, source, &m.text)?;
    Ok(m)
}

#[derive(Debug, PartialEq, Eq)]
pub enum MinifyError {
    /// The input didn't parse clean. Don't touch it — a minified view of code
    /// we can't parse is a guess.
    SourceHasErrors,
    /// Our own output failed re-parse or changed shape. A bug guard, not a
    /// statement about the input.
    ProducedInvalid { near: String },
    /// The leaf walk didn't account for every non-whitespace source byte, so
    /// minifying would delete code. Fires when a grammar gives a node children
    /// that stop short of the node itself.
    UncoveredSource { near: String },
}

impl std::fmt::Display for MinifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceHasErrors => write!(f, "the file does not parse cleanly"),
            Self::ProducedInvalid { near } => {
                write!(
                    f,
                    "the minifier produced output it could not verify near `{near}`"
                )
            }
            Self::UncoveredSource { near } => {
                write!(
                    f,
                    "the grammar left source bytes unaccounted for near `{near}`"
                )
            }
        }
    }
}

/// Property tests over committed fixtures.
///
/// A new convention for this workspace, and deliberately *properties* rather
/// than golden outputs (D-B9): byte-equality of segments, idempotency,
/// re-parse and AST-shape equivalence all survive separator changes that a
/// snapshot would churn on. `include_str!`, so the fixtures are data — nothing
/// here is compiled as Rust except `sample.rs`, which is not part of the crate.
#[cfg(test)]
mod fixture_tests {
    use super::*;

    fn fixtures() -> Vec<(Lang, &'static str)> {
        vec![
            (Lang::Rust, include_str!("../fixtures/sample.rs")),
            (Lang::Go, include_str!("../fixtures/sample.go")),
            (Lang::Python, include_str!("../fixtures/sample.py")),
            (Lang::JavaScript, include_str!("../fixtures/sample.js")),
            (Lang::TypeScript, include_str!("../fixtures/sample.ts")),
            (Lang::Tsx, include_str!("../fixtures/sample.tsx")),
        ]
    }

    /// The multi-line literals each fixture carries. They are single leaf
    /// tokens whose *content* contains newlines, so minified output legitimately
    /// contains `\n` inside them — which is why the one-line assertions elsewhere
    /// use literal-free sources.
    fn multiline_literals(lang: Lang) -> &'static [&'static str] {
        match lang {
            Lang::Rust => &["r#\"line one\nline two; still inside // the raw string\n\"#"],
            Lang::Go => &["`line one\nline two; still inside // the raw string\n`"],
            Lang::Python => {
                &["\"\"\"line one\nline two; still inside # the triple-quoted string\n\"\"\""]
            }
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
                &["`line one\nline two; still inside // the template literal\n`"]
            }
        }
    }

    #[test]
    fn every_fixture_parses_clean_before_we_touch_it() {
        for (lang, src) in fixtures() {
            assert!(
                parses_clean(lang, src),
                "{lang:?} fixture must be valid source or the test proves nothing"
            );
        }
    }

    #[test]
    fn minified_output_reparses_clean_for_every_fixture() {
        for (lang, src) in fixtures() {
            for keep in [true, false] {
                let m = minify(lang, src, keep).unwrap_or_else(|e| {
                    panic!("{lang:?} keep={keep}: {e}");
                });
                assert!(parses_clean(lang, &m.text), "{lang:?} keep={keep}");
            }
        }
    }

    #[test]
    fn minification_is_idempotent() {
        for (lang, src) in fixtures() {
            for keep in [true, false] {
                let once = minify(lang, src, keep).unwrap().text;
                let twice = minify(lang, &once, keep).unwrap().text;
                assert_eq!(
                    once, twice,
                    "{lang:?} keep={keep}: second pass must be a fixed point"
                );
            }
        }
    }

    #[test]
    fn minified_output_has_the_same_named_node_structure_as_the_source() {
        // `minify` refuses on mismatch, so a successful call *is* the assertion
        // — stated explicitly here because the guard is the load-bearing one.
        for (lang, src) in fixtures() {
            for keep in [true, false] {
                assert!(
                    minify(lang, src, keep).is_ok(),
                    "{lang:?} keep={keep}: structure drifted"
                );
            }
        }
    }

    #[test]
    fn every_segment_of_every_fixture_is_a_verbatim_copy_of_its_source_bytes() {
        for (lang, src) in fixtures() {
            let m = minify(lang, src, true).unwrap();
            let mut prev_end = 0usize;
            for s in m.segments() {
                assert_eq!(
                    &m.text[s.out_start..s.out_start + s.len],
                    &src[s.src_start..s.src_start + s.len],
                    "{lang:?}: segment is not a byte copy"
                );
                assert!(s.out_start >= prev_end, "{lang:?}: segments overlap");
                prev_end = s.out_start + s.len;
            }
        }
    }

    #[test]
    fn string_literal_bytes_are_never_altered_by_minification() {
        for (lang, src) in fixtures() {
            for keep in [true, false] {
                let m = minify(lang, src, keep).unwrap();
                for lit in multiline_literals(lang) {
                    assert!(
                        src.contains(lit),
                        "{lang:?}: fixture no longer contains {lit:?} — update the test's table"
                    );
                    assert!(
                        m.text.contains(lit),
                        "{lang:?} keep={keep}: literal altered or dropped"
                    );
                }
                // The comment marker inside a string is not a comment, so it
                // survives even in strip mode.
                assert!(m.text.contains("a;b"), "{lang:?} keep={keep}");
            }
        }
    }

    #[test]
    fn minification_actually_saves_bytes_on_every_fixture() {
        for (lang, src) in fixtures() {
            let kept = minify(lang, src, true).unwrap().text.len();
            let stripped = minify(lang, src, false).unwrap().text.len();
            assert!(stripped < kept, "{lang:?}: stripping comments must shrink");
            assert!(
                kept < src.len(),
                "{lang:?}: comment-preserving mode still saves bytes"
            );
        }
    }

    #[test]
    fn a_source_that_does_not_parse_is_refused_for_every_language() {
        for (lang, _) in fixtures() {
            assert_eq!(
                minify(lang, "!!!(((", true).unwrap_err(),
                MinifyError::SourceHasErrors,
                "{lang:?}"
            );
        }
    }
}
