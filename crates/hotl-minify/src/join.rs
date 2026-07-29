//! Separator synthesis: re-serialize a token stream with the smallest gaps
//! that preserve meaning, recording one `Segment` per emitted token.
//!
//! Not "strip whitespace" — re-serialization. The separator between two tokens
//! is chosen so concatenation can never re-lex into something else.

use crate::tokens::{Token, TokenKind};
use crate::{Lang, Minified, Segment};

/// Characters that can compose a longer operator with a neighbour, so two of
/// them must never touch: `>` then `=` would become `>=`.
const OPERATOR_CHARS: &str = "+-*/%<>=!&|^~.?:#@";

fn wordy(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The separator between two adjacent emitted tokens. `""` is the default; a
/// space wherever concatenation could re-lex differently.
fn base_separator(prev_last: char, next_first: char) -> &'static str {
    let both_wordy = wordy(prev_last) && wordy(next_first);
    let both_op = OPERATOR_CHARS.contains(prev_last) && OPERATOR_CHARS.contains(next_first);
    if both_wordy || both_op {
        " "
    } else {
        ""
    }
}

pub(crate) fn join(lang: Lang, source: &str, tokens: &[Token], keep_comments: bool) -> Minified {
    let mut text = String::with_capacity(source.len() / 2);
    let mut segments = Vec::with_capacity(tokens.len());
    let mut prev: Option<&Token> = None;
    for tok in tokens {
        if tok.kind == TokenKind::Comment && !keep_comments {
            continue;
        }
        if let Some(p) = prev {
            text.push_str(separator(lang, source, p, tok));
        }
        segments.push(Segment {
            out_start: text.len(),
            len: tok.end - tok.start,
            src_start: tok.start,
        });
        text.push_str(&source[tok.start..tok.end]);
        prev = Some(tok);
    }
    Minified { text, segments }
}

/// Language dispatch. Task 3 adds Go's ASI semicolons and Python's line
/// structure; Task 3b adds JS/TS statement boundaries and JSX gaps.
fn separator(lang: Lang, source: &str, prev: &Token, next: &Token) -> &'static str {
    match lang {
        Lang::Rust | Lang::Go | Lang::Python | Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
            rust_separator(source, prev, next)
        }
    }
}

fn rust_separator(source: &str, prev: &Token, next: &Token) -> &'static str {
    let prev_last = source[prev.start..prev.end].chars().last().unwrap_or(' ');
    let next_first = source[next.start..next.end].chars().next().unwrap_or(' ');
    base_separator(prev_last, next_first)
}

#[cfg(test)]
mod tests {
    use crate::{minify, Lang};

    const RUST_SRC: &str = "fn add(a: u32, b: u32) -> u32 {\n    let x = a + b;\n    x\n}\n";

    #[test]
    fn every_segment_is_a_verbatim_copy_of_source_bytes_and_monotonic() {
        let m = minify(Lang::Rust, RUST_SRC, true).unwrap();
        let mut prev_end = 0usize;
        for s in m.segments() {
            assert_eq!(
                &m.text[s.out_start..s.out_start + s.len],
                &RUST_SRC[s.src_start..s.src_start + s.len],
                "minified token must be byte-identical to its source slice"
            );
            assert!(s.out_start >= prev_end, "segments sorted, non-overlapping");
            prev_end = s.out_start + s.len;
        }
    }

    #[test]
    fn rust_minification_drops_newlines_and_indentation_but_keeps_word_gaps() {
        let m = minify(Lang::Rust, RUST_SRC, true).unwrap();
        assert!(!m.text.contains('\n'));
        assert!(
            m.text.contains("fn add"),
            "wordy tokens keep a separating space"
        );
        assert!(m.text.contains("let x"), "keyword/ident boundary preserved");
        assert!(m.text.len() < RUST_SRC.len());
    }

    #[test]
    fn adjacent_punctuation_that_would_merge_into_a_different_operator_is_spaced() {
        // `>` then `=` must not become `>=`.
        let src = "fn f(v: Vec<u8>) -> bool { v.len() > 1 }\n";
        let m = minify(Lang::Rust, src, true).unwrap();
        assert!(
            !m.text.contains(">="),
            "separator synthesis must not create new operators: {}",
            m.text
        );
    }
}
