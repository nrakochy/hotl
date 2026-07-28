//! Mouse drag-selection: geometry and the buffer scrape.
//!
//! A selection is a *screen region*, not a text range. It holds terminal cell
//! coordinates, `view` paints the highlight at those cells, and the copy reads
//! the rendered buffer back — so what is highlighted and what is copied cannot
//! disagree, even while a streaming turn moves text underneath it.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Where the left button went down, and where the pointer is now. Both are
/// absolute terminal cells. `head` moves with every drag event; `anchor` does
/// not move until the next button press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl Selection {
    /// A fresh selection at the button-down cell: anchor and head coincide,
    /// so it is empty until the pointer moves.
    pub fn new(col: u16, row: u16) -> Self {
        Selection {
            anchor: (col, row),
            head: (col, row),
        }
    }

    /// A click that never dragged. Copying one of these would clobber the
    /// clipboard every time the user clicked the window, so callers must not.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// `(start, end)` in reading order, so a backwards or upward drag reduces
    /// to the same span as the equivalent forward one. Row dominates: the cell
    /// on the earlier row starts the span whatever its column.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (ax, ay) = self.anchor;
        let (hx, hy) = self.head;
        if (ay, ax) <= (hy, hx) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Is this cell inside the selection? The geometry is *flowing*, like a
    /// terminal's own drag-select: the first row runs from the start column to
    /// the right edge, whole rows in between are inside, and the last row stops
    /// at the end column. It is deliberately not a rectangle.
    pub fn contains(&self, col: u16, row: u16) -> bool {
        let ((sx, sy), (ex, ey)) = self.ordered();
        if row < sy || row > ey {
            return false;
        }
        let lo = if row == sy { sx } else { 0 };
        let hi = if row == ey { ex } else { u16::MAX };
        col >= lo && col <= hi
    }
}

/// The text under a selection, read back out of the rendered buffer.
///
/// `trim_rows` is the transcript pane and `text_col` the column its prose
/// starts at (`Spine::wrap` puts it at `gutter + 2`). A selected row inside
/// that pane whose span reaches the spine is trimmed to `text_col`, so
/// dragging across a paragraph yields the prose without the gutter pad and
/// role glyph. Rows outside the pane — the input box, the hint — are taken
/// verbatim, and a span that already starts right of `text_col` is left alone.
///
/// Returns empty for an all-whitespace region: callers must not write that to
/// the clipboard.
pub fn region_text(buf: &Buffer, sel: &Selection, trim_rows: Rect, text_col: u16) -> String {
    let ((start_col, start_row), (end_col, end_row)) = sel.ordered();
    let area = buf.area;
    let (left, right) = (area.x, area.right().saturating_sub(1));
    let mut rows: Vec<String> = Vec::new();

    for row in area.y..area.bottom() {
        if row < start_row || row > end_row {
            continue;
        }
        // Flowing, not rectangular: only the first and last rows are bounded
        // by the drag's own columns.
        let first = if row == start_row { start_col } else { left };
        let last = if row == end_row { end_col } else { right };
        let (mut lo, hi) = (first.max(left), last.min(right));
        let in_transcript = row >= trim_rows.y && row < trim_rows.bottom();
        if in_transcript && lo <= text_col {
            lo = text_col;
        }
        let mut text = String::new();
        if lo <= hi {
            for col in lo..=hi {
                if let Some(cell) = buf.cell((col, row)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        rows.push(text.trim_end().to_string());
    }

    // Trailing blank rows would copy as a trailing newline; a selection made
    // entirely of them copies as nothing at all.
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rendered screen: two transcript rows behind a `gutter = 1` spine
    /// (prose starts at column 3), the input box, and a blank row.
    fn screen() -> Buffer {
        Buffer::with_lines([" | alpha beta", " | gamma", "> input text", ""])
    }

    /// The transcript pane of `screen()` — rows 0 and 1 only.
    fn transcript() -> Rect {
        Rect::new(0, 0, 13, 2)
    }

    /// Column the spine hands prose over at, for `gutter = 1`.
    const TEXT_COL: u16 = 3;

    fn copied(sel: &Selection) -> String {
        region_text(&screen(), sel, transcript(), TEXT_COL)
    }

    /// A drag from `(ax, ay)` to `(hx, hy)`.
    fn drag(ax: u16, ay: u16, hx: u16, hy: u16) -> Selection {
        Selection {
            anchor: (ax, ay),
            head: (hx, hy),
        }
    }

    #[test]
    fn a_click_that_never_dragged_is_empty() {
        assert!(Selection::new(4, 2).is_empty());
    }

    #[test]
    fn moving_the_head_one_cell_makes_it_non_empty() {
        assert!(!drag(4, 2, 5, 2).is_empty());
    }

    #[test]
    fn a_single_row_selection_spans_only_the_dragged_columns() {
        let s = drag(3, 1, 6, 1);
        assert!(!s.contains(2, 1), "left of the start column is outside");
        assert!(s.contains(3, 1));
        assert!(s.contains(6, 1));
        assert!(!s.contains(7, 1), "right of the end column is outside");
        assert!(!s.contains(4, 0), "a different row is outside");
    }

    #[test]
    fn the_first_row_of_a_multi_row_selection_runs_to_the_right_edge() {
        let s = drag(5, 1, 2, 3);
        assert!(
            !s.contains(4, 1),
            "left of the start column is still outside"
        );
        assert!(s.contains(5, 1));
        assert!(s.contains(u16::MAX, 1), "the first row has no right bound");
    }

    #[test]
    fn middle_rows_of_a_multi_row_selection_are_entirely_inside() {
        let s = drag(5, 1, 2, 3);
        assert!(s.contains(0, 2));
        assert!(s.contains(u16::MAX, 2));
    }

    #[test]
    fn the_last_row_of_a_multi_row_selection_stops_at_the_end_column() {
        let s = drag(5, 1, 2, 3);
        assert!(s.contains(0, 3), "the last row has no left bound");
        assert!(s.contains(2, 3));
        assert!(!s.contains(3, 3));
        assert!(!s.contains(0, 4), "past the last row is outside");
    }

    #[test]
    fn a_backwards_drag_selects_the_same_span_as_the_forward_one() {
        let forward = drag(3, 1, 6, 1);
        let backward = drag(6, 1, 3, 1);
        for col in 0..9u16 {
            assert_eq!(
                forward.contains(col, 1),
                backward.contains(col, 1),
                "column {col} disagrees"
            );
        }
    }

    #[test]
    fn an_upward_drag_selects_the_same_span_as_the_downward_one() {
        let down = drag(5, 1, 2, 3);
        let up = drag(2, 3, 5, 1);
        for row in 0..5u16 {
            for col in [0u16, 3, 5, 9] {
                assert_eq!(
                    down.contains(col, row),
                    up.contains(col, row),
                    "cell ({col},{row}) disagrees"
                );
            }
        }
    }

    #[test]
    fn ordered_puts_the_earlier_cell_first() {
        assert_eq!(drag(6, 3, 2, 1).ordered(), ((2, 1), (6, 3)));
        assert_eq!(drag(2, 1, 6, 3).ordered(), ((2, 1), (6, 3)));
    }

    #[test]
    fn dragging_a_transcript_row_from_the_left_edge_trims_the_spine() {
        assert_eq!(copied(&drag(0, 0, 12, 0)), "alpha beta");
    }

    #[test]
    fn a_span_starting_right_of_the_text_column_is_not_trimmed() {
        // Column 5 is the `p` of "alpha" — the drag started mid-word and must
        // be honored exactly.
        assert_eq!(copied(&drag(5, 0, 12, 0)), "pha beta");
    }

    #[test]
    fn rows_outside_the_transcript_keep_their_own_columns() {
        // The input box has no spine, so nothing may be shaved off its left.
        assert_eq!(copied(&drag(0, 2, 12, 2)), "> input text");
    }

    #[test]
    fn trailing_padding_cells_are_dropped() {
        // Row 1 is shorter than the buffer, so cells 8..12 are blank padding.
        assert_eq!(copied(&drag(0, 1, 12, 1)), "gamma");
    }

    #[test]
    fn a_whitespace_only_region_copies_nothing() {
        assert_eq!(copied(&drag(0, 3, 12, 3)), "");
    }

    #[test]
    fn rows_join_with_newlines_and_leave_no_trailing_newline() {
        assert_eq!(copied(&drag(3, 0, 12, 1)), "alpha beta\ngamma");
    }

    #[test]
    fn a_span_past_the_buffer_edge_is_clamped() {
        // `contains` reports u16::MAX for every non-final row, and a drag can
        // end past the last column; scraping must not index outside the buffer.
        assert_eq!(copied(&drag(0, 0, 999, 1)), "alpha beta\ngamma");
    }
}
