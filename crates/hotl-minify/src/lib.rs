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
pub use tokens::{leaf_tokens, parses_clean, Token, TokenKind};

/// A minified view plus the map back to the source it came from.
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

/// Re-serialize `source` as a token stream. Validation (re-parse, AST-shape
/// equivalence) arrives in Task 4.
pub fn minify(lang: Lang, source: &str, keep_comments: bool) -> Result<Minified, MinifyError> {
    let toks = tokens::leaf_tokens(lang, source)?;
    Ok(join::join(lang, source, &toks, keep_comments))
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
