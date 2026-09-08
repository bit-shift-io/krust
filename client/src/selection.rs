// Client text-selection helpers.

use std::fmt;

/// Selection mode for text selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionMode {
    /// No selection active
    None,
    /// Linear (text-flow) selection
    Linear,
}

impl fmt::Display for SelectionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SelectionMode::None => "None",
            SelectionMode::Linear => "Linear",
        };
        f.write_str(s)
    }
}

/// Extract the text between two grid coordinates.
///
/// The coordinates are normalized (anchor first) so selection direction does
/// not matter. Returns a newly-allocated string.
pub(crate) fn extract_selection(screen: &vt100::Screen, start: (u16, u16), end: (u16, u16)) -> String {
    let (start_row, start_col) = start;
    let (end_row, end_col) = end;
    let (a_r, a_c, b_r, b_c) = if (start_row, start_col) <= (end_row, end_col) {
        (start_row, start_col, end_row, end_col)
    } else {
        (end_row, end_col, start_row, start_col)
    };
    screen.contents_between(a_r, a_c, b_r, b_c)
}
