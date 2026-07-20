use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use overseer_core::agent::FlatNode;
use overseer_core::ipc::protocol::{ColorDto, GridSnapshot};

/// Renders the selected agent's live terminal grid into `area` — the pane
/// half of the tree|pane split. `grid` is the selected agent's current
/// rendered content, `None` if it has none yet (or isn't running); `--mock`
/// and daemon-attached modes both feed this from `App::pane_grid` — `ui/`
/// never sees anything backend-specific. `focused` draws the cursor and a
/// distinct border; read-only preview otherwise. `selection`, when present,
/// is a mouse-native text selection's `(anchor, cursor)` in pane-local
/// `(col, row)` — see `overseer_core::selection` for why this stands in for
/// terminal-native drag-select (mouse capture is permanently armed, so the
/// host terminal never gets the drag). Returns the inner (border-excluded)
/// rect actually painted, so callers can size the PTY to it.
pub fn render_term_pane(
    frame: &mut Frame,
    area: Rect,
    grid: Option<&GridSnapshot>,
    selected: Option<&FlatNode>,
    focused: bool,
    selection: Option<((u16, u16), (u16, u16))>,
) -> Rect {
    let offset = grid.map(|g| g.display_offset).unwrap_or(0);
    // A session that exits naturally (not via `d`/`D`) keeps its last
    // rendered content around for review — by design (AGENTS.md Cleanup),
    // not a bug — but with no marker at all, a pane the user is still
    // looking at (or focused into) just silently stops responding with
    // zero explanation, which a real user reported as the pane having
    // "frozen". `alive` reads `FlatNode::session_alive` directly — the same
    // ground-truth signal `App::is_alive` uses — rather than re-deriving it
    // from `status`: a self-reported `Done`/`Error` agent whose session is
    // still running (e.g. re-prompted after saying "done") is not "exited".
    let alive = selected.is_some_and(|n| n.session_alive);
    let block = Block::default().borders(Borders::ALL).title(pane_title(focused, offset, alive));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if selected.is_none() {
        frame.render_widget(placeholder("no agent selected"), inner);
        return inner;
    }

    match grid {
        Some(grid) => paint_grid_snapshot(grid, inner, frame.buffer_mut(), focused, selection),
        None => frame.render_widget(placeholder("agent not running"), inner),
    }
    inner
}

fn placeholder(text: impl Into<String>) -> Paragraph<'static> {
    Paragraph::new(text.into()).style(Style::default().fg(Color::DarkGray))
}

/// Pure — the pane border's title. `alive` wins over everything else that
/// isn't the escape hint: a dead session's pane otherwise looks identical to
/// a live one that's just momentarily quiet, which read as a frozen
/// terminal to a real user who typed `exit` while jumped in and got no
/// further feedback at all.
///
/// Focused and scrolled are no longer mutually exclusive (SCROLLBACK.md):
/// the mouse wheel scrolls a focused pane too, so a jumped-in agent can sit
/// mid-scrollback — the title must say so, or scrolled-but-focused looks
/// identical to live-and-focused with no way to tell you're not looking at
/// the tail. `G` isn't offered as the way back while focused since it's a
/// key that forwards straight to the agent there (only `Ctrl-h` is
/// intercepted) — "scroll to follow" describes the one thing that *is*
/// guaranteed to work in both states.
fn pane_title(focused: bool, display_offset: usize, alive: bool) -> String {
    match (focused, alive) {
        (true, true) if display_offset > 0 => {
            format!(" agent [FOCUSED, scrolled ↑{display_offset} — scroll to follow] ")
        }
        (true, true) => " agent [FOCUSED — Ctrl-h to leave] ".to_string(),
        (true, false) => " agent [exited — Ctrl-h to leave] ".to_string(),
        (false, false) => " agent [exited] ".to_string(),
        (false, true) if display_offset > 0 => format!(" agent [scrolled ↑{display_offset} — G to follow] "),
        (false, true) => " agent ".to_string(),
    }
}

/// Converts the daemon's `ColorDto` (built server-side by
/// `session::pty::dto_color`) into a `ratatui::style::Color`. A mechanical
/// 1:1 mapping since `ColorDto` was deliberately shaped to mirror `Color`'s
/// own variants.
///
/// `pub(crate)`, not private: `ui/mod.rs` also uses it to convert
/// `config::Theme`'s `ColorDto` fields (`overseer-core` is deliberately
/// `ratatui`-free — see the workspace-split notes in AGENTS.md — so `Theme`
/// carries `ColorDto`, not `ratatui::style::Color`, and this one mapping now
/// serves both grid cells and theme colors).
pub(crate) fn map_dto_color(color: ColorDto) -> Color {
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

/// Paints a `GridSnapshot` into `buf`, cell by cell — the only painter in
/// `ui/`, for both `--mock` and daemon-attached modes (`GridSnapshot` is the
/// only render currency; see `session::pty::grid_snapshot_from_term` for how
/// it's built). `selection`, when present, reverse-highlights every cell
/// `overseer_core::selection::contains` reports as within its span — the
/// same visual treatment the cursor gets below, so a selected range reads
/// the same way a real terminal's own inverse-video selection does.
pub fn paint_grid_snapshot(
    grid: &GridSnapshot,
    area: Rect,
    buf: &mut Buffer,
    show_cursor: bool,
    selection: Option<((u16, u16), (u16, u16))>,
) {
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
            if let Some((anchor, cursor)) = selection {
                if overseer_core::selection::contains(anchor, cursor, (col as u16, row as u16)) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
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
    use overseer_core::agent::AgentStatus;
    use overseer_core::session::snapshot_from_bytes;

    #[test]
    fn plain_text_renders_into_top_left() {
        let grid = snapshot_from_bytes(10, 3, b"hi");
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false, None);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "i");
        assert_eq!(buf[(2, 0)].symbol(), " ");
    }

    #[test]
    fn sgr_bold_and_color_are_mapped() {
        // \x1b[1;31m = bold + red foreground
        let grid = snapshot_from_bytes(10, 3, b"\x1b[1;31mX");
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false, None);
        let cell = &buf[(0, 0)];
        assert_eq!(cell.symbol(), "X");
        assert_eq!(cell.fg, Color::Red);
        assert!(cell.modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wide_char_spacer_is_not_drawn_over() {
        // U+4F60 ("你") is a double-width CJK character.
        let grid = snapshot_from_bytes(10, 3, "你".as_bytes());
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false, None);
        assert_eq!(buf[(0, 0)].symbol(), "你");
        // The spacer cell must stay untouched (default blank), not a stray
        // second glyph — this is the classic wide-char column-shear bug.
        assert_eq!(buf[(1, 0)].symbol(), " ");
    }

    #[test]
    fn cursor_is_drawn_only_when_requested() {
        let grid = snapshot_from_bytes(10, 3, b"a");
        let area = Rect::new(0, 0, 10, 3);

        let mut buf_no_cursor = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf_no_cursor, false, None);
        assert!(!buf_no_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));

        let mut buf_cursor = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf_cursor, true, None);
        assert!(buf_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn newline_moves_to_next_row() {
        let grid = snapshot_from_bytes(10, 3, b"a\r\nb");
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false, None);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(0, 1)].symbol(), "b");
    }

    #[test]
    fn content_outside_area_bounds_is_clipped_not_panicking() {
        // A 3-column-tall/wide area smaller than the grid's own dimensions:
        // cells beyond `area` must be skipped, not indexed out of bounds.
        let grid = snapshot_from_bytes(30, 3, b"hello world this overflows");
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false, None);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(4, 0)].symbol(), "o");
    }

    // ── scrollback (SCROLLBACK.md) ───────────────────────────────────────────

    #[test]
    fn scroll_display_shows_older_lines_then_bottom_restores_live_content() {
        use overseer_core::session::snapshot_from_bytes_scrolled;

        // 5 lines of visible height, but print far more so there's real
        // scrollback history to move into.
        let mut bytes = Vec::new();
        for i in 0..20 {
            bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }

        let area = Rect::new(0, 0, 10, 5);
        let live = snapshot_from_bytes(10, 5, &bytes);
        let mut live_buf = Buffer::empty(area);
        paint_grid_snapshot(&live, area, &mut live_buf, false, None);
        let live_top: String = (0..5).map(|c| live_buf[(c, 0)].symbol()).collect();

        let scrolled = snapshot_from_bytes_scrolled(10, 5, &bytes, 5, false);
        let mut scrolled_buf = Buffer::empty(area);
        paint_grid_snapshot(&scrolled, area, &mut scrolled_buf, false, None);
        let scrolled_top: String = (0..5).map(|c| scrolled_buf[(c, 0)].symbol()).collect();
        assert_ne!(scrolled_top, live_top, "scrolling up must show older content");

        let restored = snapshot_from_bytes_scrolled(10, 5, &bytes, 5, true);
        let mut restored_buf = Buffer::empty(area);
        paint_grid_snapshot(&restored, area, &mut restored_buf, false, None);
        let restored_top: String = (0..5).map(|c| restored_buf[(c, 0)].symbol()).collect();
        assert_eq!(restored_top, live_top, "scrolling back to bottom must restore the live view");
    }

    // ── pane_title ────────────────────────────────────────────────────────────

    #[test]
    fn pane_title_plain_when_not_focused_and_not_scrolled() {
        assert_eq!(pane_title(false, 0, true), " agent ");
    }

    #[test]
    fn pane_title_shows_focused() {
        assert_eq!(pane_title(true, 0, true), " agent [FOCUSED — Ctrl-h to leave] ");
    }

    #[test]
    fn pane_title_shows_scrolled_offset_when_not_focused() {
        assert_eq!(pane_title(false, 42, true), " agent [scrolled ↑42 — G to follow] ");
    }

    #[test]
    fn pane_title_shows_scrolled_offset_while_focused() {
        // Focused and scrolled are simultaneous now that the mouse wheel
        // scrolls a focused pane (SCROLLBACK.md) — both must show, with a
        // "scroll to follow" hint rather than "G to follow" since `G`
        // forwards to the agent while focused, not back to the tail.
        assert_eq!(pane_title(true, 42, true), " agent [FOCUSED, scrolled ↑42 — scroll to follow] ");
    }

    #[test]
    fn pane_title_shows_exited_when_focused_on_a_dead_session() {
        // The exact reported bug: typing `exit` while jumped in left the
        // pane looking identical to a live, momentarily-quiet one.
        assert_eq!(pane_title(true, 0, false), " agent [exited — Ctrl-h to leave] ");
    }

    #[test]
    fn pane_title_shows_exited_when_not_focused_regardless_of_scroll() {
        assert_eq!(pane_title(false, 0, false), " agent [exited] ");
        assert_eq!(pane_title(false, 10, false), " agent [exited] ");
    }

    // ── render_term_pane's `alive` derivation (session_alive, not status) ────

    fn flat_node_with(status: AgentStatus, session_alive: bool) -> FlatNode {
        FlatNode {
            id: overseer_core::agent::AgentId::new(),
            name: "agent".to_string(),
            status,
            role: overseer_core::agent::AgentRole::Root,
            repo: "repo".to_string(),
            branch: "main".to_string(),
            context_pct: None,
            model_name: None,
            attention: None,
            session_alive,
            has_children: false,
            prefix: String::new(),
            status_since: std::time::Instant::now(),
            adapter: "claude".to_string(),
        }
    }

    fn rendered_title(node: &FlatNode) -> String {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        terminal
            .draw(|frame| {
                render_term_pane(frame, frame.area(), None, Some(node), false, None);
            })
            .unwrap();
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn render_term_pane_is_not_exited_for_a_done_agent_whose_session_is_still_alive() {
        // The real bug: an agent that self-reports `done` while the user
        // keeps prompting it must not look "[exited]" -- that title must
        // come from `session_alive`, not from `status` being Done/Error.
        let node = flat_node_with(AgentStatus::Done, true);
        let content = rendered_title(&node);
        assert!(!content.contains("exited"), "done-but-alive session must not render as exited: {content}");
    }

    #[test]
    fn render_term_pane_shows_exited_once_the_session_has_actually_exited() {
        let node = flat_node_with(AgentStatus::Done, false);
        let content = rendered_title(&node);
        assert!(content.contains("exited"), "a genuinely exited session must still show [exited]: {content}");
    }
}
