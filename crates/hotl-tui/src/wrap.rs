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
///
/// Walks `char_indices` with a byte cursor instead of collecting a
/// `Vec<char>` — zero allocation beyond the output, and each row re-scans at
/// most its own chars (a space break resumes from the break's byte offset).
/// Differential-tested against the old implementation
/// (`rows_matches_the_reference_implementation`).
pub fn rows(text: &str, width: usize) -> Vec<(usize, usize)> {
    if text.is_empty() || width == 0 {
        return vec![(0, text.chars().count())];
    }
    let mut out = Vec::new();
    let mut row_char = 0usize; // char index where the current row starts
    let mut row_byte = 0usize; // its byte offset
    loop {
        let mut used = 0usize;
        // Char/byte position AFTER the last space seen in this row.
        let mut space: Option<(usize, usize)> = None;
        let mut i = row_char;
        // (char, byte) of the first char that does NOT fit, if any.
        let mut overflow = None;
        for (b, c) in text[row_byte..].char_indices() {
            let w = c.width().unwrap_or(0);
            if used + w > width {
                overflow = Some((i, row_byte + b));
                break;
            }
            used += w;
            if c == ' ' {
                space = Some((i + 1, row_byte + b + 1));
            }
            i += 1;
        }
        let Some((fits_char, fits_byte)) = overflow else {
            // Everything left fits: the trailing row.
            out.push((row_char, i));
            return out;
        };
        // Break after the last space that fits, else cut at the overflow;
        // `max(start + 1)` guarantees progress when even one char overflows.
        let (brk_char, brk_byte) = match space {
            Some(sp) => sp,
            None if fits_char == row_char => {
                let first = text[row_byte..].chars().next().expect("non-empty row");
                (row_char + 1, row_byte + first.len_utf8())
            }
            None => (fits_char, fits_byte),
        };
        out.push((row_char, brk_char));
        (row_char, row_byte) = (brk_char, brk_byte);
        // A break that consumed the final char ends the text with no empty
        // trailing row — the reference's `while start < len` exit.
        if row_byte == text.len() {
            return out;
        }
    }
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

    /// The pre-0033 implementation, verbatim — the oracle the zero-alloc
    /// rewrite is differential-tested against.
    fn rows_reference(text: &str, width: usize) -> Vec<(usize, usize)> {
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

    /// `fit` as it was when `rows_reference` was live code.
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

    #[test]
    fn rows_matches_the_reference_implementation() {
        let corpus = [
            "",
            " ",
            "   leading and trailing   ",
            "the quick brown fox jumps over the lazy dog",
            "averyveryverylongwordwithnospacesatallinit then short",
            "日本語のテキストです これは折り返しのテスト",
            "mixed 日本語 and ascii wrapping テスト here",
            "e\u{301}e\u{301}e\u{301} combining marks e\u{301} everywhere",
            "🦀🦀🦀 emoji 🎉 row 🚀🚀 with spaces",
            "🦀日本a b日本🦀ascii mixed hard",
            "word  double  spaces   triple",
            "\n \n",
        ];
        for text in corpus {
            for width in 1..=120 {
                assert_eq!(
                    rows(text, width),
                    rows_reference(text, width),
                    "text={text:?} width={width}"
                );
            }
            assert_eq!(rows(text, 0), rows_reference(text, 0), "width=0 {text:?}");
        }
    }

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
