mod term_pane;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use unicode_segmentation::UnicodeSegmentation;

use crate::agent::{AgentRole, AgentStatus, AgentTree, FlatNode};
use crate::app::{InputState, PendingAction};

pub use term_pane::{render_term_pane, PaneSource};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One glyph representing `status`, used both as the tree-row badge and (for
/// non-`Running` statuses) in place of the animated spinner.
pub(crate) fn status_badge(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Spawning => "…",
        AgentStatus::Running => "●",
        AgentStatus::Blocked => "!",
        AgentStatus::Idle => "◌",
        AgentStatus::Done => "✓",
        AgentStatus::Error => "✗",
    }
}

/// The color/weight `status` renders with, wherever it appears in the UI.
pub(crate) fn status_style(status: &AgentStatus) -> Style {
    match status {
        AgentStatus::Spawning => Style::default().fg(Color::Cyan),
        AgentStatus::Running => Style::default().fg(Color::Green),
        AgentStatus::Blocked => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentStatus::Idle => Style::default().fg(Color::DarkGray),
        AgentStatus::Done => Style::default().fg(Color::Blue),
        AgentStatus::Error => Style::default().fg(Color::Red),
    }
}
/// Width of the tree column as a percentage of the full window — the pane
/// takes the rest.
const TREE_COLUMN_PCT: u16 = 25;

/// Renders the whole window: the tree|pane split plus a full-width status
/// bar. `frame.area()` is now the entire terminal — there is no longer a
/// separate tmux pane on the right; `render_term_pane` draws the selected
/// agent's live grid directly into this same ratatui frame. Returns the
/// pane's inner rect so the caller can size every agent's PTY to it.
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    tree: &AgentTree,
    tick: u64,
    prompt: Option<&str>,
    input: Option<&InputState>,
    pane_source: &PaneSource,
    pane_focused: bool,
) -> Rect {
    let area = frame.area();
    let (running, blocked, total) = tree.agent_counts();
    let status_line = build_status_line(running, blocked, total, prompt);
    let status_text: String = status_line.spans.iter().map(|s| s.content.as_ref()).collect();
    let status_height = status_bar_height(&status_text, area.width);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
        .split(area);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(TREE_COLUMN_PCT),
            Constraint::Percentage(100 - TREE_COLUMN_PCT),
        ])
        .split(outer[0]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(7)])
        .split(columns[0]);

    let flat = tree.flatten();
    let selected = flat.get(tree.cursor).cloned();

    render_agent_tree(frame, tree.cursor, tick, left[0], &flat);
    render_agent_detail(frame, left[1], &selected);
    let pane_rect =
        render_term_pane(frame, columns[1], pane_source, selected.as_ref().map(|n| &n.id), pane_focused);
    render_status_bar(frame, status_line, outer[1]);

    // Drawn last, on top of everything — a bordered, centered modal instead of
    // the old cramped status-bar text, since that was genuinely easy to miss
    // (no visual weight beyond wrapped plain text at the very bottom of an
    // already-narrow pane).
    if let Some(input) = input {
        render_spawn_modal(frame, area, input);
    }

    pane_rect
}

/// How many terminal rows `text` needs when word-wrapped to `area_width`
/// columns. A plain `ceil(char_count / width)` division under-counts whenever
/// a word straddles the boundary — `render_status_bar` renders with
/// `Wrap { trim: false }`, a *word* wrapper, not a column-count one, so this
/// simulates the same greedy word-packing to avoid silently clipping the last
/// line of a wrapped confirm/error prompt (a drop confirmation's trailing
/// "(y/n)", for example).
fn status_bar_height(text: &str, area_width: u16) -> u16 {
    let width = (area_width.max(1) as usize).max(1);
    let mut lines: u16 = 1;
    let mut col = 0usize;
    for word in text.split_whitespace() {
        let word_len = word.chars().count().min(width);
        let needed = if col == 0 { word_len } else { col + 1 + word_len };
        if needed > width {
            lines += 1;
            col = word_len;
        } else {
            col = needed;
        }
    }
    lines
}

fn render_agent_tree(frame: &mut Frame, cursor: usize, tick: u64, area: Rect, flat: &[FlatNode]) {
    // `List`'s border consumes 2 columns; the row text itself gets the rest.
    let inner_width = area.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = flat
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let selected = i == cursor;
            let line = tree_row(node, selected, tick, inner_width);
            if selected {
                ListItem::new(line).style(Style::default().bg(Color::DarkGray))
            } else {
                ListItem::new(line)
            }
        })
        .collect();

    let block = Block::default()
        .title(" AGENTS ")
        .borders(Borders::ALL)
        .border_style(focused_border(true));

    frame.render_widget(List::new(items).block(block), area);
}

fn tree_row(node: &FlatNode, selected: bool, tick: u64, width: usize) -> Line<'static> {
    let name_style = if selected {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let badge = if node.status == AgentStatus::Running {
        let frame = (tick as usize / 2) % SPINNER_FRAMES.len();
        SPINNER_FRAMES[frame].to_string()
    } else {
        status_badge(&node.status).to_string()
    };

    // prefix + badge (always 1 column wide) + the space separating badge from name.
    let left_fixed = node.prefix.chars().count() + 1 + 1;
    let row_width = width.saturating_sub(left_fixed);
    // Age only for blocked/idle (ATTENTION.md) — a running agent doesn't
    // need a clock, it's actively doing something.
    let age = matches!(node.status, AgentStatus::Blocked | AgentStatus::Idle)
        .then(|| format_age(node.status_since.elapsed()));
    let layout =
        format_tree_row(&node.name, node.status.label(), age.as_deref(), node.context_pct, row_width);
    let gap = row_width
        .saturating_sub(layout.name.chars().count() + layout.status_word.chars().count() + layout.pct_suffix.chars().count())
        .max(1);

    let row_status_style = if node.status == AgentStatus::Blocked {
        status_style(&node.status)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Line::from(vec![
        Span::styled(node.prefix.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(badge, status_style(&node.status)),
        Span::raw(" "),
        Span::styled(layout.name, name_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(layout.status_word, row_status_style),
        Span::styled(layout.pct_suffix, Style::default().fg(Color::DarkGray)),
    ])
}

/// Pure. Lays out one tree row's text: the name (truncated with `…` if it would
/// otherwise overflow `width`) and a right-aligned `<status> <pct>%` block —
/// `pct_suffix` is empty while the context % is unknown. Kept free of styling so
/// `tree_row` can color the status word specially when blocked; this function
/// only owns the width arithmetic.
struct TreeRowLayout {
    name: String,
    status_word: String,
    pct_suffix: String,
}

fn format_tree_row(
    name: &str,
    status_label: &str,
    age: Option<&str>,
    pct: Option<u8>,
    width: usize,
) -> TreeRowLayout {
    let status_word = match age {
        Some(age) => format!("{status_label} {age}"),
        None => status_label.to_string(),
    };
    let pct_suffix = pct.map(|p| format!(" {p}%")).unwrap_or_default();
    let right_len = status_word.chars().count() + pct_suffix.chars().count();
    // Reserve at least 1 column of gap between name and the right block; if the
    // row is too narrow even for the right block alone, let the name collapse to
    // nothing rather than underflow — render() just clips an over-length line,
    // same as any other ratatui `Line`.
    let name_budget = width.saturating_sub(right_len + 1);
    let name = truncate_with_ellipsis(name, name_budget);
    TreeRowLayout { name, status_word, pct_suffix }
}

/// Pure. Formats an elapsed duration as a single-unit age (ATTENTION.md):
/// `45s`, `12m`, `3h` — never a compound like "1h 2m", and never `0`-padded.
fn format_age(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// Truncates `s` to at most `max` grapheme clusters, replacing the last one
/// with `…` when it doesn't fit — never panics, even for `max` of 0 or 1.
/// Cuts on grapheme cluster boundaries (not `char`/Unicode scalar value), so a
/// base letter + combining accent or a ZWJ emoji sequence at the truncation
/// point is kept whole instead of split into a dangling combining mark or half
/// a sequence.
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    let graphemes: Vec<&str> = s.graphemes(true).collect();
    if graphemes.len() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else if max == 1 {
        "…".to_string()
    } else {
        let keep: String = graphemes[..max - 1].concat();
        format!("{keep}…")
    }
}

fn render_agent_detail(frame: &mut Frame, area: Rect, selected: &Option<FlatNode>) {
    let content = match selected {
        Some(node) => vec![
            Line::from(vec![
                // A root's name is the repo it was spawned in, not a task
                // description (Part A: roots are a bare shell, no task text) —
                // a child's name is the task it was actually spawned with.
                Span::styled(
                    if node.role == AgentRole::Root { "name:   " } else { "task:   " },
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(node.name.clone()),
            ]),
            Line::from(vec![
                Span::styled("status: ", Style::default().fg(Color::DarkGray)),
                Span::styled(node.status.label(), status_style(&node.status)),
            ]),
            Line::from(vec![
                Span::styled("since:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format_age(node.status_since.elapsed()), Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled("repo:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(node.repo.clone(), Style::default().fg(Color::Cyan)),
            ]),
            Line::from(vec![
                Span::styled("branch: ", Style::default().fg(Color::DarkGray)),
                Span::styled(node.branch.clone(), Style::default().fg(Color::Yellow)),
            ]),
            Line::from(vec![
                Span::styled("ctx:    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    node.context_pct
                        .map(|p| format!("{p}%  {}", context_bar(p)))
                        .unwrap_or_else(|| "—".to_string()),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("id:     ", Style::default().fg(Color::DarkGray)),
                Span::styled(node.id.short(), Style::default().fg(Color::DarkGray)),
            ]),
        ],
        None => vec![Line::from(Span::styled(
            "  no agent selected",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let block = Block::default()
        .title(" DETAIL ")
        .borders(Borders::ALL)
        .border_style(focused_border(false));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

/// Pure — builds the status line's content so `render()` can measure its
/// wrapped height before laying out the rest of the frame.
fn build_status_line(running: usize, blocked: usize, total: usize, prompt: Option<&str>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            " OVERSEER ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];

    if let Some(prompt) = prompt {
        spans.push(Span::styled(prompt.to_string(), Style::default().fg(Color::White)));
    } else {
        // Quitting never kills agents (they outlive the TUI, tmux-detach
        // style) — `d`/`D` kills one session (still confirms); `Q` is the
        // one command that also confirms, since it reaches every agent plus
        // the daemon itself.
        let hints: Vec<Span> = vec![
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
            Span::styled("<space>", Style::default().fg(Color::Yellow)),
            Span::styled(" fold  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Ctrl-l/↵", Style::default().fg(Color::Yellow)),
            Span::styled(" jump in  ", Style::default().fg(Color::DarkGray)),
            Span::styled("n/s", Style::default().fg(Color::Yellow)),
            Span::styled(" spawn  ", Style::default().fg(Color::DarkGray)),
            Span::styled("d/D", Style::default().fg(Color::Yellow)),
            Span::styled(" drop  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Q", Style::default().fg(Color::Yellow)),
            Span::styled(" shutdown", Style::default().fg(Color::DarkGray)),
        ];
        let counts_text = if blocked > 0 {
            format!("{running}/{total} running · {blocked} blocked")
        } else {
            format!("{running}/{total} running")
        };
        let counts_style = if blocked > 0 {
            status_style(&AgentStatus::Blocked)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(counts_text, counts_style));
        spans.push(Span::raw("   "));
        spans.extend(hints);
    }

    Line::from(spans)
}

fn render_status_bar(frame: &mut Frame, line: Line<'static>, area: Rect) {
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), area);
}

/// Centers a `width`x`height` box within `area`, clamped to fit (shrinking
/// rather than overflowing in a narrow pane). `None` if there's truly no room.
fn centered_rect(area: Rect, width: u16, height: u16) -> Option<Rect> {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    if w < 4 || h < 4 {
        return None;
    }
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Some(Rect::new(x, y, w, h))
}

fn render_spawn_modal(frame: &mut Frame, area: Rect, input: &InputState) {
    let (title, label) = match &input.action {
        PendingAction::SpawnRoot => (" spawn root ", "repo path:"),
        PendingAction::SpawnChild { .. } => (" spawn child ", "task:"),
    };

    let Some(popup) = centered_rect(area, 56, 6) else { return };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, Style::default().fg(Color::DarkGray)))),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{}█", tail(&input.buffer, rows[1].width)), Style::default().fg(Color::White)),
        ])),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("↵ ", Style::default().fg(Color::Yellow)),
            Span::styled("spawn   ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc ", Style::default().fg(Color::Yellow)),
            Span::styled("cancel", Style::default().fg(Color::DarkGray)),
        ])),
        rows[2],
    );
}

/// The last `width` characters of `text` (accounting for the `"> "` prefix and
/// the `"█"` cursor glyph this is always rendered with) — a single-line input
/// scrolls to keep the cursor visible instead of wrapping a long path/task
/// across rows the modal doesn't have room for.
fn tail(text: &str, width: u16) -> String {
    let budget = (width as usize).saturating_sub(3); // "> " + cursor
    let len = text.chars().count();
    if len <= budget {
        text.to_string()
    } else {
        text.chars().skip(len - budget).collect()
    }
}

fn context_bar(pct: u8) -> String {
    let filled = (pct as usize * 8 / 100).min(8);
    format!("{}{}", "█".repeat(filled), "░".repeat(8 - filled))
}

fn focused_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── status_badge / status_style ──────────────────────────────────────────

    #[test]
    fn idle_has_distinct_badge_and_style() {
        assert_eq!(status_badge(&AgentStatus::Idle), "◌");
        assert_eq!(status_style(&AgentStatus::Idle), Style::default().fg(Color::DarkGray));
        assert_ne!(status_badge(&AgentStatus::Idle), status_badge(&AgentStatus::Blocked));
    }

    #[test]
    fn blocked_is_red_and_bold() {
        assert_eq!(status_badge(&AgentStatus::Blocked), "!");
        assert_eq!(
            status_style(&AgentStatus::Blocked),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
    }

    // ── format_tree_row / truncate_with_ellipsis ─────────────────────────────

    #[test]
    fn format_tree_row_fits_without_truncation_in_a_roomy_width() {
        let layout = format_tree_row("auth-module", "idle", None, Some(62), 40);
        assert_eq!(layout.name, "auth-module");
        assert_eq!(layout.status_word, "idle");
        assert_eq!(layout.pct_suffix, " 62%");
    }

    #[test]
    fn format_tree_row_omits_pct_when_unknown() {
        let layout = format_tree_row("write-tests", "running", None, None, 40);
        assert_eq!(layout.pct_suffix, "");
        assert_eq!(layout.status_word, "running");
    }

    #[test]
    fn format_tree_row_truncates_long_name_with_ellipsis() {
        let layout = format_tree_row("a-very-long-task-description-here", "blocked", None, Some(91), 20);
        assert!(layout.name.ends_with('…'));
        assert!(layout.name.chars().count() < "a-very-long-task-description-here".chars().count());
    }

    #[test]
    fn format_tree_row_narrow_width_does_not_panic() {
        for width in 0..6 {
            let layout = format_tree_row("some-task", "blocked", None, Some(5), width);
            // Right block (status + pct) always survives intact even if the name collapses.
            assert_eq!(layout.status_word, "blocked");
            assert_eq!(layout.pct_suffix, " 5%");
        }
    }

    #[test]
    fn format_tree_row_appends_age_to_the_status_word() {
        let layout = format_tree_row("update-docs", "blocked", Some("2m"), Some(5), 40);
        assert_eq!(layout.status_word, "blocked 2m");
    }

    #[test]
    fn format_tree_row_age_survives_at_a_narrow_width_alongside_pct() {
        for width in 0..6 {
            let layout = format_tree_row("some-task", "idle", Some("12m"), Some(5), width);
            assert_eq!(layout.status_word, "idle 12m");
            assert_eq!(layout.pct_suffix, " 5%");
        }
    }

    // ── format_age ────────────────────────────────────────────────────────────

    #[test]
    fn format_age_single_unit_seconds_minutes_hours() {
        assert_eq!(format_age(std::time::Duration::from_secs(45)), "45s");
        assert_eq!(format_age(std::time::Duration::from_secs(59)), "59s");
        assert_eq!(format_age(std::time::Duration::from_secs(60)), "1m");
        assert_eq!(format_age(std::time::Duration::from_secs(12 * 60)), "12m");
        assert_eq!(format_age(std::time::Duration::from_secs(3599)), "59m");
        assert_eq!(format_age(std::time::Duration::from_secs(3600)), "1h");
        assert_eq!(format_age(std::time::Duration::from_secs(3 * 3600 + 59 * 60)), "3h");
    }

    #[test]
    fn truncate_with_ellipsis_fits_unchanged() {
        assert_eq!(truncate_with_ellipsis("short", 10), "short");
    }

    #[test]
    fn truncate_with_ellipsis_exact_fit_unchanged() {
        assert_eq!(truncate_with_ellipsis("exact", 5), "exact");
    }

    #[test]
    fn truncate_with_ellipsis_overflow_gets_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello world", 6), "hello…");
    }

    #[test]
    fn truncate_with_ellipsis_keeps_a_multi_codepoint_grapheme_whole() {
        // "é" as base "e" + combining acute accent (U+0301) is 2 chars but one
        // grapheme cluster — truncating by char would split it and leave a
        // dangling combining mark; by grapheme it must stay whole or be dropped.
        let combining_e_acute = "e\u{0301}";
        let name = format!("caf{combining_e_acute}"); // "café", 4 graphemes, 5 chars
        let truncated = truncate_with_ellipsis(&name, 4); // fits exactly, no truncation
        assert_eq!(truncated, name);

        let truncated = truncate_with_ellipsis(&name, 3); // must drop the whole café-accent grapheme, not split it
        assert_eq!(truncated, "ca…");
    }

    #[test]
    fn truncate_with_ellipsis_keeps_a_zwj_emoji_sequence_whole() {
        // Family emoji: man + ZWJ + woman + ZWJ + girl — one grapheme cluster
        // made of 5 Unicode scalar values (3 emoji + 2 ZWJ joiners). A
        // char-based truncation to budget 2 would keep only the first 2
        // scalar values (man + ZWJ), a dangling broken half-sequence.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let name = format!("{family}extra"); // 6 graphemes: family, e, x, t, r, a
        let truncated = truncate_with_ellipsis(&name, 2);
        assert_eq!(truncated, format!("{family}…"), "the ZWJ sequence must survive whole, not be split");
    }

    #[test]
    fn truncate_with_ellipsis_max_zero_is_empty() {
        assert_eq!(truncate_with_ellipsis("hello", 0), "");
    }

    #[test]
    fn truncate_with_ellipsis_max_one_is_just_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello", 1), "…");
    }

    #[test]
    fn centered_rect_centers_within_a_roomy_area() {
        let area = Rect::new(0, 0, 100, 50);
        let popup = centered_rect(area, 44, 6).unwrap();
        assert_eq!(popup.width, 44);
        assert_eq!(popup.height, 6);
        assert_eq!(popup.x, (100 - 44) / 2);
        assert_eq!(popup.y, (50 - 6) / 2);
    }

    #[test]
    fn centered_rect_shrinks_to_fit_a_narrow_area() {
        let area = Rect::new(0, 0, 20, 10);
        let popup = centered_rect(area, 44, 6).unwrap();
        assert!(popup.width <= 18); // area.width - 2
        assert!(popup.x + popup.width <= area.width);
    }

    #[test]
    fn centered_rect_none_when_area_too_small() {
        assert!(centered_rect(Rect::new(0, 0, 3, 3), 44, 6).is_none());
    }

    #[test]
    fn tail_returns_text_unchanged_when_it_fits() {
        assert_eq!(tail("short", 20), "short");
    }

    #[test]
    fn tail_keeps_the_end_when_text_overflows() {
        let long = "/Users/nikita.ivanov/projects/overseer";
        let truncated = tail(long, 20);
        assert!(long.ends_with(&truncated));
        assert!(truncated.chars().count() <= 17); // 20 - "> " - cursor
    }

    #[test]
    fn status_bar_height_fits_on_one_line() {
        assert_eq!(status_bar_height("short message", 80), 1);
        assert_eq!(status_bar_height(&"x".repeat(80), 80), 1);
    }

    #[test]
    fn status_bar_height_wraps_more_than_naive_char_division_predicts() {
        // 3 words of 6 chars, width 10: naive ceil((6+1+6+1+6)/10) = ceil(20/10)
        // = 2, but greedy word-wrap can't fit "bbbbbb" after "aaaaaa" (needs 7 of
        // the remaining 4 columns) nor "cccccc" after that — it actually needs
        // 3 lines. This is exactly the case that used to clip a wrapped
        // confirm/error message's last line under the old char-count formula.
        let text = "aaaaaa bbbbbb cccccc";
        assert_eq!(status_bar_height(text, 10), 3);
    }

    #[test]
    fn status_bar_height_a_single_overlong_word_still_counts_as_one_line() {
        let text = "x".repeat(200);
        assert_eq!(status_bar_height(&text, 80), 1);
    }

    #[test]
    fn status_bar_height_never_zero_even_with_zero_width_area() {
        assert_eq!(status_bar_height("", 0), 1);
        assert!(status_bar_height(&"x ".repeat(50), 0) >= 1);
    }

    #[test]
    fn build_status_line_with_prompt_uses_prompt_text_not_hints() {
        let line = build_status_line(1, 0, 2, Some("drop 'agent'? (y/n)"));
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("drop 'agent'? (y/n)"));
        assert!(!text.contains("jump in"));
    }

    #[test]
    fn build_status_line_without_prompt_shows_hints_and_counts() {
        let line = build_status_line(1, 0, 2, None);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1/2 running"));
        assert!(text.contains("jump in"));
    }

    #[test]
    fn build_status_line_with_blocked_shows_blocked_count() {
        let line = build_status_line(1, 2, 4, None);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1/4 running"));
        assert!(text.contains("2 blocked"));
    }

    #[test]
    fn build_status_line_without_blocked_omits_blocked_text() {
        let line = build_status_line(1, 0, 2, None);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("blocked"));
    }
}
