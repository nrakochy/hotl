//! The tree-sitter side: parse, and walk the tree for everything the joiner
//! needs — leaf tokens, the statement boundaries where ASI fired, and the
//! subtrees whose whitespace is not ours to remove.
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

/// What one walk of the tree yields.
#[derive(Debug, Default)]
pub struct Extraction {
    pub tokens: Vec<Token>,
    /// End bytes of statement-class nodes that did **not** spell their own
    /// terminator — the tree's record of where ASI fired. Sorted, deduplicated.
    /// Empty for languages that need no ASI reconstruction.
    pub semi_ends: Vec<usize>,
    /// Byte ranges whose inter-token gaps are renderer-visible (JSX), so the
    /// joiner copies those gaps instead of synthesizing them.
    pub verbatim: Vec<(usize, usize)>,
}

impl Extraction {
    pub fn ends_a_statement(&self, offset: usize) -> bool {
        self.semi_ends.binary_search(&offset).is_ok()
    }

    pub fn is_verbatim_gap(&self, from: usize, to: usize) -> bool {
        self.verbatim.iter().any(|(a, b)| *a <= from && to <= *b)
    }
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

pub fn extract(lang: Lang, source: &str) -> Result<Extraction, MinifyError> {
    let tree = parse(lang, source).ok_or(MinifyError::SourceHasErrors)?;
    if tree.root_node().has_error() {
        return Err(MinifyError::SourceHasErrors);
    }
    let mut w = Walker {
        lang,
        source,
        out: Extraction::default(),
    };
    let mut cursor = tree.root_node().walk();
    w.visit(&mut cursor);
    let mut out = w.out;
    out.tokens.retain(|t| t.start < t.end);
    out.semi_ends.sort_unstable();
    out.semi_ends.dedup();
    check_coverage(source, &out.tokens)?;
    Ok(out)
}

struct Walker<'s> {
    lang: Lang,
    source: &'s str,
    out: Extraction,
}

impl Walker<'_> {
    /// Depth-first. Zero-width leaves (a grammar's synthetic indent/dedent
    /// markers) fall out in `extract`; everything else is a source slice.
    fn visit(&mut self, cursor: &mut tree_sitter::TreeCursor) {
        loop {
            let node = cursor.node();
            self.note(&node);
            if self.lang.is_comment(node.kind()) {
                self.push(&node, TokenKind::Comment);
            } else if node.child_count() == 0 || !children_cover(&node, self.source) {
                self.push(&node, TokenKind::Code);
            } else if cursor.goto_first_child() {
                self.visit(cursor);
                cursor.goto_parent();
            }
            if !cursor.goto_next_sibling() {
                return;
            }
        }
    }

    fn push(&mut self, node: &tree_sitter::Node, kind: TokenKind) {
        self.out.tokens.push(Token {
            start: node.start_byte(),
            end: node.end_byte(),
            kind,
        });
    }

    /// Record what the joiner cannot see from the token stream alone: which
    /// byte positions end a statement that spelled no terminator, and which
    /// subtrees own their whitespace.
    fn note(&mut self, node: &tree_sitter::Node) {
        let kind = node.kind();
        if self.lang.owns_its_whitespace(kind) {
            self.out.verbatim.push((node.start_byte(), node.end_byte()));
        }
        if !self.lang.is_asi_statement(kind) {
            return;
        }
        // The node's own last token *is* its terminator when it spelled one.
        // `,` counts: an object-type member ending in `,` needs no `;` added,
        // and appending one there is a syntax error.
        let text = &self.source[node.start_byte()..node.end_byte()];
        if !text.ends_with(';') && !text.ends_with(',') {
            self.out.semi_ends.push(node.end_byte());
        }
    }
}

/// Do a node's children account for all of its non-whitespace bytes?
///
/// When they don't, the node is emitted whole rather than descended into — a
/// grammar is free to leave bytes out of its child list, and tree-sitter-rust
/// does: `raw_string_literal` has a single `string_content` child and no node
/// at all for the `r#"` / `"#` delimiters, and `line_comment` has a `//` child
/// that stops short of the comment text. Emitting the parent verbatim is always
/// byte-safe; descending past a hole silently deletes code.
fn children_cover(node: &tree_sitter::Node, source: &str) -> bool {
    let mut at = node.start_byte();
    for i in 0..node.child_count() as u32 {
        let child = node.child(i).expect("i < child_count");
        if child.start_byte() < at || !source[at..child.start_byte()].trim().is_empty() {
            return false;
        }
        at = child.end_byte();
    }
    source[at..node.end_byte()].trim().is_empty()
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
        let toks = extract(Lang::Rust, src).unwrap().tokens;
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
            extract(Lang::Rust, "fn broken( {").unwrap_err(),
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

    #[test]
    fn a_language_without_asi_records_no_statement_boundaries() {
        let ex = extract(Lang::Rust, "fn f() -> u32 { 1 }\n").unwrap();
        assert!(ex.semi_ends.is_empty() && ex.verbatim.is_empty());
    }

    #[test]
    fn a_js_statement_that_spelled_its_own_semicolon_records_no_boundary() {
        let with = extract(Lang::JavaScript, "const a = 1;\nconst b = 2;\n").unwrap();
        assert!(
            with.semi_ends.is_empty(),
            "explicit terminators need no reconstruction: {:?}",
            with.semi_ends
        );
        let without = extract(Lang::JavaScript, "const a = 1\nconst b = 2\n").unwrap();
        assert_eq!(without.semi_ends.len(), 2);
    }
}
