//! Display-width wrapping, done up front instead of inside a widget.
//!
//! Ratatui's `Wrap` re-flows while it renders, so the caller never learns how
//! many rows a line became — and both the transcript's follow-scroll and the
//! input's cursor need exactly that number. Wrapping here keeps those in
//! lock-step with what actually lands on screen. Widths are display columns
//! (the same measure the backend uses), so a wide glyph can't overrun the edge;
//! ranges are char indices, never bytes, matching `vim`'s column arithmetic.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Char ranges covering `text`, each at most `width` display columns wide.
/// Breaks after the last space that fits; a word longer than the row is cut.
/// Ranges are contiguous and cover every char — that total coverage is what
/// lets a cursor's char index map back to exactly one row.
pub fn rows(text: &str, width: usize) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || width == 0 {
        return vec![(0, chars.len())];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let fits = start + fit(&chars[start..], width);
        if fits >= chars.len() {
            out.push((start, chars.len()));
            break;
        }
        // `max(start + 1)` guarantees progress when even one char overflows.
        let brk = chars[start..fits]
            .iter()
            .rposition(|c| *c == ' ')
            .map_or(fits, |i| start + i + 1)
            .max(start + 1);
        out.push((start, brk));
        start = brk;
    }
    out
}

/// Chars from the front of `chars` that fit in `width` display columns.
fn fit(chars: &[char], width: usize) -> usize {
    let mut used = 0;
    for (i, c) in chars.iter().enumerate() {
        used += c.width().unwrap_or(0);
        if used > width {
            return i;
        }
    }
    chars.len()
}

/// Display columns spanned by `text`'s chars in `[a, b)` — the input's cursor
/// column, measured the way the terminal will.
pub fn columns(text: &str, a: usize, b: usize) -> usize {
    text.chars()
        .skip(a)
        .take(b.saturating_sub(a))
        .map(|c| c.width().unwrap_or(0))
        .sum()
}

/// The chars of `text` in `[a, b)`.
pub fn slice(text: &str, a: usize, b: usize) -> String {
    text.chars().skip(a).take(b.saturating_sub(a)).collect()
}

/// Split one styled line into as many rows as it needs, preserving each span's
/// style across the break. A line that already fits is handed back untouched.
pub fn line<'a>(line: &Line<'a>, width: usize) -> Vec<Line<'a>> {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let rows = rows(&text, width);
    if rows.len() <= 1 {
        return vec![line.clone()];
    }
    rows.iter()
        .map(|&(a, b)| {
            let mut spans = Vec::new();
            let mut at = 0;
            for span in &line.spans {
                let len = span.content.chars().count();
                let (lo, hi) = (a.max(at), b.min(at + len));
                if lo < hi {
                    spans.push(Span::styled(
                        slice(&span.content, lo - at, hi - at),
                        span.style,
                    ));
                }
                at += len;
            }
            Line {
                spans,
                style: line.style,
                alignment: line.alignment,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    fn texts(text: &str, width: usize) -> Vec<String> {
        rows(text, width)
            .iter()
            .map(|&(a, b)| slice(text, a, b))
            .collect()
    }

    #[test]
    fn short_text_is_one_row_and_ranges_cover_every_char() {
        assert_eq!(rows("hello", 10), vec![(0, 5)]);
        assert_eq!(rows("", 10), vec![(0, 0)]);
        assert_eq!(
            rows("exactly!!", 9),
            vec![(0, 9)],
            "an exact fit never wraps"
        );
    }

    #[test]
    fn wrapping_prefers_the_last_space_that_fits() {
        assert_eq!(texts("one two three", 8), vec!["one two ", "three"]);
    }

    #[test]
    fn a_word_longer_than_the_row_is_cut_hard() {
        assert_eq!(texts("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn ranges_are_contiguous_and_total() {
        let text = "the quick brown fox jumps over the lazy dog";
        for width in 1..12 {
            let r = rows(text, width);
            assert_eq!(r[0].0, 0, "width {width}");
            assert_eq!(r.last().unwrap().1, text.chars().count(), "width {width}");
            for pair in r.windows(2) {
                assert_eq!(pair[0].1, pair[1].0, "gap at width {width}");
            }
        }
    }

    #[test]
    fn wide_glyphs_are_measured_in_columns_not_chars() {
        // Each of these is two columns wide, so only two fit in a 5-col row.
        assert_eq!(texts("日本語", 5), vec!["日本", "語"]);
        assert_eq!(columns("日本", 0, 2), 4);
    }

    #[test]
    fn splitting_a_line_keeps_each_spans_style() {
        let src = Line::from(vec![
            Span::styled("aaaa", Style::new().fg(Color::Red)),
            Span::styled("bbbb", Style::new().fg(Color::Blue)),
        ]);
        let out = line(&src, 3);
        let flat: Vec<(String, Option<Color>)> = out
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect();
        assert_eq!(
            flat,
            vec![
                ("aaa".into(), Some(Color::Red)),
                ("a".into(), Some(Color::Red)),
                ("bb".into(), Some(Color::Blue)),
                ("bb".into(), Some(Color::Blue)),
            ]
        );
    }

    #[test]
    fn splitting_carries_the_line_level_style() {
        let src = Line::styled("one two three", Style::new().fg(Color::Green));
        let out = line(&src, 8);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|l| l.style.fg == Some(Color::Green)));
    }
}
