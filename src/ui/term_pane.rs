use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::agent::{AgentStatus, FlatNode};
use crate::ipc::protocol::{ColorDto, GridSnapshot};

/// Renders the selected agent's live terminal grid into `area` — the pane
/// half of the tree|pane split. `grid` is the selected agent's current
/// rendered content, `None` if it has none yet (or isn't running); `--mock`
/// and daemon-attached modes both feed this from `App::pane_grid` — `ui/`
/// never sees anything backend-specific. `focused` draws the cursor and a
/// distinct border; read-only preview otherwise. Returns the inner
/// (border-excluded) rect actually painted, so callers can size the PTY to it.
pub fn render_term_pane(
    frame: &mut Frame,
    area: Rect,
    grid: Option<&GridSnapshot>,
    selected: Option<&FlatNode>,
    focused: bool,
) -> Rect {
    let offset = grid.map(|g| g.display_offset).unwrap_or(0);
    // A session that exits naturally (not via `d`/`D`) keeps its last
    // rendered content around for review — by design (AGENTS.md Cleanup),
    // not a bug — but with no marker at all, a pane the user is still
    // looking at (or focused into) just silently stops responding with
    // zero explanation, which a real user reported as the pane having
    // "frozen". `alive` mirrors `App::is_alive`'s own rule (not Done, not
    // Error) so the title can say plainly why nothing more is happening.
    let alive = selected.is_some_and(|n| !matches!(n.status, AgentStatus::Done | AgentStatus::Error));
    let block = Block::default().borders(Borders::ALL).title(pane_title(focused, offset, alive));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(node) = selected else {
        frame.render_widget(placeholder("no agent selected"), inner);
        return inner;
    };

    // A freshly spawned agent's harness (claude/opencode/pi, or a bare
    // shell) hasn't painted anything yet — the PTY exists and streams a
    // real (all-blank) grid well before the process's first visible output,
    // so with no marker the pane just looks stuck for however long the
    // harness takes to boot (measured ~1.6s+, worse with MCP-heavy configs).
    // Only for `Spawning`: a running/idle agent that happens to have a
    // blank screen is showing real content (or the real absence of it) and
    // must never be lied about — and the instant any visible cell shows up,
    // this branch stops applying on its own since `grid_is_blank` goes
    // false.
    if node.status == AgentStatus::Spawning && grid.is_none_or(grid_is_blank) {
        frame.render_widget(
            placeholder(format!("launching {}… (waiting for first output)", node.adapter)),
            inner,
        );
        return inner;
    }

    match grid {
        Some(grid) => paint_grid_snapshot(grid, inner, frame.buffer_mut(), focused),
        None => frame.render_widget(placeholder("agent not running"), inner),
    }
    inner
}

fn placeholder(text: impl Into<String>) -> Paragraph<'static> {
    Paragraph::new(text.into()).style(Style::default().fg(Color::DarkGray))
}

/// True when every cell is either absent or whitespace — i.e. the harness
/// hasn't painted anything a user would perceive as content yet. One
/// O(cols×lines) scan; only ever called for the single selected pane, once
/// per frame (see `render_term_pane`'s `Spawning` branch).
fn grid_is_blank(grid: &GridSnapshot) -> bool {
    grid.cells.iter().all(|cell| match cell {
        None => true,
        Some(c) => c.ch.is_whitespace(),
    })
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

/// Paints a `GridSnapshot` into `buf`, cell by cell — the only painter in
/// `ui/`, for both `--mock` and daemon-attached modes (`GridSnapshot` is the
/// only render currency; see `session::pty::grid_snapshot_from_term` for how
/// it's built).
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
    use crate::agent::{AgentId, AgentRole};
    use crate::ipc::protocol::CellDto;
    use crate::session::snapshot_from_bytes;

    fn flat_node(status: AgentStatus, adapter: &str) -> FlatNode {
        FlatNode {
            id: AgentId::new(),
            name: "agent".to_string(),
            status,
            role: AgentRole::Child,
            repo: "repo".to_string(),
            branch: "main".to_string(),
            context_pct: None,
            has_children: false,
            prefix: String::new(),
            status_since: std::time::Instant::now(),
            adapter: adapter.to_string(),
        }
    }

    #[test]
    fn plain_text_renders_into_top_left() {
        let grid = snapshot_from_bytes(10, 3, b"hi");
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false);
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
        paint_grid_snapshot(&grid, area, &mut buf, false);
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
        paint_grid_snapshot(&grid, area, &mut buf, false);
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
        paint_grid_snapshot(&grid, area, &mut buf_no_cursor, false);
        assert!(!buf_no_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));

        let mut buf_cursor = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf_cursor, true);
        assert!(buf_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn newline_moves_to_next_row() {
        let grid = snapshot_from_bytes(10, 3, b"a\r\nb");
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        paint_grid_snapshot(&grid, area, &mut buf, false);
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
        paint_grid_snapshot(&grid, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(4, 0)].symbol(), "o");
    }

    // ── scrollback (SCROLLBACK.md) ───────────────────────────────────────────

    #[test]
    fn scroll_display_shows_older_lines_then_bottom_restores_live_content() {
        use crate::session::snapshot_from_bytes_scrolled;

        // 5 lines of visible height, but print far more so there's real
        // scrollback history to move into.
        let mut bytes = Vec::new();
        for i in 0..20 {
            bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }

        let area = Rect::new(0, 0, 10, 5);
        let live = snapshot_from_bytes(10, 5, &bytes);
        let mut live_buf = Buffer::empty(area);
        paint_grid_snapshot(&live, area, &mut live_buf, false);
        let live_top: String = (0..5).map(|c| live_buf[(c, 0)].symbol()).collect();

        let scrolled = snapshot_from_bytes_scrolled(10, 5, &bytes, 5, false);
        let mut scrolled_buf = Buffer::empty(area);
        paint_grid_snapshot(&scrolled, area, &mut scrolled_buf, false);
        let scrolled_top: String = (0..5).map(|c| scrolled_buf[(c, 0)].symbol()).collect();
        assert_ne!(scrolled_top, live_top, "scrolling up must show older content");

        let restored = snapshot_from_bytes_scrolled(10, 5, &bytes, 5, true);
        let mut restored_buf = Buffer::empty(area);
        paint_grid_snapshot(&restored, area, &mut restored_buf, false);
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

    // ── grid_is_blank ─────────────────────────────────────────────────────────

    fn blank_cell_grid(cols: u16, lines: u16) -> GridSnapshot {
        GridSnapshot {
            cols,
            lines,
            cells: vec![None; cols as usize * lines as usize],
            cursor: None,
            app_cursor_mode: false,
            bracketed_paste_mode: false,
            display_offset: 0,
        }
    }

    fn space_cell() -> CellDto {
        CellDto {
            ch: ' ',
            fg: ColorDto::Reset,
            bg: ColorDto::Reset,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }

    #[test]
    fn grid_is_blank_true_for_empty_grid() {
        let grid = blank_cell_grid(5, 2);
        assert!(grid_is_blank(&grid));
    }

    #[test]
    fn grid_is_blank_false_when_a_cell_has_visible_content() {
        let mut grid = blank_cell_grid(5, 2);
        grid.cells[3] = Some(CellDto { ch: 'x', ..space_cell() });
        assert!(!grid_is_blank(&grid));
    }

    #[test]
    fn grid_is_blank_true_when_every_cell_is_whitespace() {
        let mut grid = blank_cell_grid(3, 1);
        for cell in grid.cells.iter_mut() {
            *cell = Some(space_cell());
        }
        assert!(grid_is_blank(&grid));
    }

    // ── render_term_pane: spawning placeholder ──────────────────────────────

    fn rendered_content(grid: Option<&GridSnapshot>, selected: Option<&FlatNode>) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_term_pane(frame, frame.area(), grid, selected, false);
            })
            .unwrap();
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn spawning_with_blank_grid_shows_launching_placeholder() {
        let node = flat_node(AgentStatus::Spawning, "claude");
        let grid = blank_cell_grid(20, 3);
        let content = rendered_content(Some(&grid), Some(&node));
        assert!(content.contains("launching claude"), "expected placeholder, got: {content}");
        assert!(content.contains("waiting for first output"));
    }

    #[test]
    fn spawning_with_no_grid_shows_launching_placeholder() {
        let node = flat_node(AgentStatus::Spawning, "opencode");
        let content = rendered_content(None, Some(&node));
        assert!(content.contains("launching opencode"), "expected placeholder, got: {content}");
    }

    #[test]
    fn spawning_with_visible_content_paints_the_real_grid_not_the_placeholder() {
        let node = flat_node(AgentStatus::Spawning, "claude");
        let grid = snapshot_from_bytes(20, 3, b"hi");
        let content = rendered_content(Some(&grid), Some(&node));
        assert!(!content.contains("launching"), "must not show placeholder once content exists: {content}");
        assert!(content.contains('h') && content.contains('i'));
    }

    #[test]
    fn running_with_blank_grid_paints_the_real_blank_grid_not_a_placeholder() {
        // Only `Spawning` gets the launching placeholder — a running (or
        // idle) agent with a genuinely blank screen must never be lied
        // about by pretending it's still launching.
        let node = flat_node(AgentStatus::Running, "claude");
        let grid = blank_cell_grid(20, 3);
        let content = rendered_content(Some(&grid), Some(&node));
        assert!(!content.contains("launching"), "must not show placeholder for a running agent: {content}");
    }
}
