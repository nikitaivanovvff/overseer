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
use crate::session::SessionManager;

/// Renders the selected agent's live terminal grid into `area` — the pane
/// half of the tree|pane split (PHASE6.md §3.5). `focused` draws the cursor
/// and a distinct border; read-only preview otherwise.
pub fn render_term_pane(
    frame: &mut Frame,
    area: Rect,
    sessions: &SessionManager,
    selected: Option<&AgentId>,
    focused: bool,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if focused { " agent [FOCUSED — Ctrl-h to leave] " } else { " agent " });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(id) = selected else {
        frame.render_widget(placeholder("no agent selected"), inner);
        return;
    };

    let painted = sessions.with_term(id, |term| paint_term(term, inner, frame.buffer_mut(), focused));
    if painted.is_none() {
        frame.render_widget(placeholder("agent not running"), inner);
    }
}

fn placeholder(text: &'static str) -> Paragraph<'static> {
    Paragraph::new(text).style(Style::default().fg(Color::DarkGray))
}

/// Pure grid->buffer painter — the direct unit-test seam (PHASE6.md §5): feed
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
