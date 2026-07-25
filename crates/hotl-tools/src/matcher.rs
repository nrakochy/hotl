//! Edit-match cascade (M3a — an OpenCode/Crush-style replacer ladder).
//!
//! Models reproduce *content* faithfully and *whitespace* unreliably (tabs
//! vs spaces, trailing spaces, re-indented blocks). The cascade tries exact
//! match first, then two line-based whitespace-tolerant levels. Each level
//! still requires a **unique** match — tolerance never trades away the
//! only-one-place-to-apply guarantee. The Levenshtein block-anchor level of
//! the full ladder is deferred to the MCP milestone (ledger).

/// Byte range of the text to replace, or why none was chosen.
pub enum Match {
    Unique {
        start: usize,
        end: usize,
        exact: bool,
        /// How to rebase the replacement onto the file's indentation. `None`
        /// for an exact match — there is nothing to rebase.
        reindent: Option<Reindent>,
    },
    None,
    Ambiguous(usize),
}

/// How to rebase a replacement block onto the file's own indentation.
///
/// Levels 2–3 fire *precisely when* the model's whitespace disagrees with the
/// file's, so splicing `new_string` verbatim installs the model's indentation
/// over the file's — on every tolerant match. The fix needs no inference about
/// tab widths: the two blocks matched **line for line**, so the leading
/// whitespace of `old_string`'s line *i* is, by construction, the model's
/// spelling of the leading whitespace of the matched line *i*. That is a
/// ground-truth table, and rebasing is a lookup in it.
///
/// INVARIANT: a whitespace-tolerant edit changes content, never the file's
/// indentation style. Enforced by
/// `edit_keeps_the_files_indentation_on_a_tolerant_match`.
pub struct Reindent {
    /// (the model's leading whitespace, the file's), longest first so prefix
    /// lookup is greedy.
    map: Vec<(String, String)>,
}

impl Reindent {
    /// The file's spelling of `ws`, falling back to translating the longest
    /// known prefix and carrying the remainder through unchanged. A depth the
    /// match never saw therefore survives as *extra* depth rather than being
    /// flattened.
    fn translate(&self, ws: &str) -> String {
        for (from, to) in &self.map {
            if let Some(rest) = ws.strip_prefix(from.as_str()) {
                return format!("{to}{rest}");
            }
        }
        ws.to_string()
    }
}

/// Rewrite every line of `new` so its indentation is spelled the way the file
/// spells it. Blank lines stay blank — indentation is never added to a line
/// that has no content.
pub fn rebase_indent(new: &str, r: &Reindent) -> String {
    let mut out = String::with_capacity(new.len());
    for chunk in new.split_inclusive('\n') {
        let body = chunk.trim_end_matches(['\n', '\r']);
        let eol = &chunk[body.len()..];
        if body.trim().is_empty() {
            out.push_str(chunk); // blank (or whitespace-only) line, untouched
            continue;
        }
        let ws = leading_ws(body);
        out.push_str(&r.translate(ws));
        out.push_str(&body[ws.len()..]);
        out.push_str(eol);
    }
    out
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

pub fn find(content: &str, old: &str) -> Match {
    match content.match_indices(old).take(2).count() {
        1 => {
            let start = content.find(old).expect("counted above");
            return Match::Unique {
                start,
                end: start + old.len(),
                exact: true,
                reindent: None, // exact: the file's own bytes, nothing to rebase
            };
        }
        n if n > 1 => return Match::Ambiguous(content.matches(old).count()),
        _ => {}
    }
    // Level 2: per-line trim (indentation/trailing-space drift).
    // Level 3: collapse every whitespace run (tabs vs spaces inside lines).
    for normalize in [trim_line as fn(&str) -> String, collapse_ws] {
        match find_lines(content, old, normalize) {
            Match::None => {}
            found => return found,
        }
    }
    Match::None
}

/// Match `old` as a block of normalized lines against content line windows;
/// the returned range covers the original (un-normalized) lines.
fn find_lines(content: &str, old: &str, normalize: fn(&str) -> String) -> Match {
    let content_lines = lines_with_spans(content);
    let mut old_raw: Vec<&str> = old.lines().collect();
    let mut old_lines: Vec<String> = old_raw.iter().map(|l| normalize(l)).collect();
    // A trailing newline in old_string is not an extra empty line to match.
    if old.ends_with('\n') && old_lines.last().is_some_and(String::is_empty) {
        old_lines.pop();
        old_raw.pop();
    }
    if old_lines.is_empty() || content_lines.len() < old_lines.len() {
        return Match::None;
    }
    // Normalize each content line once; windows then compare as slices.
    let normalized: Vec<String> = content_lines
        .iter()
        .map(|(text, ..)| normalize(text))
        .collect();
    let mut found: Option<(usize, usize, Reindent)> = None;
    let mut count = 0usize;
    for window_start in 0..=(content_lines.len() - old_lines.len()) {
        if normalized[window_start..window_start + old_lines.len()] == old_lines[..] {
            count += 1;
            let window = &content_lines[window_start..window_start + old_lines.len()];
            let (_, start, _) = window[0];
            let (_, _, end) = window[window.len() - 1];
            found = Some((start, end, indent_map(&old_raw, window)));
        }
    }
    match (count, found) {
        (1, Some((start, end, reindent))) => Match::Unique {
            start,
            end,
            exact: false,
            reindent: Some(reindent),
        },
        (0, _) => Match::None,
        (n, _) => Match::Ambiguous(n),
    }
}

/// Pair each of `old_string`'s lines with the content line it matched and
/// record how the two spell that line's indentation. First spelling wins if a
/// model indent maps to two different file indents inside one block — the
/// block matched, so the disagreement is the model's, not the file's.
fn indent_map(old_raw: &[&str], window: &[(&str, usize, usize)]) -> Reindent {
    let mut map: Vec<(String, String)> = Vec::new();
    for (old_line, (content_line, ..)) in old_raw.iter().zip(window) {
        if old_line.trim().is_empty() || content_line.trim().is_empty() {
            continue; // a blank line pins nothing
        }
        let (from, to) = (leading_ws(old_line), leading_ws(content_line));
        if map.iter().any(|(f, _)| f == from) {
            continue;
        }
        map.push((from.to_string(), to.to_string()));
    }
    // Longest first, so `translate`'s prefix fallback takes the most specific
    // known indent rather than the first one that happens to match.
    map.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    Reindent { map }
}

/// (line text, byte start, byte end) — end excludes the newline.
fn lines_with_spans(content: &str) -> Vec<(&str, usize, usize)> {
    let mut out = Vec::new();
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        out.push((text, offset, offset + text.len()));
        offset += line.len();
    }
    out
}

fn trim_line(s: &str) -> String {
    s.trim().to_string()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(content: &str, old: &str) -> (usize, usize, bool) {
        match find(content, old) {
            Match::Unique {
                start, end, exact, ..
            } => (start, end, exact),
            Match::None => panic!("no match"),
            Match::Ambiguous(n) => panic!("ambiguous ({n})"),
        }
    }

    fn reindent_of(content: &str, old: &str) -> Reindent {
        match find(content, old) {
            Match::Unique {
                exact: false,
                reindent: Some(r),
                ..
            } => r,
            _ => panic!("expected a tolerant match carrying reindent info"),
        }
    }

    #[test]
    fn tolerant_match_reports_the_indent_to_rebase_onto() {
        // The file indents with tabs; the model reproduced the block with
        // spaces. The two blocks matched line for line, so each of the model's
        // indents has a known counterpart in the file.
        let content = "fn f() {\n\tif x {\n\t\ta();\n\t}\n}\n";
        let old = "    if x {\n        a();\n    }";
        let r = reindent_of(content, old);
        // One model indent per file indent, taken from the lines that matched.
        assert_eq!(rebase_indent("    q();", &r), "\tq();");
        assert_eq!(rebase_indent("        q();", &r), "\t\tq();");
    }

    #[test]
    fn rebase_rewrites_every_line_of_the_replacement() {
        let content = "fn f() {\n\tif x {\n\t\ta();\n\t}\n}\n";
        let old = "    if x {\n        a();\n    }";
        let r = reindent_of(content, old);
        assert_eq!(
            rebase_indent("    if x {\n        b();\n    }", &r),
            "\tif x {\n\t\tb();\n\t}"
        );
    }

    #[test]
    fn rebase_is_a_noop_when_the_indents_already_agree() {
        // Level 3 fired on *internal* whitespace, not indentation: there is
        // nothing to rebase and the replacement must pass through untouched.
        let content = "let x =\t1;\nlet y = 2;\n";
        let r = reindent_of(content, "let x = 1;");
        assert_eq!(rebase_indent("let x = 3;", &r), "let x = 3;");
    }

    #[test]
    fn rebase_preserves_indentation_deeper_than_the_matched_block() {
        // `new_string` introduces a level the match never saw. The known
        // prefix is translated and the extra depth is carried through, so the
        // added structure survives.
        let content = "fn f() {\n\tif x {\n\t\ta();\n\t}\n}\n";
        let old = "    if x {\n        a();\n    }";
        let r = reindent_of(content, old);
        assert_eq!(
            rebase_indent(
                "    if x {\n        if y {\n            c();\n        }\n    }",
                &r
            ),
            "\tif x {\n\t\tif y {\n\t\t    c();\n\t\t}\n\t}"
        );
        // A blank line stays blank — never padded with indentation.
        assert_eq!(
            rebase_indent("    a();\n\n    b();", &r),
            "\ta();\n\n\tb();"
        );
    }

    #[test]
    fn exact_wins_and_reports_exact() {
        let content = "fn a() {}\nfn b() {}\n";
        let (start, end, exact) = unique(content, "fn b() {}");
        assert!(exact);
        assert_eq!(&content[start..end], "fn b() {}");
    }

    #[test]
    fn indentation_drift_matches_original_lines() {
        // The model reproduced the block with spaces; the file uses tabs.
        let content = "if x {\n\tdo_thing();\n\tother();\n}\n";
        let old = "if x {\n    do_thing();\n    other();\n}";
        let (start, end, exact) = unique(content, old);
        assert!(!exact);
        assert_eq!(&content[start..end], "if x {\n\tdo_thing();\n\tother();\n}");
    }

    #[test]
    fn internal_whitespace_drift_falls_to_collapse_level() {
        let content = "let x =\t1;\n";
        let (start, end, exact) = unique(content, "let x = 1;");
        assert!(!exact);
        assert_eq!(&content[start..end], "let x =\t1;");
    }

    #[test]
    fn tolerance_never_breaks_uniqueness() {
        let content = "  a();\n\ta();\n";
        // Trimmed, both lines are `a();` — ambiguous, not silently applied.
        assert!(matches!(find(content, "a();"), Match::Ambiguous(2)));
        assert!(matches!(find(content, "missing()"), Match::None));
    }
}
