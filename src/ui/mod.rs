mod term_pane;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use unicode_segmentation::UnicodeSegmentation;

use std::collections::HashSet;

use crate::agent::{AgentId, AgentNode, AgentRole, AgentStatus, AgentTree, FlatNode};
use crate::app::{InputState, PendingAction};
use crate::config::{Action, Keybindings, Theme};

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
/// Colors come from `theme` (PHASE5B.md); `Blocked`'s bold weight is fixed —
/// `[theme]` is "small and honest: the statuses + chrome, nothing else",
/// colors only.
pub(crate) fn status_style(status: &AgentStatus, theme: &Theme) -> Style {
    match status {
        AgentStatus::Spawning => Style::default().fg(theme.spawning),
        AgentStatus::Running => Style::default().fg(theme.running),
        AgentStatus::Blocked => Style::default().fg(theme.blocked).add_modifier(Modifier::BOLD),
        AgentStatus::Idle => Style::default().fg(theme.idle),
        AgentStatus::Done => Style::default().fg(theme.done),
        AgentStatus::Error => Style::default().fg(theme.error),
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
    theme: &Theme,
    keybindings: &Keybindings,
    show_help: bool,
) -> Rect {
    let area = frame.area();
    let (running, blocked, total) = tree.agent_counts();
    let status_line = build_status_line(running, blocked, total, prompt, theme);
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
    // The detail pane / live grid always track the *real* cursor — a live
    // search filter only affects what the tree list shows, nothing moves
    // for real until Enter (PHASE5B.md).
    let selected = flat.get(tree.cursor).cloned();

    // While `/` is open, render only rows that match the query (plus every
    // ancestor of a match, dimmed, for context) instead of the full tree.
    let search_query = match input {
        Some(InputState { action: PendingAction::Search, buffer }) => Some(buffer.as_str()),
        _ => None,
    };
    let (display_flat, display_cursor, matched) = match search_query {
        Some(query) => {
            let (visible, matched) = search_visibility(&tree.roots, query);
            let filtered: Vec<FlatNode> = flat.iter().filter(|n| visible.contains(&n.id)).cloned().collect();
            let real_selected_id = flat.get(tree.cursor).map(|n| n.id.clone());
            let highlight = real_selected_id
                .as_ref()
                .and_then(|id| filtered.iter().position(|n| &n.id == id))
                .or_else(|| filtered.iter().position(|n| matched.contains(&n.id)))
                .unwrap_or(0);
            (filtered, highlight, Some(matched))
        }
        None => (flat, tree.cursor, None),
    };

    render_agent_tree(frame, display_cursor, tick, left[0], &display_flat, matched.as_ref(), theme);
    render_agent_detail(frame, left[1], &selected, theme);
    let pane_rect =
        render_term_pane(frame, columns[1], pane_source, selected.as_ref().map(|n| &n.id), pane_focused);
    render_status_bar(frame, status_line, outer[1]);

    // Drawn last, on top of everything — a bordered, centered modal instead of
    // the old cramped status-bar text, since that was genuinely easy to miss
    // (no visual weight beyond wrapped plain text at the very bottom of an
    // already-narrow pane).
    if let Some(input) = input {
        render_input_modal(frame, area, input);
    }

    if show_help {
        render_help_popup(frame, area, keybindings);
    }

    pane_rect
}

/// Pure. Every agent whose name matches `query` (`matched`), plus every node
/// that's a match itself or has a matching descendant (`visible` — an
/// ancestor stays on screen for context, dimmed in the UI). Walks the real
/// tree (not the flattened list) since only it has parent/child structure;
/// deliberately ignores each node's `expanded` flag — a match hidden inside
/// a currently-folded branch still won't surface (this only filters what
/// `flatten()` already produced), but a match's own ancestors are always
/// included regardless of whether *they* happen to be folded.
fn search_visibility(roots: &[AgentNode], query: &str) -> (HashSet<AgentId>, HashSet<AgentId>) {
    fn walk(nodes: &[AgentNode], query: &str, visible: &mut HashSet<AgentId>, matched: &mut HashSet<AgentId>) -> bool {
        let mut any = false;
        for node in nodes {
            let self_match = fuzzy_match(query, &node.name).is_some();
            let child_visible = walk(&node.children, query, visible, matched);
            if self_match || child_visible {
                visible.insert(node.id.clone());
                any = true;
            }
            if self_match {
                matched.insert(node.id.clone());
            }
        }
        any
    }
    let mut visible = HashSet::new();
    let mut matched = HashSet::new();
    walk(roots, query, &mut visible, &mut matched);
    (visible, matched)
}

/// Pure. Case-insensitive subsequence match with a contiguity bonus — a
/// contiguous run scores higher than the same characters scattered, so
/// `"au"` outranks `"ae"` against `"auth-module"` even though both match.
/// `None` if `query`'s characters don't all appear, in order, in `name`. An
/// empty `query` matches everything (score `0`) — "not currently filtering"
/// and "filtering on nothing" are the same thing from the caller's side.
pub fn fuzzy_match(query: &str, name: &str) -> Option<u32> {
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let name: Vec<char> = name.to_lowercase().chars().collect();
    if query.is_empty() {
        return Some(0);
    }
    let mut score: u32 = 0;
    let mut qi = 0;
    let mut consecutive: u32 = 0;
    for &nc in &name {
        if qi < query.len() && nc == query[qi] {
            consecutive += 1;
            score += consecutive;
            qi += 1;
        } else {
            consecutive = 0;
        }
    }
    if qi == query.len() {
        Some(score)
    } else {
        None
    }
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

fn render_agent_tree(
    frame: &mut Frame,
    cursor: usize,
    tick: u64,
    area: Rect,
    flat: &[FlatNode],
    matched: Option<&HashSet<AgentId>>,
    theme: &Theme,
) {
    // `List`'s border consumes 2 columns; the row text itself gets the rest.
    let inner_width = area.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = flat
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let selected = i == cursor;
            // While searching, a row kept only for ancestor context (not
            // itself a match) renders dimmed — `matched` is `None` outside
            // search, so nothing dims then.
            let dimmed = matched.is_some_and(|m| !m.contains(&node.id));
            let line = tree_row(node, selected, dimmed, tick, theme, inner_width);
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
        .border_style(focused_border(true, theme));

    frame.render_widget(List::new(items).block(block), area);
}

fn tree_row(node: &FlatNode, selected: bool, dimmed: bool, tick: u64, theme: &Theme, width: usize) -> Line<'static> {
    let name_style = if dimmed {
        Style::default().fg(theme.idle)
    } else if selected {
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

    // A dimmed (ancestor-context-only) row during search stays legible but
    // visually recedes behind actual matches — everything in `theme.idle`,
    // badge included, regardless of the node's real status color.
    let badge_style = if dimmed { Style::default().fg(theme.idle) } else { status_style(&node.status, theme) };
    let row_status_style = if dimmed {
        Style::default().fg(theme.idle)
    } else if node.status == AgentStatus::Blocked {
        status_style(&node.status, theme)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Line::from(vec![
        Span::styled(node.prefix.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(badge, badge_style),
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

fn render_agent_detail(frame: &mut Frame, area: Rect, selected: &Option<FlatNode>, theme: &Theme) {
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
                Span::styled(node.status.label(), status_style(&node.status, theme)),
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
        .border_style(focused_border(false, theme));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

/// Pure — builds the status line's content so `render()` can measure its
/// wrapped height before laying out the rest of the frame. `?` now opens the
/// full, live keybinding reference (PHASE5B.md), so this hint line only
/// needs to stay short and point there — `q quit` stays spelled out
/// regardless, since "how do I leave" shouldn't require opening help first.
fn build_status_line(running: usize, blocked: usize, total: usize, prompt: Option<&str>, theme: &Theme) -> Line<'static> {
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
        // `d/D drop` earns its place back on-screen (not just in the `?`
        // popup) — a user reported having no visible way to clean up a
        // `done` agent after PHASE5B.md's hint-line shortening dropped it
        // silently. "How do I get rid of a finished agent" is common and
        // urgent enough to not gate behind "thought to press `?` first".
        let hints: Vec<Span> = vec![
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Ctrl-l/↵", Style::default().fg(Color::Yellow)),
            Span::styled(" jump in  ", Style::default().fg(Color::DarkGray)),
            Span::styled("n/s", Style::default().fg(Color::Yellow)),
            Span::styled(" spawn  ", Style::default().fg(Color::DarkGray)),
            Span::styled("d/D", Style::default().fg(Color::Yellow)),
            Span::styled(" drop  ", Style::default().fg(Color::DarkGray)),
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::styled(" search  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
            Span::styled("?", Style::default().fg(Color::Yellow)),
            Span::styled(" help", Style::default().fg(Color::DarkGray)),
        ];
        let counts_text = if blocked > 0 {
            format!("{running}/{total} running · {blocked} blocked")
        } else {
            format!("{running}/{total} running")
        };
        let counts_style = if blocked > 0 {
            status_style(&AgentStatus::Blocked, theme)
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

/// Pure. Every action's current binding, in `Action::ALL` order, plus the
/// fixed non-configurable keys — what the `?` popup lists (PHASE5B.md Task
/// 3). Built straight from the live `Keybindings` struct, never a hardcoded
/// string list, so a remap (or a newly added `Action` some future change
/// forgets to label) can't silently drift out of sync with what the popup
/// actually shows.
pub fn help_rows(kb: &Keybindings) -> Vec<(String, &'static str)> {
    let mut rows: Vec<(String, &'static str)> =
        Action::ALL.iter().map(|&action| (kb.get(action).to_string(), action.label())).collect();
    rows.push(("enter/o".to_string(), "jump in (fixed alias)"));
    rows.push(("ctrl-c".to_string(), "quit (fixed alias)"));
    rows.push(("ctrl-h".to_string(), "leave pane (the only key a focused pane intercepts)"));
    rows
}

fn render_help_popup(frame: &mut Frame, area: Rect, keybindings: &Keybindings) {
    let rows = help_rows(keybindings);
    let height = rows.len() as u16 + 2;
    let Some(popup) = centered_rect(area, 58, height) else { return };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" help — any key closes ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, label)| {
            Line::from(vec![
                Span::styled(format!("{key:>10}  "), Style::default().fg(Color::Yellow)),
                Span::styled(*label, Style::default().fg(Color::White)),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_input_modal(frame: &mut Frame, area: Rect, input: &InputState) {
    let (title, label, submit_hint) = match &input.action {
        PendingAction::SpawnRoot => (" spawn root ", "repo path:", "spawn"),
        PendingAction::SpawnChild { .. } => (" spawn child ", "task:", "spawn"),
        PendingAction::Search => (" search ", "agent name:", "jump"),
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
            Span::styled(format!("{submit_hint}   "), Style::default().fg(Color::DarkGray)),
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

fn focused_border(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── status_badge / status_style ──────────────────────────────────────────

    #[test]
    fn idle_has_distinct_badge_and_style() {
        let theme = Theme::default();
        assert_eq!(status_badge(&AgentStatus::Idle), "◌");
        assert_eq!(status_style(&AgentStatus::Idle, &theme), Style::default().fg(Color::DarkGray));
        assert_ne!(status_badge(&AgentStatus::Idle), status_badge(&AgentStatus::Blocked));
    }

    #[test]
    fn blocked_is_red_and_bold() {
        let theme = Theme::default();
        assert_eq!(status_badge(&AgentStatus::Blocked), "!");
        assert_eq!(
            status_style(&AgentStatus::Blocked, &theme),
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
        let line = build_status_line(1, 0, 2, Some("drop 'agent'? (y/n)"), &Theme::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("drop 'agent'? (y/n)"));
        assert!(!text.contains("jump in"));
    }

    #[test]
    fn build_status_line_without_prompt_shows_hints_and_counts() {
        let line = build_status_line(1, 0, 2, None, &Theme::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1/2 running"));
        assert!(text.contains("jump in"));
    }

    #[test]
    fn build_status_line_shows_drop_on_screen_not_just_in_the_help_popup() {
        // Regression: PHASE5B.md's hint-line shortening dropped "d/D drop"
        // entirely, relying on the `?` popup alone — a real user reported
        // having no visible way to clean up a `done` agent as a result.
        let line = build_status_line(1, 0, 2, None, &Theme::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("drop"), "drop hint must stay visible on screen: {text}");
    }

    #[test]
    fn build_status_line_with_blocked_shows_blocked_count() {
        let line = build_status_line(1, 2, 4, None, &Theme::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1/4 running"));
        assert!(text.contains("2 blocked"));
    }

    #[test]
    fn build_status_line_without_blocked_omits_blocked_text() {
        let line = build_status_line(1, 0, 2, None, &Theme::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("blocked"));
    }

    // ── fuzzy_match (PHASE5B.md Task 1) ───────────────────────────────────────

    #[test]
    fn fuzzy_match_in_order_subsequence_matches() {
        assert!(fuzzy_match("atm", "auth-module").is_some());
    }

    #[test]
    fn fuzzy_match_is_case_insensitive() {
        assert!(fuzzy_match("AUTH", "auth-module").is_some());
        assert!(fuzzy_match("auth", "AUTH-MODULE").is_some());
    }

    #[test]
    fn fuzzy_match_out_of_order_does_not_match() {
        // "ma" never appears in that order in "auth-module" (a before m,
        // never m before a) — a subsequence match requires order.
        assert_eq!(fuzzy_match("ma", "auth"), None);
    }

    #[test]
    fn fuzzy_match_non_contiguous_still_matches() {
        assert!(fuzzy_match("aue", "auth-module").is_some());
    }

    #[test]
    fn fuzzy_match_no_match_is_none() {
        assert_eq!(fuzzy_match("xyz", "auth-module"), None);
    }

    #[test]
    fn fuzzy_match_empty_query_matches_everything() {
        assert_eq!(fuzzy_match("", "anything"), Some(0));
    }

    #[test]
    fn fuzzy_match_scores_contiguous_higher_than_scattered() {
        // "au" is contiguous in "auth-module"; "ae" matches the same length
        // but scattered (a...e) — contiguous must score strictly higher.
        let contiguous = fuzzy_match("au", "auth-module").unwrap();
        let scattered = fuzzy_match("ae", "auth-module").unwrap();
        assert!(contiguous > scattered, "contiguous={contiguous} scattered={scattered}");
    }

    // ── search_visibility ─────────────────────────────────────────────────────

    fn node(name: &str, children: Vec<AgentNode>) -> AgentNode {
        AgentNode {
            id: crate::agent::AgentId::new(),
            name: name.to_string(),
            status: AgentStatus::Running,
            role: AgentRole::Root,
            repo: "repo".to_string(),
            branch: "main".to_string(),
            adapter: "claude".to_string(),
            cwd: std::path::PathBuf::from("."),
            context_pct: None,
            children,
            expanded: true,
            status_since: std::time::Instant::now(),
        }
    }

    #[test]
    fn search_visibility_matches_by_name() {
        let roots = vec![node("auth-module", vec![])];
        let (visible, matched) = search_visibility(&roots, "auth");
        assert!(visible.contains(&roots[0].id));
        assert!(matched.contains(&roots[0].id));
    }

    #[test]
    fn search_visibility_keeps_a_non_matching_parent_for_context() {
        let child = node("write-tests", vec![]);
        let child_id = child.id.clone();
        let parent = node("implement-auth", vec![child]);
        let parent_id = parent.id.clone();
        let roots = vec![parent];

        let (visible, matched) = search_visibility(&roots, "write");

        assert!(visible.contains(&child_id));
        assert!(matched.contains(&child_id));
        // The parent doesn't match "write" itself, but stays visible for
        // context — just not marked as a direct match (so the UI dims it).
        assert!(visible.contains(&parent_id));
        assert!(!matched.contains(&parent_id));
    }

    #[test]
    fn search_visibility_excludes_unrelated_subtrees() {
        let unrelated = node("fix-login-bug", vec![]);
        let unrelated_id = unrelated.id.clone();
        let matching = node("auth-module", vec![]);
        let matching_id = matching.id.clone();
        let roots = vec![unrelated, matching];

        let (visible, _matched) = search_visibility(&roots, "auth");

        assert!(visible.contains(&matching_id));
        assert!(!visible.contains(&unrelated_id));
    }

    // ── help_rows (PHASE5B.md Task 3) ─────────────────────────────────────────

    #[test]
    fn help_rows_covers_every_action_exactly_once() {
        let kb = Keybindings::default();
        let rows = help_rows(&kb);
        for action in Action::ALL {
            let count = rows.iter().filter(|(_, label)| *label == action.label()).count();
            assert_eq!(count, 1, "{:?} should appear exactly once, appeared {count} times", action.label());
        }
    }

    #[test]
    fn help_rows_includes_the_fixed_non_configurable_keys() {
        let kb = Keybindings::default();
        let rows = help_rows(&kb);
        assert!(rows.iter().any(|(key, _)| key == "enter/o"));
        assert!(rows.iter().any(|(key, _)| key == "ctrl-c"));
        assert!(rows.iter().any(|(key, _)| key == "ctrl-h"));
    }

    #[test]
    fn help_rows_reflects_a_remap() {
        let kb = Keybindings { spawn_root: crate::config::KeyBinding::Char('a'), ..Keybindings::default() };
        let rows = help_rows(&kb);
        let spawn_root_row = rows.iter().find(|(_, label)| *label == Action::SpawnRoot.label()).unwrap();
        assert_eq!(spawn_root_row.0, "a");
    }

    // ── render-path perf (SCALE.md Task 1) ───────────────────────────────────

    /// Not run by default (`cargo test`); `cargo test -- --ignored` or a
    /// direct name match runs it. A pure timing check, not a correctness
    /// test: catches an accidental O(n²) creeping into the per-frame
    /// flatten+format path, which runs every tick regardless of whether
    /// anything changed (SCALE.md's risk #3).
    ///
    /// Tree shape (5 roots times 9 children, plus the 5 roots themselves)
    /// approximates the spec's "30 agents" fleet with headroom; 1000 frames
    /// approximates roughly 100 seconds of continuous rendering at a
    /// 10fps-equivalent poll rate.
    #[test]
    #[ignore]
    fn flatten_and_format_1000_frames_of_a_50_node_tree_stays_fast() {
        let mut tree = AgentTree::new();
        for r in 0..5 {
            let mut root = node(&format!("root-{r}"), vec![]);
            for c in 0..9 {
                root.children.push(node(&format!("root-{r}-child-{c}"), vec![]));
            }
            tree.add_root(root);
        }
        let theme = Theme::default();

        let start = std::time::Instant::now();
        for tick in 0..1000u64 {
            let flat = tree.flatten();
            for (i, n) in flat.iter().enumerate() {
                let _ = tree_row(n, i == 0, false, tick, &theme, 80);
            }
        }
        let elapsed = start.elapsed();

        eprintln!("1000 frames of a 50-node tree (flatten + row format): {elapsed:?}");
        // Generous ceiling -- this is a canary for an accidental O(n^2), not a
        // tight perf budget; a real regression would blow well past this.
        assert!(elapsed.as_millis() < 2000, "flatten+format got suspiciously slow: {elapsed:?}");
    }
}
