use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::agent::AgentId;
use crate::ipc::protocol::{ColorDto, GridSnapshot};
use crate::session::SessionManager;

/// Where `render_term_pane` gets the selected agent's terminal content from.
/// Mock mode holds a live `Term` locally (`Local`); real (daemon-attached)
/// mode only ever has the last `GridSnapshot` the daemon streamed for the
/// watched agent (`Remote`) — there's no local `Term` to read from across
/// the process boundary (see `session::pty` for why raw bytes aren't
/// streamed instead).
pub enum PaneSource<'a> {
    Local(&'a SessionManager),
    Remote(Option<&'a GridSnapshot>),
}

/// Renders the selected agent's live terminal grid into `area` — the pane
/// half of the tree|pane split. `focused` draws the cursor
/// and a distinct border; read-only preview otherwise. Returns the inner
/// (border-excluded) rect actually painted, so callers can size the PTY to it.
pub fn render_term_pane(
    frame: &mut Frame,
    area: Rect,
    source: &PaneSource,
    selected: Option<&AgentId>,
    focused: bool,
) -> Rect {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if focused { " agent [FOCUSED — Ctrl-h to leave] " } else { " agent " });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(id) = selected else {
        frame.render_widget(placeholder("no agent selected"), inner);
        return inner;
    };

    let painted = match source {
        PaneSource::Local(sessions) => {
            sessions.with_term(id, |term| paint_term(term, inner, frame.buffer_mut(), focused)).is_some()
        }
        PaneSource::Remote(grid) => match grid {
            Some(grid) => {
                paint_grid_snapshot(grid, inner, frame.buffer_mut(), focused);
                true
            }
            None => false,
        },
    };
    if !painted {
        frame.render_widget(placeholder("agent not running"), inner);
    }
    inner
}

fn placeholder(text: &'static str) -> Paragraph<'static> {
    Paragraph::new(text).style(Style::default().fg(Color::DarkGray))
}

/// Pure grid->buffer painter — the direct unit-test seam: feed
/// canned escape sequences into a `Term`, call this, assert buffer cells.
/// Generic over `EventListener` so tests can use `VoidListener` without
/// constructing a real `EventProxy`.
pub fn paint_term<T: EventListener>(term: &Term<T>, area: Rect, buf: &mut Buffer, show_cursor: bool) {
    let content = term.renderable_content();
    let cols = area.width as usize;
    let lines = area.height as usize;

    for cell in content.display_iter {
        let point = cell.point;
        if point.line.0 < 0 {
            continue;
        }
        let row = point.line.0 as usize;
        let col = point.column.0;
        if row >= lines || col >= cols {
            continue;
        }

        // Wide chars occupy two grid cells; the spacer cell renders nothing
        // (drawing it would double-print and shear the following column).
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let x = area.x + col as u16;
        let y = area.y + row as u16;
        let ch = if cell.c == '\0' { ' ' } else { cell.c };
        let mut style = Style::default().fg(map_color(cell.fg)).bg(map_color(cell.bg));

        if cell.flags.contains(Flags::BOLD) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if cell.flags.contains(Flags::ITALIC) {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if cell.flags.contains(Flags::UNDERLINE) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if cell.flags.contains(Flags::INVERSE) {
            style = style.add_modifier(Modifier::REVERSED);
        }

        let target = &mut buf[(x, y)];
        target.set_char(ch);
        target.set_style(style);
    }

    if show_cursor {
        let cursor = content.cursor.point;
        if cursor.line.0 >= 0 {
            let row = cursor.line.0 as usize;
            let col = cursor.column.0;
            if row < lines && col < cols {
                let x = area.x + col as u16;
                let y = area.y + row as u16;
                buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

fn map_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(idx) => Color::Indexed(idx),
        AnsiColor::Named(named) => match named {
            NamedColor::Black => Color::Black,
            NamedColor::Red => Color::Red,
            NamedColor::Green => Color::Green,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::White => Color::White,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite => Color::White,
            _ => Color::Reset,
        },
    }
}

/// The wire-side twin of `map_color`, above — converts the daemon's
/// `ColorDto` (built server-side by `session::pty::dto_color`) back into a
/// `ratatui::style::Color`. A mechanical 1:1 mapping since `ColorDto` was
/// deliberately shaped to mirror `Color`'s own variants.
fn map_dto_color(color: ColorDto) -> Color {
    match color {
        ColorDto::Reset => Color::Reset,
        ColorDto::Black => Color::Black,
        ColorDto::Red => Color::Red,
        ColorDto::Green => Color::Green,
        ColorDto::Yellow => Color::Yellow,
        ColorDto::Blue => Color::Blue,
        ColorDto::Magenta => Color::Magenta,
        ColorDto::Cyan => Color::Cyan,
        ColorDto::Gray => Color::Gray,
        ColorDto::DarkGray => Color::DarkGray,
        ColorDto::LightRed => Color::LightRed,
        ColorDto::LightGreen => Color::LightGreen,
        ColorDto::LightYellow => Color::LightYellow,
        ColorDto::LightBlue => Color::LightBlue,
        ColorDto::LightMagenta => Color::LightMagenta,
        ColorDto::LightCyan => Color::LightCyan,
        ColorDto::White => Color::White,
        ColorDto::Rgb(r, g, b) => Color::Rgb(r, g, b),
        ColorDto::Indexed(idx) => Color::Indexed(idx),
    }
}

/// Paints a `GridSnapshot` (the daemon's rendered-grid DTO) into `buf` —
/// the `Remote`-source twin of `paint_term`, same cell-by-cell styling, just
/// reading from a plain data snapshot instead of a live `Term`.
pub fn paint_grid_snapshot(grid: &GridSnapshot, area: Rect, buf: &mut Buffer, show_cursor: bool) {
    let cols = area.width as usize;
    let lines = area.height as usize;

    for row in 0..(grid.lines as usize).min(lines) {
        for col in 0..(grid.cols as usize).min(cols) {
            let Some(Some(cell)) = grid.cells.get(row * grid.cols as usize + col) else { continue };
            let x = area.x + col as u16;
            let y = area.y + row as u16;
            let mut style = Style::default().fg(map_dto_color(cell.fg)).bg(map_dto_color(cell.bg));
            if cell.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let target = &mut buf[(x, y)];
            target.set_char(cell.ch);
            target.set_style(style);
        }
    }

    if show_cursor {
        if let Some((row, col)) = grid.cursor {
            let (row, col) = (row as usize, col as usize);
            if row < lines && col < cols {
                let x = area.x + col as u16;
                let y = area.y + row as u16;
                buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vte::ansi::Processor;

    #[derive(Clone, Copy)]
    struct TestSize {
        cols: usize,
        lines: usize,
    }

    impl Dimensions for TestSize {
        fn total_lines(&self) -> usize {
            self.lines
        }
        fn screen_lines(&self) -> usize {
            self.lines
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn term_from(bytes: &[u8], cols: usize, lines: usize) -> Term<VoidListener> {
        let size = TestSize { cols, lines };
        let mut term = Term::new(TermConfig::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    #[test]
    fn plain_text_renders_into_top_left() {
        let term = term_from(b"hi", 10, 3);
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_term(&term, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "i");
        assert_eq!(buf[(2, 0)].symbol(), " ");
    }

    #[test]
    fn sgr_bold_and_color_are_mapped() {
        // \x1b[1;31m = bold + red foreground
        let term = term_from(b"\x1b[1;31mX", 10, 3);
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_term(&term, area, &mut buf, false);
        let cell = &buf[(0, 0)];
        assert_eq!(cell.symbol(), "X");
        assert_eq!(cell.fg, Color::Red);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wide_char_spacer_is_not_drawn_over() {
        // U+4F60 ("你") is a double-width CJK character.
        let term = term_from("你".as_bytes(), 10, 3);
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_term(&term, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "你");
        // The spacer cell must stay untouched (default blank), not a stray
        // second glyph — this is the classic wide-char column-shear bug.
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn cursor_is_drawn_only_when_requested() {
        let term = term_from(b"a", 10, 3);
        let area = Rect::new(0, 0, 10, 3);

        let mut buf_no_cursor = Buffer::empty(area);
        paint_term(&term, area, &mut buf_no_cursor, false);
        assert!(!buf_no_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));

        let mut buf_cursor = Buffer::empty(area);
        paint_term(&term, area, &mut buf_cursor, true);
        assert!(buf_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn newline_moves_to_next_row() {
        let term = term_from(b"a\r\nb", 10, 3);
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_term(&term, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(0, 1)].symbol(), "b");
    }

    #[test]
    fn content_outside_area_bounds_is_clipped_not_panicking() {
        // A 3-column-tall/wide area smaller than the term's own grid: cells
        // beyond `area` must be skipped, not indexed out of bounds.
        let term = term_from(b"hello world this overflows", 30, 3);
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        paint_term(&term, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(4, 0)].symbol(), "o");
    }
}
