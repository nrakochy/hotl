//! Edit-match cascade (M3a; corpus 05 — OpenCode/Crush replacer ladder).
//!
//! Models reproduce *content* faithfully and *whitespace* unreliably (tabs
//! vs spaces, trailing spaces, re-indented blocks). The cascade tries exact
//! match first, then two line-based whitespace-tolerant levels. Each level
//! still requires a **unique** match — tolerance never trades away the
//! only-one-place-to-apply guarantee. The Levenshtein block-anchor level of
//! the full ladder is deferred to the MCP milestone (ledger).

/// Byte range of the text to replace, or why none was chosen.
pub enum Match {
    Unique { start: usize, end: usize, exact: bool },
    None,
    Ambiguous(usize),
}

pub fn find(content: &str, old: &str) -> Match {
    match content.match_indices(old).take(2).count() {
        1 => {
            let start = content.find(old).expect("counted above");
            return Match::Unique { start, end: start + old.len(), exact: true };
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
    let mut old_lines: Vec<String> = old.lines().map(normalize).collect();
    // A trailing newline in old_string is not an extra empty line to match.
    if old.ends_with('\n') && old_lines.last().is_some_and(String::is_empty) {
        old_lines.pop();
    }
    if old_lines.is_empty() || content_lines.len() < old_lines.len() {
        return Match::None;
    }
    let mut found: Option<(usize, usize)> = None;
    let mut count = 0usize;
    for window_start in 0..=(content_lines.len() - old_lines.len()) {
        let window = &content_lines[window_start..window_start + old_lines.len()];
        if window.iter().map(|(text, ..)| normalize(text)).eq(old_lines.iter().cloned()) {
            count += 1;
            let (_, start, _) = window[0];
            let (_, _, end) = window[window.len() - 1];
            found = Some((start, end));
        }
    }
    match (count, found) {
        (1, Some((start, end))) => Match::Unique { start, end, exact: false },
        (0, _) => Match::None,
        (n, _) => Match::Ambiguous(n),
    }
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
            Match::Unique { start, end, exact } => (start, end, exact),
            Match::None => panic!("no match"),
            Match::Ambiguous(n) => panic!("ambiguous ({n})"),
        }
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
