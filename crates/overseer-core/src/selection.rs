//! Pure logic for mouse-driven pane text selection and its OSC 52 clipboard
//! copy — the mouse-native counterpart to `session::keys::encode_mouse_wheel`.
//!
//! Once `crossterm::event::EnableMouseCapture` is armed (`tui.rs`, for the
//! whole TUI lifetime — see AGENTS.md's Scrollback section on why that's
//! permanent), the host terminal never sees a raw click/drag as its own
//! gesture; every one arrives at Overseer as a `MouseEvent` instead, the same
//! way a wheel notch does. Scrolling already answers that by owning the
//! event and either moving Overseer's own history offset or re-encoding a
//! report for the focused inner PTY. Selection answers it the same way:
//! Overseer tracks the drag itself against the `GridSnapshot` it's already
//! rendering, and on release, copies the selected text out via OSC 52
//! instead of relying on a terminal-native selection that mouse capture
//! makes unreachable. This is the same trick tmux/Neovim/kitty use to offer
//! real clipboard copy while still owning the whole mouse stream.

use base64::Engine;

use crate::ipc::protocol::GridSnapshot;

/// A grid-local point, `(col, row)` — the same field order as
/// `crossterm::MouseEvent`'s own `(column, row)`, so callers translating a
/// raw event into pane-local coordinates don't need to transpose anything.
pub type GridPoint = (u16, u16);

/// Orders two points into `(start, end)` in reading order (row-major, then
/// column) — whichever of anchor/cursor the drag actually ran from/to,
/// extraction and hit-testing both always walk forward from `start`.
fn ordered(a: GridPoint, b: GridPoint) -> (GridPoint, GridPoint) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Whether `point` falls inside the reading-order span between `anchor` and
/// `cursor`, inclusive — a full row is "selected" between the start and end
/// rows, matching ordinary terminal stream-selection (not the block/column
/// selection some terminals offer as a modifier variant).
pub fn contains(anchor: GridPoint, cursor: GridPoint, point: GridPoint) -> bool {
    let (start, end) = ordered(anchor, cursor);
    let (col, row) = point;
    if row < start.1 || row > end.1 {
        return false;
    }
    if start.1 == end.1 {
        return col >= start.0 && col <= end.0;
    }
    if row == start.1 {
        return col >= start.0;
    }
    if row == end.1 {
        return col <= end.0;
    }
    true
}

/// Reconstructs the selected text between `anchor` and `cursor` (inclusive),
/// clamped to `grid`'s own current bounds — a live pane can redraw or resize
/// mid-drag, so points recorded against an earlier frame must not index out
/// of bounds against a fresher one. Rows are newline-joined; each row's
/// trailing padding (unwritten cells render as space, same as a blank/spacer
/// `None` cell) is trimmed, matching mainstream terminal copy behavior.
pub fn extract_text(grid: &GridSnapshot, anchor: GridPoint, cursor: GridPoint) -> String {
    let cols = grid.cols;
    let lines = grid.lines;
    if cols == 0 || lines == 0 {
        return String::new();
    }
    let clamp = |(col, row): GridPoint| (col.min(cols - 1), row.min(lines - 1));
    let (start, end) = ordered(clamp(anchor), clamp(cursor));

    let mut out = String::new();
    for row in start.1..=end.1 {
        let row_start = if row == start.1 { start.0 } else { 0 };
        let row_end = if row == end.1 { end.0 } else { cols - 1 };
        let mut line = String::with_capacity((row_end - row_start + 1) as usize);
        for col in row_start..=row_end {
            let idx = row as usize * cols as usize + col as usize;
            let ch = grid.cells.get(idx).and_then(|c| c.as_ref()).map(|c| c.ch).unwrap_or(' ');
            line.push(ch);
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out
}

/// Wraps `text` as an OSC 52 clipboard-set escape sequence, BEL-terminated.
/// Terminals that don't recognize OSC 52 just ignore an unrecognized escape
/// sequence, so writing this is always safe — no capability negotiation
/// needed. `crate::notify::ring_bell` is the sibling precedent for writing a
/// raw control sequence straight to this process's own stdout outside
/// ratatui's own buffered draw.
pub fn osc52_copy(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::{CellDto, ColorDto};

    fn grid(cols: u16, lines: u16, rows: &[&str]) -> GridSnapshot {
        let mut cells = vec![None; cols as usize * lines as usize];
        for (row, text) in rows.iter().enumerate() {
            for (col, ch) in text.chars().enumerate() {
                cells[row * cols as usize + col] = Some(CellDto {
                    ch,
                    fg: ColorDto::Reset,
                    bg: ColorDto::Reset,
                    bold: false,
                    italic: false,
                    underline: false,
                    inverse: false,
                });
            }
        }
        GridSnapshot {
            cols,
            lines,
            cells,
            cursor: None,
            app_cursor_mode: false,
            bracketed_paste_mode: false,
            mouse_reporting_mode: false,
            sgr_mouse_mode: false,
            utf8_mouse_mode: false,
            display_offset: 0,
        }
    }

    #[test]
    fn single_row_selection_extracts_the_span() {
        let g = grid(10, 2, &["hello world"]);
        assert_eq!(extract_text(&g, (0, 0), (4, 0)), "hello");
    }

    #[test]
    fn reversed_drag_direction_extracts_the_same_text() {
        let g = grid(10, 2, &["hello world"]);
        assert_eq!(extract_text(&g, (4, 0), (0, 0)), extract_text(&g, (0, 0), (4, 0)));
    }

    #[test]
    fn multi_row_selection_joins_with_newline_and_trims_padding() {
        let g = grid(10, 3, &["hello", "world"]);
        assert_eq!(extract_text(&g, (3, 0), (2, 1)), "lo\nwor");
    }

    #[test]
    fn full_middle_row_is_included_entirely() {
        let g = grid(6, 3, &["aaa", "bbb", "ccc"]);
        assert_eq!(extract_text(&g, (0, 0), (2, 2)), "aaa\nbbb\nccc");
    }

    #[test]
    fn out_of_bounds_points_clamp_to_the_current_grid_instead_of_panicking() {
        let g = grid(5, 1, &["ab"]);
        assert_eq!(extract_text(&g, (0, 0), (99, 99)), "ab");
    }

    #[test]
    fn out_of_bounds_row_clamps_to_the_last_real_row_not_the_first() {
        // Distinct from the single-line case above: clamping must land on
        // the grid's actual last row, so a genuinely blank second line still
        // contributes its own (empty) line to the join.
        let g = grid(5, 2, &["ab"]);
        assert_eq!(extract_text(&g, (0, 0), (99, 99)), "ab\n");
    }

    #[test]
    fn empty_grid_dimensions_return_empty_text() {
        let g = grid(0, 0, &[]);
        assert_eq!(extract_text(&g, (0, 0), (0, 0)), "");
    }

    #[test]
    fn unwritten_cells_render_as_a_single_trimmed_space() {
        let g = grid(10, 1, &["a"]);
        // Selecting past the written text pulls in unwritten (`None`) cells —
        // they must not panic and must not survive trailing-trim as content.
        assert_eq!(extract_text(&g, (0, 0), (4, 0)), "a");
    }

    #[test]
    fn contains_matches_single_row_span() {
        assert!(contains((2, 0), (5, 0), (3, 0)));
        assert!(!contains((2, 0), (5, 0), (1, 0)));
        assert!(!contains((2, 0), (5, 0), (6, 0)));
    }

    #[test]
    fn contains_matches_multi_row_span_edges() {
        let (anchor, cursor) = ((3, 0), (2, 2));
        assert!(contains(anchor, cursor, (9, 0)), "rest of the start row");
        assert!(contains(anchor, cursor, (0, 1)), "a fully-included middle row");
        assert!(contains(anchor, cursor, (2, 2)), "up to the end row's cursor col");
        assert!(!contains(anchor, cursor, (3, 2)), "past the end row's cursor col");
        assert!(!contains(anchor, cursor, (2, 0)), "before the start row's anchor col");
    }

    #[test]
    fn contains_is_order_independent() {
        assert!(contains((5, 2), (1, 0), (3, 1)));
    }

    #[test]
    fn osc52_wraps_base64_with_bel_terminator() {
        let seq = osc52_copy("hi");
        assert_eq!(seq, "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn osc52_roundtrips_through_base64() {
        let seq = osc52_copy("hello world\nsecond line");
        let inner = seq.strip_prefix("\x1b]52;c;").unwrap().strip_suffix('\x07').unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(inner).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "hello world\nsecond line");
    }
}
