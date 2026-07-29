//! The tree-sitter side: parse, and walk the tree for leaf tokens.
//!
//! The extraction layer is `pub`: the joiner is its only real consumer, but
//! an internal crate with no semver promise gains nothing from hiding it, and
//! the layer ships (with its tests) one commit ahead of the joiner.
//!
//! INVARIANT: a `Token` is nothing but a byte range into the source, so every
//! byte the joiner emits from one is a verbatim source byte. The whole edit
//! projection stands on that. Enforced by
//! `every_segment_is_a_verbatim_copy_of_source_bytes_and_monotonic`.

use crate::lang::Lang;
use crate::MinifyError;

#[derive(Debug, Clone, Copy)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Code,
    Comment,
}

pub(crate) fn parse(lang: Lang, source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.grammar()).ok()?;
    parser.parse(source, None)
}

/// Does `source` parse without an ERROR or MISSING node anywhere?
pub fn parses_clean(lang: Lang, source: &str) -> bool {
    parse(lang, source).is_some_and(|t| !t.root_node().has_error())
}

/// Depth-first leaf walk. Zero-width leaves (a grammar's synthetic
/// indent/dedent markers) are dropped; everything else is a source slice.
pub fn leaf_tokens(lang: Lang, source: &str) -> Result<Vec<Token>, MinifyError> {
    let tree = parse(lang, source).ok_or(MinifyError::SourceHasErrors)?;
    if tree.root_node().has_error() {
        return Err(MinifyError::SourceHasErrors);
    }
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    walk(&mut cursor, lang, &mut out);
    out.retain(|t| t.start < t.end);
    check_coverage(source, &out)?;
    Ok(out)
}

fn walk(cursor: &mut tree_sitter::TreeCursor, lang: Lang, out: &mut Vec<Token>) {
    loop {
        let node = cursor.node();
        // Comments are atomic even when the grammar gives them children:
        // tree-sitter-rust's `line_comment` has a `//` child that stops short
        // of the node, so descending would drop the comment's actual text.
        if lang.is_comment(node.kind()) {
            out.push(Token {
                start: node.start_byte(),
                end: node.end_byte(),
                kind: TokenKind::Comment,
            });
        } else if node.child_count() == 0 {
            out.push(Token {
                start: node.start_byte(),
                end: node.end_byte(),
                kind: TokenKind::Code,
            });
        } else if cursor.goto_first_child() {
            walk(cursor, lang, out);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            return;
        }
    }
}

/// INVARIANT: the token set accounts for every non-whitespace byte of the
/// source. A grammar whose children stop short of their parent would otherwise
/// silently delete code from the minified view; here it degrades the whole
/// minify instead. Enforced by
/// `a_grammar_that_hides_bytes_from_the_leaf_walk_is_refused_not_truncated`.
fn check_coverage(source: &str, tokens: &[Token]) -> Result<(), MinifyError> {
    let mut covered = 0usize;
    for tok in tokens {
        if tok.start > covered && !source[covered..tok.start].trim().is_empty() {
            return Err(uncovered(source, covered));
        }
        covered = covered.max(tok.end);
    }
    if covered < source.len() && !source[covered..].trim().is_empty() {
        return Err(uncovered(source, covered));
    }
    Ok(())
}

fn uncovered(source: &str, at: usize) -> MinifyError {
    MinifyError::UncoveredSource {
        near: source[at..].chars().take(60).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_tokens_cover_every_non_whitespace_byte_of_rust_source() {
        let src = "fn add(a: u32, b: u32) -> u32 {\n    // sum\n    a + b\n}\n";
        let toks = leaf_tokens(Lang::Rust, src).unwrap();
        // Every token's bytes are verbatim source bytes.
        for t in &toks {
            assert!(t.start < t.end && t.end <= src.len());
        }
        // The comment arrives tagged as a comment, code as code.
        let texts: Vec<&str> = toks.iter().map(|t| &src[t.start..t.end]).collect();
        assert!(texts.contains(&"fn") && texts.contains(&"->") && texts.contains(&"// sum"));
        let comment = toks
            .iter()
            .find(|t| &src[t.start..t.end] == "// sum")
            .unwrap();
        assert_eq!(comment.kind, TokenKind::Comment);
        // Tokens are sorted and non-overlapping.
        for w in toks.windows(2) {
            assert!(w[0].end <= w[1].start);
        }
    }

    #[test]
    fn source_with_a_syntax_error_is_refused() {
        assert_eq!(
            leaf_tokens(Lang::Rust, "fn broken( {").unwrap_err(),
            MinifyError::SourceHasErrors
        );
    }

    #[test]
    fn parses_clean_agrees_with_leaf_extraction_about_what_is_broken() {
        assert!(parses_clean(Lang::Rust, "fn f() {}\n"));
        assert!(!parses_clean(Lang::Rust, "fn broken( {"));
    }

    #[test]
    fn a_grammar_that_hides_bytes_from_the_leaf_walk_is_refused_not_truncated() {
        // Simulating the failure directly: a token set with a hole over real
        // code is rejected, while a hole over whitespace is fine.
        let src = "fn f() { g() }\n";
        let hole = vec![
            Token {
                start: 0,
                end: 2,
                kind: TokenKind::Code,
            },
            Token {
                start: 9,
                end: src.len(),
                kind: TokenKind::Code,
            },
        ];
        assert!(matches!(
            check_coverage(src, &hole),
            Err(MinifyError::UncoveredSource { .. })
        ));
        assert!(check_coverage(
            "a  b",
            &[
                Token {
                    start: 0,
                    end: 1,
                    kind: TokenKind::Code
                },
                Token {
                    start: 3,
                    end: 4,
                    kind: TokenKind::Code
                },
            ]
        )
        .is_ok());
    }
}
