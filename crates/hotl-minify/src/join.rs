//! Separator synthesis: re-serialize a token stream with the smallest gaps
//! that preserve meaning, recording one `Segment` per emitted token.
//!
//! Not "strip whitespace" — re-serialization. The separator between two tokens
//! is chosen so concatenation can never re-lex into something else.

use std::borrow::Cow;

use crate::tokens::{Extraction, Token, TokenKind};
use crate::{Lang, Minified, Segment};

/// What goes between two emitted tokens.
enum Sep {
    /// Text we invented. It exists nowhere in the source, so `project_span`
    /// must snap away from it.
    Synthetic(Cow<'static, str>),
    /// A source byte range copied through untouched — JSX's renderer-visible
    /// whitespace. Recorded as a real segment: it *is* source bytes, so a match
    /// landing in it projects like any token.
    Verbatim(usize, usize),
}

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

pub(crate) fn join(lang: Lang, source: &str, ex: &Extraction, keep_comments: bool) -> Minified {
    let py = (lang == Lang::Python).then(|| PyLines::new(source));
    let mut text = String::with_capacity(source.len() / 2);
    let mut segments = Vec::with_capacity(ex.tokens.len());
    let mut prev: Option<&Token> = None;
    for tok in &ex.tokens {
        if tok.kind == TokenKind::Comment && !keep_comments {
            continue;
        }
        if let Some(p) = prev {
            match separator(lang, source, p, tok, py.as_ref(), ex) {
                Sep::Synthetic(s) => text.push_str(&s),
                Sep::Verbatim(a, b) => {
                    segments.push(Segment {
                        out_start: text.len(),
                        len: b - a,
                        src_start: a,
                    });
                    text.push_str(&source[a..b]);
                }
            }
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

fn separator(
    lang: Lang,
    source: &str,
    prev: &Token,
    next: &Token,
    py: Option<&PyLines>,
    ex: &Extraction,
) -> Sep {
    match lang {
        Lang::Go => Sep::Synthetic(go_separator(source, prev, next)),
        // `py` is `Some` exactly when `lang` is Python — same `match` in `join`.
        Lang::Python => Sep::Synthetic(python_separator(
            source,
            prev,
            next,
            py.expect("python line table"),
        )),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => script_separator(source, prev, next, ex),
        Lang::Rust => Sep::Synthetic(Cow::Borrowed(rust_separator(source, prev, next))),
    }
}

/// JS/TS: the gap is copied when it sits inside a JSX subtree, becomes an
/// explicit `;` when it crosses a line break at a statement boundary the tree
/// recorded, and otherwise joins like any other language.
///
/// The `;` is the whole of D-B7: `let a = b\n(c)` is one call while `a\n++b` is
/// two statements, and the token pair at the break is the same shape in both —
/// only the tree knows which. Where the source relied on ASI it says so; where
/// the break was a continuation, no statement ends there.
fn script_separator(source: &str, prev: &Token, next: &Token, ex: &Extraction) -> Sep {
    if ex.is_verbatim_gap(prev.end, next.start) {
        return Sep::Verbatim(prev.end, next.start);
    }
    if crossed_newline(source, prev, next) && ex.ends_a_statement(prev.end) {
        return Sep::Synthetic(Cow::Borrowed(";"));
    }
    Sep::Synthetic(Cow::Borrowed(rust_separator(source, prev, next)))
}

fn rust_separator(source: &str, prev: &Token, next: &Token) -> &'static str {
    base_separator(last_char(source, prev), first_char(source, next))
}

fn last_char(source: &str, tok: &Token) -> char {
    source[tok.start..tok.end].chars().last().unwrap_or(' ')
}

fn first_char(source: &str, tok: &Token) -> char {
    source[tok.start..tok.end].chars().next().unwrap_or(' ')
}

fn crossed_newline(source: &str, prev: &Token, next: &Token) -> bool {
    source[prev.end..next.start].contains('\n')
}

/// Go's automatic semicolon rule, applied at the original line boundaries: the
/// spec inserts `;` when a line ends in an identifier, a literal, or one of
/// `break continue fallthrough return ++ -- ) ] }`.
fn go_separator(source: &str, prev: &Token, next: &Token) -> Cow<'static, str> {
    if crossed_newline(source, prev, next) && go_asi_trigger(&source[prev.start..prev.end]) {
        return Cow::Borrowed(";");
    }
    Cow::Borrowed(rust_separator(source, prev, next))
}

fn go_asi_trigger(tok: &str) -> bool {
    matches!(
        tok,
        "break" | "continue" | "fallthrough" | "return" | "++" | "--" | ")" | "]" | "}"
    ) || tok
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"' || c == '`' || c == '\'')
}

/// Python keeps one source line per line; only the indentation is rewritten,
/// to one space per level.
fn python_separator(source: &str, prev: &Token, next: &Token, py: &PyLines) -> Cow<'static, str> {
    if crossed_newline(source, prev, next) {
        let level = py.level_at(next.start);
        return Cow::Owned(format!("\n{}", " ".repeat(level)));
    }
    Cow::Borrowed(rust_separator(source, prev, next))
}

/// Per-line indent levels plus the byte offsets they start at, so a token's
/// level is a lookup rather than a re-scan.
struct PyLines {
    starts: Vec<usize>,
    levels: Vec<usize>,
}

impl PyLines {
    fn new(source: &str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self {
            starts,
            levels: python_levels(source),
        }
    }

    fn level_at(&self, offset: usize) -> usize {
        let line = self.starts.partition_point(|s| *s <= offset).max(1) - 1;
        self.levels.get(line).copied().unwrap_or(0)
    }
}

/// Indent level per source line: a stack of seen indent widths, matching
/// Python's tokenizer.
///
/// INVARIANT: levels come only from code-bearing lines. CPython's tokenizer
/// generates no INDENT/DEDENT for blank or comment-only lines; treating them as
/// code would let an outdented `# note` inside a block pop the stack and
/// silently re-nest what follows — a valid parse with changed meaning. Enforced
/// by `an_outdented_comment_inside_a_python_block_does_not_renest_what_follows`.
pub(crate) fn python_levels(source: &str) -> Vec<usize> {
    let mut stack: Vec<usize> = vec![0];
    let mut levels = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            levels.push(stack.len() - 1);
            continue;
        }
        let width = line.len() - trimmed.len();
        while *stack.last().unwrap_or(&0) > width {
            stack.pop();
        }
        if width > *stack.last().unwrap_or(&0) {
            stack.push(width);
        }
        levels.push(stack.len() - 1);
    }
    levels
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

    #[test]
    fn go_line_breaks_become_explicit_semicolons_at_asi_positions() {
        let src = "package m\n\nfunc Add(a int, b int) int {\n\tx := a + b\n\treturn x\n}\n";
        let m = minify(Lang::Go, src, true).unwrap();
        assert!(!m.text.contains('\n'));
        assert!(
            m.text.contains("x:=a+b;return x") || m.text.contains("x:=a+b;return x;"),
            "got: {}",
            m.text
        );
    }

    #[test]
    fn go_semicolon_is_not_inserted_after_an_opening_token() {
        let src = "package m\n\nfunc F(\n\ta int,\n) int { return a }\n";
        let m = minify(Lang::Go, src, true).unwrap();
        assert!(!m.text.contains("(;"), "no ASI after an opener: {}", m.text);
    }

    #[test]
    fn python_keeps_line_structure_and_renormalizes_indent_to_one_space_per_level() {
        let src = "def f(a):\n    if a:\n        return 1\n    return 2\n";
        let m = minify(Lang::Python, src, true).unwrap();
        assert_eq!(m.text, "def f(a):\n if a:\n  return 1\n return 2");
    }

    #[test]
    fn an_outdented_comment_inside_a_python_block_does_not_renest_what_follows() {
        // INVARIANT: indentation levels are computed only from code-bearing
        // lines (CPython tokenizer rule) — a col-0 comment inside a nested
        // block must not move `y = 2` out of the `if` body. Enforced by this
        // test.
        let src = "def f(a):\n    if a:\n        x = 1\n# note\n        y = 2\n";
        let m = minify(Lang::Python, src, true).unwrap();
        assert!(m.text.contains("\n  y"), "y stays at level 2: {}", m.text);
    }

    #[test]
    fn a_semicolonless_js_file_gets_explicit_semicolons_at_statement_boundaries() {
        let src = "const a = 1\nconst b = a + 1\n";
        let m = minify(Lang::JavaScript, src, true).unwrap();
        assert_eq!(m.text, "const a=1;const b=a+1");
    }

    #[test]
    fn a_restricted_production_keeps_its_asi_semicolon() {
        // `return` + newline + expr is `return; expr` in the source; joining
        // without `;` would parse clean and mean something else.
        let src = "function f(x) {\n  return\n  x\n}\n";
        let m = minify(Lang::JavaScript, src, true).unwrap();
        assert!(m.text.contains("return;x"), "got: {}", m.text);
    }

    #[test]
    fn a_multiline_continuation_is_joined_without_a_semicolon() {
        let src = "const a = b\n  .c()\n  .d()\n";
        let m = minify(Lang::JavaScript, src, true).unwrap();
        assert_eq!(m.text, "const a=b.c().d()");
    }

    #[test]
    fn a_paren_continuation_stays_a_call_not_two_statements() {
        // The source parsed `f\n(1)` as one call expression; no statement ends
        // at `f`, so no `;` may appear there.
        let src = "const x = f\n(1)\n";
        let m = minify(Lang::JavaScript, src, true).unwrap();
        assert!(!m.text.contains(";("), "got: {}", m.text);
    }

    #[test]
    fn a_next_line_increment_stays_two_statements() {
        // `a\n++b` is two statements in the source. Joining to `a++b` would be
        // one, and would parse clean.
        let src = "let a = 1\nlet b = 2\na\n++b\n";
        let m = minify(Lang::JavaScript, src, true).unwrap();
        assert!(m.text.contains("a;++b"), "got: {}", m.text);
    }

    #[test]
    fn ts_interface_members_get_separators_that_reparse() {
        let src = "interface A {\n  x: number\n  y: string\n}\n";
        let m = minify(Lang::TypeScript, src, true).unwrap();
        assert!(
            crate::parses_clean(Lang::TypeScript, &m.text),
            "got: {}",
            m.text
        );
        assert!(m.text.contains("x:number;y:string"), "got: {}", m.text);
    }

    #[test]
    fn jsx_text_whitespace_is_preserved_verbatim() {
        let src = "const el = (\n  <p>\n    hello <b>world</b>\n  </p>\n)\n";
        let m = minify(Lang::Tsx, src, true).unwrap();
        assert!(
            m.text.contains("hello <b>world</b>"),
            "renderer-visible space kept: {}",
            m.text
        );
    }

    #[test]
    fn jsx_attribute_lists_keep_the_space_that_separates_them() {
        // No re-lex hazard `base_separator` can see between `"a"` and `id`,
        // yet joining them is a syntax error. The verbatim-gap rule is what
        // saves it.
        let src = "const el = <p className=\"a\" id=\"b\">hi</p>\n";
        let m = minify(Lang::Tsx, src, true).unwrap();
        assert!(
            crate::parses_clean(Lang::Tsx, &m.text),
            "attributes still separated: {}",
            m.text
        );
    }

    #[test]
    fn a_go_struct_literal_with_a_trailing_comma_gets_no_semicolon_before_its_brace() {
        let src = "package m\n\ntype T struct {\n\tA int\n}\n\nfunc F() T {\n\treturn T{\n\t\tA: 1,\n\t}\n}\n";
        let m = minify(Lang::Go, src, true).unwrap();
        assert!(!m.text.contains(",;"), "no ASI after a comma: {}", m.text);
        assert!(
            m.text.contains("A int;"),
            "struct fields still separate: {}",
            m.text
        );
        assert!(crate::parses_clean(Lang::Go, &m.text), "got: {}", m.text);
    }
}
