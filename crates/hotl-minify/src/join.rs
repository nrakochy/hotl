//! Separator synthesis: re-serialize a token stream with the smallest gaps
//! that preserve meaning, recording one `Segment` per emitted token.
//!
//! Not "strip whitespace" — re-serialization. The separator between two tokens
//! is chosen so concatenation can never re-lex into something else.

use std::borrow::Cow;

use crate::tokens::{Extraction, Token, TokenKind};
use crate::{Lang, Minified, Segment};

/// Characters that can compose a longer operator with a neighbour, so two of
/// them must never touch: `>` then `=` would become `>=`.
const OPERATOR_CHARS: &str = "+-*/%<>=!&|^~.?:#@";

/// Tokens an ASI semicolon is never needed before: the closer terminates the
/// statement on its own. Skipping it keeps the output smaller *and* keeps the
/// parse shape identical, which the AST-equivalence check cares about.
const CLOSERS: [&str; 3] = ["}", ")", "]"];

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
    let py = (lang == Lang::Python).then(|| PyLines::new(source, &ex.tokens));
    let mut text = String::with_capacity(source.len() / 2);
    let mut segments = Vec::with_capacity(ex.tokens.len());
    let mut prev: Option<&Token> = None;
    for (i, tok) in ex.tokens.iter().enumerate() {
        if tok.kind == TokenKind::Comment && !keep_comments {
            continue;
        }
        if let Some(p) = prev {
            let cx = Gap {
                lang,
                source,
                prev: p,
                next: tok,
                next_index: i,
                py: py.as_ref(),
                ex,
            };
            match cx.separator() {
                Sep::Synthetic(s) => text.push_str(&s),
                // An empty gap records nothing: a zero-length segment overlaps
                // no range, so `project_span` could never snap to it.
                Sep::Verbatim(a, b) if a < b => {
                    segments.push(Segment {
                        out_start: text.len(),
                        len: b - a,
                        src_start: a,
                    });
                    text.push_str(&source[a..b]);
                }
                Sep::Verbatim(..) => {}
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

/// Everything the separator decision needs about one gap between two emitted
/// tokens.
struct Gap<'a> {
    lang: Lang,
    source: &'a str,
    prev: &'a Token,
    next: &'a Token,
    next_index: usize,
    py: Option<&'a PyLines>,
    ex: &'a Extraction,
}

impl Gap<'_> {
    fn separator(&self) -> Sep {
        let sep = match self.lang {
            Lang::Go => Sep::Synthetic(self.go_separator()),
            // `py` is `Some` exactly when `lang` is Python — same test in `join`.
            Lang::Python => Sep::Synthetic(self.python_separator()),
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => self.script_separator(),
            Lang::Rust => Sep::Synthetic(Cow::Borrowed(self.base())),
        };
        self.guard_line_comment(sep)
    }

    /// A kept `//` or `#` comment swallows everything to the next newline, so
    /// the token after one can never share its line.
    fn guard_line_comment(&self, sep: Sep) -> Sep {
        if self.prev.kind != TokenKind::Comment {
            return sep;
        }
        let text = self.text(self.prev);
        if !(text.starts_with("//") || text.starts_with('#')) {
            return sep;
        }
        match &sep {
            // Python's own separator already carries the newline *and* the
            // indentation; replacing it would outdent the next line.
            Sep::Synthetic(s) if s.contains('\n') => sep,
            Sep::Verbatim(a, b) if self.source[*a..*b].contains('\n') => sep,
            _ => Sep::Synthetic(Cow::Borrowed("\n")),
        }
    }

    fn text(&self, tok: &Token) -> &str {
        &self.source[tok.start..tok.end]
    }

    fn base(&self) -> &'static str {
        let prev_last = self.text(self.prev).chars().last().unwrap_or(' ');
        let next_first = self.text(self.next).chars().next().unwrap_or(' ');
        base_separator(prev_last, next_first)
    }

    fn crossed_newline(&self) -> bool {
        self.source[self.prev.end..self.next.start].contains('\n')
    }

    fn next_is_closer(&self) -> bool {
        CLOSERS.contains(&self.text(self.next))
    }

    /// Go's automatic semicolon rule, applied at the original line boundaries:
    /// the spec inserts `;` when a line ends in an identifier, a literal, or one
    /// of `break continue fallthrough return ++ -- ) ] }`.
    fn go_separator(&self) -> Cow<'static, str> {
        if self.crossed_newline() && !self.next_is_closer() && go_asi_trigger(self.text(self.prev))
        {
            return Cow::Borrowed(";");
        }
        Cow::Borrowed(self.base())
    }

    /// JS/TS: the gap is copied when it sits inside a JSX subtree, becomes an
    /// explicit `;` when it crosses a line break at a statement boundary the
    /// tree recorded, and otherwise joins like any other language.
    ///
    /// The `;` is the whole of D-B7: `let a = b\n(c)` is one call while `a\n++b`
    /// is two statements, and the token pair at the break is the same shape in
    /// both — only the tree knows which.
    fn script_separator(&self) -> Sep {
        if self.ex.is_verbatim_gap(self.prev.end, self.next.start) {
            return Sep::Verbatim(self.prev.end, self.next.start);
        }
        if self.crossed_newline()
            && !self.next_is_closer()
            && self.ex.ends_a_statement(self.prev.end)
        {
            return Sep::Synthetic(Cow::Borrowed(";"));
        }
        Sep::Synthetic(Cow::Borrowed(self.base()))
    }

    /// Python keeps one logical line per line; only the indentation is
    /// rewritten, to one space per level.
    fn python_separator(&self) -> Cow<'static, str> {
        let py = self.py.expect("python line table");
        match py.line_start_level(self.next_index) {
            Some(level) => Cow::Owned(format!("\n{}", " ".repeat(level))),
            None => Cow::Borrowed(self.base()),
        }
    }
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

/// Python's logical line structure, derived from the token stream rather than
/// the raw text.
///
/// INVARIANT: only tokens that begin a *logical* line touch the indent stack.
/// A line inside a triple-quoted string, or inside an open bracket, is not one:
/// scanning raw text instead lets a string's indented interior line push a
/// bogus indent width, which re-nests the code that follows. Enforced by
/// `a_python_docstring_with_an_odd_interior_indent_does_not_renest_what_follows`.
///
/// INVARIANT: blank and comment-only lines contribute no level. CPython's
/// tokenizer emits no INDENT/DEDENT for them, and counting them would let an
/// outdented `# note` inside a block pop the stack and silently re-nest what
/// follows — a valid parse with changed meaning. Enforced by
/// `an_outdented_comment_inside_a_python_block_does_not_renest_what_follows`.
struct PyLines {
    /// `Some(level)` for tokens that begin a logical line.
    levels: Vec<Option<usize>>,
}

impl PyLines {
    fn new(source: &str, tokens: &[Token]) -> Self {
        let line_starts = line_starts(source);
        let mut stack: Vec<usize> = vec![0];
        let mut depth = 0usize;
        let mut levels = vec![None; tokens.len()];
        let mut prev_end: Option<usize> = None;
        for (i, tok) in tokens.iter().enumerate() {
            let fresh_line = prev_end.is_some_and(|e| source[e..tok.start].contains('\n'));
            if fresh_line && depth == 0 {
                levels[i] = Some(if tok.kind == TokenKind::Comment {
                    stack.len() - 1
                } else {
                    let start = *line_starts
                        .range(..=tok.start)
                        .next_back()
                        .expect("line 0 always present");
                    push_level(&mut stack, tok.start - start)
                });
            }
            depth = depth.saturating_add_signed(bracket_delta(tok, &source[tok.start..tok.end]));
            prev_end = Some(tok.end);
        }
        Self { levels }
    }

    fn line_start_level(&self, token_index: usize) -> Option<usize> {
        self.levels.get(token_index).copied().flatten()
    }
}

fn line_starts(source: &str) -> std::collections::BTreeSet<usize> {
    let mut set = std::collections::BTreeSet::new();
    set.insert(0usize);
    set.extend(
        source
            .bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i + 1),
    );
    set
}

/// The tokenizer's indent stack: pop past anything wider, push anything wider.
fn push_level(stack: &mut Vec<usize>, width: usize) -> usize {
    while stack.last().copied().unwrap_or(0) > width {
        stack.pop();
    }
    if width > stack.last().copied().unwrap_or(0) {
        stack.push(width);
    }
    stack.len() - 1
}

/// Bracket nesting, which is what makes a Python line break implicit. Counting
/// f-string interpolation braces along the way is harmless: they balance.
fn bracket_delta(tok: &Token, text: &str) -> isize {
    if tok.kind == TokenKind::Comment {
        return 0;
    }
    match text {
        "(" | "[" | "{" => 1,
        ")" | "]" | "}" => -1,
        _ => 0,
    }
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
        assert!(m.text.contains("x:=a+b;return x"), "got: {}", m.text);
    }

    #[test]
    fn go_semicolon_is_not_inserted_after_an_opening_token() {
        let src = "package m\n\nfunc F(\n\ta int,\n) int { return a }\n";
        let m = minify(Lang::Go, src, true).unwrap();
        assert!(!m.text.contains("(;"), "no ASI after an opener: {}", m.text);
    }

    #[test]
    fn no_asi_semicolon_is_inserted_before_a_closing_brace() {
        // The closer terminates the statement itself, so the `;` is pure waste
        // — and an extra one changes the parse shape the equivalence check pins.
        let src = "package m\n\nfunc F() int {\n\treturn 1\n}\n";
        let m = minify(Lang::Go, src, true).unwrap();
        assert!(!m.text.contains(";}"), "got: {}", m.text);
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
    fn a_python_docstring_with_an_odd_interior_indent_does_not_renest_what_follows() {
        // The interior line sits at column 3 — between the stack's 0 and 4.
        // Scanning raw text would push 3 and then read `return` at 4 as one
        // level deeper than it is.
        let src = "def f():\n    x = \"\"\"abc\n   def\"\"\"\n    return x\n";
        let m = minify(Lang::Python, src, true).unwrap();
        assert!(
            m.text.contains("\n return x"),
            "return stays at level 1: {:?}",
            m.text
        );
        assert!(
            crate::parses_clean(Lang::Python, &m.text),
            "got: {}",
            m.text
        );
    }

    #[test]
    fn a_python_bracket_continuation_is_joined_onto_one_line() {
        // Python ignores line breaks inside brackets, so there is nothing to
        // preserve — and joining keeps the continuation's column off the
        // indent stack.
        let src = "def f():\n    x = g(1,\n          2)\n    return x\n";
        let m = minify(Lang::Python, src, true).unwrap();
        assert!(m.text.contains("x=g(1,2)"), "got: {:?}", m.text);
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
            m.text.contains("A int"),
            "struct fields still separate: {}",
            m.text
        );
        assert!(crate::parses_clean(Lang::Go, &m.text), "got: {}", m.text);
    }

    #[test]
    fn a_kept_line_comment_is_followed_by_a_newline_so_it_cannot_swallow_code() {
        let src = "fn f() -> u32 {\n    // the answer\n    42\n}\n";
        let m = minify(Lang::Rust, src, true).unwrap();
        assert!(m.text.contains("// the answer\n"), "got: {}", m.text);
        assert!(crate::parses_clean(Lang::Rust, &m.text));
    }

    #[test]
    fn a_kept_block_comment_stays_inline() {
        let src = "fn f() -> u32 {\n    /* the answer */\n    42\n}\n";
        let m = minify(Lang::Rust, src, true).unwrap();
        assert!(!m.text.contains('\n'), "got: {}", m.text);
        assert!(crate::parses_clean(Lang::Rust, &m.text));
    }

    #[test]
    fn stripped_comments_do_not_appear_and_the_result_still_reparses() {
        let src = "fn f() -> u32 {\n    // gone\n    42\n}\n";
        let m = minify(Lang::Rust, src, false).unwrap();
        assert!(!m.text.contains("gone"));
        assert!(crate::parses_clean(Lang::Rust, &m.text));
    }

    #[test]
    fn comment_modes_are_both_idempotent() {
        for keep in [true, false] {
            let src = include_str!("../fixtures/sample.rs");
            let once = minify(Lang::Rust, src, keep).unwrap().text;
            assert_eq!(
                once,
                minify(Lang::Rust, &once, keep).unwrap().text,
                "keep_comments={keep}: second pass must be a fixed point"
            );
        }
    }

    #[test]
    fn a_dropped_comment_still_contributes_its_line_break_to_go_asi() {
        let src = "package m\n\nfunc F() {\n\tx := 1\n\t// c\n\ty := x\n\t_ = y\n}\n";
        let m = minify(Lang::Go, src, false).unwrap();
        assert!(!m.text.contains("// c"), "comment gone: {}", m.text);
        // The `;` still fires: the gap the dropped comment sat in is the one
        // measured for the line break.
        assert!(m.text.contains("x:=1;y:=x"), "got: {}", m.text);
    }
}
