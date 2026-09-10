// Client text-selection helpers.

use std::fmt;

/// Selection mode for text selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionMode {
    /// No selection active
    None,
    /// Line-based selection following the terminal's text flow
    Line,
}

impl fmt::Display for SelectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SelectionMode::None => "None",
            SelectionMode::Line => "Line",
        };
        f.write_str(s)
    }
}

/// Normalize selection coordinates into text-flow order so that `start`
/// always comes before `end` (selection direction does not matter).
pub(crate) fn normalized_bounds(
    start: (u16, u16),
    end: (u16, u16),
) -> ((u16, u16), (u16, u16)) {
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

/// Whether cell (`row`, `col`) falls inside the line-based (text-flow)
/// selection between `start` and `end`.
///
/// This is not a box selection: the anchor row is selected from its anchor
/// column to the end of the row, intermediate rows are selected in full, and
/// the last row is selected from column 0 up to its end column. This mirrors
/// how terminals highlight a linear selection.
pub(crate) fn cell_is_selected(
    start: (u16, u16),
    end: (u16, u16),
    row: u16,
    col: u16,
) -> bool {
    let ((sr, sc), (er, ec)) = normalized_bounds(start, end);
    if sr == er {
        return row == sr && col >= sc && col <= ec;
    }
    if row == sr {
        col >= sc
    } else if row == er {
        col <= ec
    } else {
        row > sr && row < er
    }
}

/// Extract the text between two grid coordinates as a line-based (text-flow)
/// selection.
///
/// Both `start` and `end` are inclusive cell coordinates, matching the cells
/// highlighted by [`cell_is_selected`]. The extracted text joins the rows it
/// crosses on newlines (except where the terminal wrapped the line), so it
/// can span multiple lines for clipboard copying. Trailing newlines are
/// trimmed so a selection ending on an empty row does not carry a stray `\n`.
pub(crate) fn extract_selection(screen: &vt100::Screen, start: (u16, u16), end: (u16, u16)) -> String {
    let ((sr, sc), (er, ec)) = normalized_bounds(start, end);
    // `contents_between` treats the end column as exclusive; the selection is
    // inclusive, so extend by one cell.
    let mut out = screen.contents_between(sr, sc, er, ec.saturating_add(1));
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DEFAULT_COLS, DEFAULT_ROWS, SCROLLBACK_LEN};
    use vt100::Parser;

    #[test]
    fn cell_is_selected_single_row_range() {
        let start = (0, 1);
        let end = (0, 4);
        assert!(cell_is_selected(start, end, 0, 1));
        assert!(cell_is_selected(start, end, 0, 3));
        assert!(cell_is_selected(start, end, 0, 4));
        assert!(!cell_is_selected(start, end, 0, 5));
        assert!(!cell_is_selected(start, end, 0, 0));
        assert!(!cell_is_selected(start, end, 1, 2));
    }

    #[test]
    fn cell_is_selected_is_direction_agnostic() {
        let a = (0, 3);
        let b = (2, 5);
        assert!(cell_is_selected(a, b, 0, 3));
        assert!(cell_is_selected(a, b, 0, 70));
        assert!(cell_is_selected(b, a, 0, 3));
        assert!(cell_is_selected(a, b, 1, 0));
        assert!(cell_is_selected(a, b, 1, 79));
        assert!(cell_is_selected(a, b, 2, 5));
        assert!(!cell_is_selected(a, b, 2, 6));
        assert!(!cell_is_selected(a, b, 3, 0));
    }

    #[test]
    fn cell_is_selected_middle_rows_are_fully_selected() {
        assert!(cell_is_selected((0, 4), (3, 2), 1, 0));
        assert!(cell_is_selected((0, 4), (3, 2), 2, 79));
    }

    #[test]
    fn cell_is_selected_degenerate_single_cell() {
        assert!(cell_is_selected((2, 2), (2, 2), 2, 2));
        assert!(!cell_is_selected((2, 2), (2, 2), 2, 3));
    }

    #[test]
    fn extract_selection_reads_cells_in_order() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"hello\r\nworld");
        let out = extract_selection(parser.screen(), (0, 0), (1, 10));
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn extract_selection_copies_across_lines() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"hello\r\nworld");
        let out = extract_selection(parser.screen(), (0, 0), (1, 5));
        assert_eq!(out, "hello\nworld");
    }

    #[test]
    fn extract_selection_copies_middle_rows_in_full() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"one\r\ntwo\r\nthree\r\nfour");
        let out = extract_selection(parser.screen(), (0, 0), (3, 4));
        assert_eq!(out, "one\ntwo\nthree\nfour");
    }

    #[test]
    fn extract_selection_backwards_equals_forward() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"abcdef");
        let screen = parser.screen();
        let forward = extract_selection(&screen, (0, 1), (0, 4));
        let backward = extract_selection(&screen, (0, 4), (0, 1));
        assert_eq!(forward, backward);
        assert_eq!(forward, "bcde");
    }

    #[test]
    fn extract_selection_single_cell_copies_contents() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"abcdef");
        let screen = parser.screen();
        assert_eq!(extract_selection(&screen, (0, 2), (0, 2)), "c");
    }

    #[test]
    fn extract_selection_trims_trailing_newline() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"hello\r\nworld\r\n");
        let out = extract_selection(parser.screen(), (0, 0), (2, 0));
        assert_eq!(out, "hello\nworld");
    }

    #[test]
    fn extract_selection_joins_wrapped_lines_without_newline() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        let mut feed = String::new();
        for _ in 0..DEFAULT_COLS {
            feed.push('a');
        }
        feed.push_str("hello");
        parser.process(feed.as_bytes());
        let out = extract_selection(parser.screen(), (0, 0), (1, 5));
        assert!(!out.contains('\n'));
        assert!(out.starts_with('a'));
        assert!(out.ends_with("hello"));
    }
}
