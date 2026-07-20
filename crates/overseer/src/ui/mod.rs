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

use overseer_core::agent::{AgentId, AgentNode, AgentRole, AgentStatus, AgentTree, Attention, AttentionKind, FlatNode};
use crate::app::{InputState, PendingAction};
use overseer_core::config::{Action, Keybindings, Theme};
use overseer_core::ipc::protocol::GridSnapshot;

pub use term_pane::render_term_pane;

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

/// Attention outranks lifecycle for the one-column tree badge. Provider-limit
/// reasons use `$`; permission uses `!`; otherwise lifecycle owns the badge.
/// This keeps overlap deterministic even if a blocked lifecycle and provider
/// attention arrive together.
pub(crate) fn agent_badge<'a>(status: &'a AgentStatus, attention: Option<&Attention>) -> &'a str {
    match attention.map(|attention| attention.kind) {
        Some(AttentionKind::RateLimit | AttentionKind::QuotaLimit | AttentionKind::Billing) => "$",
        Some(AttentionKind::Permission) => "!",
        _ => status_badge(status),
    }
}

/// The color/weight `status` renders with, wherever it appears in the UI.
/// Colors come from `theme` (PHASE5B.md); `Blocked`'s bold weight is fixed —
/// `[theme]` is "small and honest: the statuses + chrome, nothing else",
/// colors only.
pub(crate) fn status_style(status: &AgentStatus, theme: &Theme) -> Style {
    match status {
        AgentStatus::Spawning => Style::default().fg(term_pane::map_dto_color(theme.spawning)),
        AgentStatus::Running => Style::default().fg(term_pane::map_dto_color(theme.running)),
        AgentStatus::Blocked => {
            Style::default().fg(term_pane::map_dto_color(theme.blocked)).add_modifier(Modifier::BOLD)
        }
        AgentStatus::Idle => Style::default().fg(term_pane::map_dto_color(theme.idle)),
        AgentStatus::Done => Style::default().fg(term_pane::map_dto_color(theme.done)),
        AgentStatus::Error => Style::default().fg(term_pane::map_dto_color(theme.error)),
    }
}
/// Width of the tree column as a percentage of the full window — the pane
/// takes the rest.
const TREE_COLUMN_PCT: u16 = 25;

/// What one frame's `render` drew, returned for the caller to act on next
/// frame: PTY sizing (`pane_rect`) and mouse click hit-testing
/// (`tree_rect`/`tree_rows`) both need to know exactly what's on screen,
/// which only `render` itself can say for certain (the ~25/75 split and any
/// active search filter both live here).
pub struct RenderLayout {
    /// The pane's inner rect, for sizing every agent's PTY to it.
    pub pane_rect: Rect,
    /// The tree list's outer rect (border included), for mapping a mouse
    /// click's (column, row) to a row index.
    pub tree_rect: Rect,
    /// The agent id occupying each terminal line in the tree, top to bottom.
    /// Every two-line item contributes its id twice, and the vector is already
    /// search-filtered, so line `i` exactly matches the screen position.
    pub tree_rows: Vec<AgentId>,
}

/// Renders the whole window: the tree|pane split plus a full-width status
/// bar. `frame.area()` is now the entire terminal — there is no longer a
/// separate tmux pane on the right; `render_term_pane` draws the selected
/// agent's live grid directly into this same ratatui frame. Returns the
/// drawn layout so the caller can size every agent's PTY to it and hit-test
/// mouse clicks against the tree.
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    tree: &AgentTree,
    tick: u64,
    prompt: Option<&str>,
    input: Option<&InputState>,
    confirm_text: Option<&str>,
    pane_grid: Option<&GridSnapshot>,
    pane_focused: bool,
    theme: &Theme,
    keybindings: &Keybindings,
    show_help: bool,
) -> RenderLayout {
    let area = frame.area();
    let (running, blocked, total) = tree.agent_counts();
    let status_line = build_status_line(running, blocked, total, prompt, area.width, theme, keybindings);
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

    let flat = tree.flatten();
    // The detail pane / live grid always track the *real* cursor — a live
    // search filter only affects what the tree list shows, nothing moves
    // for real until Enter (PHASE5B.md).
    let selected = flat.get(tree.cursor).cloned();

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(detail_pane_height(&selected, theme))])
        .split(columns[0]);

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

    let tree_rect = left[0];
    let tree_rows: Vec<AgentId> = display_flat
        .iter()
        .flat_map(|node| [node.id.clone(), node.id.clone()])
        .collect();
    render_agent_tree(frame, display_cursor, tick, tree_rect, &display_flat, matched.as_ref(), theme, search_query);
    render_agent_detail(frame, left[1], &selected, theme);
    let pane_rect = render_term_pane(frame, columns[1], pane_grid, selected.as_ref(), pane_focused);
    render_status_bar(frame, status_line, outer[1]);

    // Drawn last, on top of everything. Search has no modal of its own — its
    // query lives in the tree pane's own title (see `render_agent_tree`) so
    // the live-filtered rows underneath it are never obscured by a floating
    // box; only the two prompts that need real typing room (spawn root/child)
    // still get a centered modal.
    if let Some(input) = input {
        if !matches!(input.action, PendingAction::Search) {
            render_spawn_modal(frame, area, input);
        }
    }

    if let Some(confirm_text) = confirm_text {
        render_confirm_modal(frame, area, confirm_text);
    }

    if show_help {
        render_help_popup(frame, area, keybindings);
    }

    RenderLayout { pane_rect, tree_rect, tree_rows }
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

/// Maps a mouse click's screen `(column, row)` to the id occupying that tree
/// line. Each two-line agent item appears twice in `tree_rows`, so either line
/// selects the same agent. `tree_rect` is
/// the `List`'s outer rect (border included) and `tree_rows` is the exact
/// top-to-bottom id order `render_agent_tree` drew into it (already
/// search-filtered/fold-aware, since it's read straight off `RenderLayout`
/// rather than recomputed here). `None` for a click on the border or past
/// the last row. Pure — no `Frame` needed — so it's directly unit-testable.
pub fn hit_test_tree(tree_rect: Rect, tree_rows: &[AgentId], column: u16, row: u16) -> Option<AgentId> {
    let inner_x0 = tree_rect.x.saturating_add(1);
    let inner_x1 = tree_rect.x.saturating_add(tree_rect.width).saturating_sub(1);
    let inner_y0 = tree_rect.y.saturating_add(1);
    let inner_y1 = tree_rect.y.saturating_add(tree_rect.height).saturating_sub(1);
    if column < inner_x0 || column >= inner_x1 || row < inner_y0 || row >= inner_y1 {
        return None;
    }
    let row_index = (row - inner_y0) as usize;
    tree_rows.get(row_index).cloned()
}

#[allow(clippy::too_many_arguments)]
fn render_agent_tree(
    frame: &mut Frame,
    cursor: usize,
    tick: u64,
    area: Rect,
    flat: &[FlatNode],
    matched: Option<&HashSet<AgentId>>,
    theme: &Theme,
    search_query: Option<&str>,
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
            let lines = tree_row(node, selected, dimmed, tick, theme, inner_width);
            if selected {
                ListItem::new(lines).style(Style::default().bg(Color::DarkGray))
            } else {
                ListItem::new(lines)
            }
        })
        .collect();

    // The query lives directly in the pane's own title bar instead of a
    // floating popup: the already-filtered rows below stay fully visible
    // while typing, and the border turns yellow as the same "you're in a
    // prompt" cue the spawn modal's border otherwise carries alone.
    let (title, border_style) = match search_query {
        Some(query) => (
            Line::from(vec![
                Span::styled(" / ", Style::default().fg(Color::Yellow)),
                Span::styled(query.to_string(), Style::default().fg(Color::White)),
                Span::styled("█ ", Style::default().fg(Color::Yellow)),
            ]),
            Style::default().fg(Color::Yellow),
        ),
        None => (Line::from(" WORKSPACES "), focused_border(true, theme)),
    };
    let block = Block::default().title(title).borders(Borders::ALL).border_style(border_style);

    frame.render_widget(List::new(items).block(block), area);
}

fn tree_row(node: &FlatNode, selected: bool, dimmed: bool, tick: u64, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let name_style = if dimmed {
        Style::default().fg(term_pane::map_dto_color(theme.idle))
    } else if selected {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let badge = if node.status == AgentStatus::Running && node.attention.is_none() {
        let frame = (tick as usize / 2) % SPINNER_FRAMES.len();
        SPINNER_FRAMES[frame].to_string()
    } else {
        agent_badge(&node.status, node.attention.as_ref()).to_string()
    };
    // Prefix + badge + the space separating it from the name.
    let left_fixed = node.prefix.chars().count() + 1 + 1;
    let row_width = width.saturating_sub(left_fixed);
    // Age only for blocked/idle (ATTENTION.md) — a running agent doesn't
    // need a clock, it's actively doing something.
    let age = matches!(node.status, AgentStatus::Blocked | AgentStatus::Idle)
        .then(|| format_age(node.status_since.elapsed()));
    let layout = format_tree_row(&node.name, node.status.label(), age.as_deref(), row_width);
    let gap =
        row_width.saturating_sub(layout.name.chars().count() + layout.status_word.chars().count()).max(1);

    // A dimmed (ancestor-context-only) row during search stays legible but
    // visually recedes behind actual matches — everything in `theme.idle`,
    // badge included, regardless of the node's real status color.
    let badge_style = if dimmed {
        Style::default().fg(term_pane::map_dto_color(theme.idle))
    } else if node.attention.is_some() {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        status_style(&node.status, theme)
    };
    let row_status_style = if dimmed {
        Style::default().fg(term_pane::map_dto_color(theme.idle))
    } else if node.status == AgentStatus::Blocked {
        status_style(&node.status, theme)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let primary = Line::from(vec![
        Span::styled(node.prefix.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(badge, badge_style),
        Span::raw(" "),
        Span::styled(layout.name, name_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(layout.status_word, row_status_style),
    ]);

    let metadata = format_tree_metadata(
        &node.branch,
        node.model_name.as_deref(),
        &node.adapter,
        width.saturating_sub(left_fixed),
    );
    let secondary = Line::from(vec![
        Span::styled(continuation_prefix(&node.prefix), Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(metadata, Style::default().fg(term_pane::map_dto_color(theme.idle))),
    ]);

    vec![primary, secondary]
}

/// Pure. Converts a first-line tree connector into the continuation shown beneath
/// it. Ancestor segments already encode their own continuation state, so only
/// this node's final connector segment changes.
fn continuation_prefix(prefix: &str) -> String {
    if let Some(rest) = prefix.strip_suffix("├ ") {
        format!("{rest}│ ")
    } else if let Some(rest) = prefix.strip_suffix("└ ") {
        format!("{rest}  ")
    } else {
        prefix.to_string()
    }
}

/// Pure. Composes the secondary tree line from the branch and the best known
/// harness identity. A verified model wins over the adapter fallback; a bare
/// shell contributes neither. The result always fits `width`.
fn format_tree_metadata(branch: &str, model_name: Option<&str>, adapter: &str, width: usize) -> String {
    let harness = model_name
        .filter(|name| !name.is_empty())
        .or_else(|| (adapter != "shell" && !adapter.is_empty()).then_some(adapter));
    let metadata = match (!branch.is_empty(), harness) {
        (true, Some(harness)) => {
            let full = format!("{branch} · {harness}");
            if full.graphemes(true).count() <= width || width < 5 {
                full
            } else {
                let content_width = width - 3;
                let branch_width = content_width / 2;
                let harness_width = content_width - branch_width;
                format!(
                    "{} · {}",
                    truncate_with_ellipsis(branch, branch_width),
                    truncate_with_ellipsis(harness, harness_width)
                )
            }
        }
        (true, None) => branch.to_string(),
        (false, Some(harness)) => harness.to_string(),
        (false, None) => String::new(),
    };
    truncate_with_ellipsis(&metadata, width)
}

/// Pure. Lays out one tree row's text: the name (truncated with `…` if it would
/// otherwise overflow `width`) and a right-aligned status word. Kept free of
/// styling so `tree_row` can color the status word specially when blocked;
/// this function only owns the width arithmetic.
struct TreeRowLayout {
    name: String,
    status_word: String,
}

fn format_tree_row(name: &str, status_label: &str, age: Option<&str>, width: usize) -> TreeRowLayout {
    let status_word = match age {
        Some(age) => format!("{status_label} {age}"),
        None => status_label.to_string(),
    };
    let right_len = status_word.chars().count();
    // Reserve at least 1 column of gap between name and the right block; if the
    // row is too narrow even for the right block alone, let the name collapse to
    // nothing rather than underflow — render() just clips an over-length line,
    // same as any other ratatui `Line`.
    let name_budget = width.saturating_sub(right_len + 1);
    let name = truncate_with_ellipsis(name, name_budget);
    TreeRowLayout { name, status_word }
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

/// Pure — the Details pane's content lines for a selected agent. Shared by
/// `render_agent_detail` and `detail_pane_height` so the reserved layout
/// space always matches exactly what gets drawn (never a stale guess that
/// leaves dead space, never one that clips a line).
fn detail_lines(node: &FlatNode, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
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
            Span::styled(
                if node.branch.is_empty() { "—".to_string() } else { node.branch.clone() },
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("id:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(node.id.short(), Style::default().fg(Color::DarkGray)),
        ]),
    ];
    if let Some(attention) = &node.attention {
        let age = attention.observed_at.elapsed().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled("attention: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ({})", attention.kind.label(), format_age(age)),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]));
        if let Some(message) = &attention.message {
            lines.push(Line::from(vec![
                Span::styled("message: ", Style::default().fg(Color::DarkGray)),
                Span::raw(truncate_with_ellipsis(message, 80)),
            ]));
        }
        if let Some(retry_at) = attention.retry_at {
            let retry = retry_at.duration_since(std::time::SystemTime::now()).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled("retry:  ", Style::default().fg(Color::DarkGray)),
                Span::raw(if retry.is_zero() { "now".to_string() } else { format!("in {}", format_age(retry)) }),
            ]));
        }
    }
    lines
}

/// Pure — how tall the Details pane needs to be (content rows + 2 for the
/// border) for the currently selected agent. Computed fresh each frame from
/// `detail_lines`, not a hardcoded constant, so the pane shrinks/grows with
/// what it's actually showing instead of reserving worst-case dead space.
fn detail_pane_height(selected: &Option<FlatNode>, theme: &Theme) -> u16 {
    let content_rows = match selected {
        Some(node) => detail_lines(node, theme).len(),
        None => 1,
    };
    content_rows as u16 + 2
}

fn render_agent_detail(frame: &mut Frame, area: Rect, selected: &Option<FlatNode>, theme: &Theme) {
    let content = match selected {
        Some(node) => detail_lines(node, theme),
        None => vec![Line::from(Span::styled(
            "  no agent selected",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let block = Block::default()
        .title(" DETAIL ")
        .borders(Borders::ALL)
        .border_style(focused_border(false, theme));

    frame.render_widget(Paragraph::new(content).wrap(Wrap { trim: false }).block(block), area);
}


/// Pure — builds the status line's content so `render()` can measure its
/// wrapped height before laying out the rest of the frame. `?` now opens the
/// full, live keybinding reference (PHASE5B.md), so this hint line only
/// needs to stay short and point there — `q quit` stays spelled out
/// regardless, since "how do I leave" shouldn't require opening help first.
fn build_status_line(
    running: usize,
    blocked: usize,
    total: usize,
    prompt: Option<&str>,
    width: u16,
    theme: &Theme,
    keybindings: &Keybindings,
) -> Line<'static> {
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
        // Drop controls earn their place back on-screen (not just in the `?`
        // popup) — a user reported having no visible way to clean up a
        // `done` agent after PHASE5B.md's hint-line shortening dropped it
        // silently. "How do I get rid of a finished agent" is common and
        // urgent enough to not gate behind "thought to press `?` first".
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
        let used = spans.iter().map(|span| span.content.chars().count()).sum::<usize>();
        for (index, hint) in fitting_footer_hints(keybindings, usize::from(width).saturating_sub(used))
            .into_iter()
            .enumerate()
        {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(hint.key, Style::default().fg(Color::Yellow)));
            spans.push(Span::styled(format!(" {}", hint.label), Style::default().fg(Color::DarkGray)));
        }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpRow {
    pub section: &'static str,
    pub key: String,
    pub label: &'static str,
}

#[derive(Clone)]
struct Hint {
    key: String,
    label: &'static str,
}

fn action_hint(kb: &Keybindings, action: Action, label: &'static str) -> Hint {
    Hint { key: kb.get(action).to_string(), label }
}

/// Returns only whole key-label pairs that fit. Quit is always retained; both
/// drop controls then receive priority because cleanup must remain visible.
fn fitting_footer_hints(kb: &Keybindings, available: usize) -> Vec<Hint> {
    let hints = [
        Hint { key: format!("{}/{}", kb.get(Action::NavDown), kb.get(Action::NavUp)), label: "nav" },
        Hint { key: format!("{}/↵", kb.get(Action::JumpIn)), label: "jump in" },
        action_hint(kb, Action::SpawnRoot, "workspace"),
        action_hint(kb, Action::SpawnChild, "child"),
        action_hint(kb, Action::Drop, "drop"),
        action_hint(kb, Action::DropRecursive, "drop+children"),
        action_hint(kb, Action::Search, "search"),
        action_hint(kb, Action::Quit, "quit"),
        action_hint(kb, Action::Help, "help"),
    ];
    let mut selected = Vec::new();
    let mut used = 0;
    for index in [7, 4, 5, 2, 3, 8, 0, 1, 6] {
        let hint = &hints[index];
        let pair_width = hint.key.chars().count() + 1 + hint.label.chars().count();
        let separator = if selected.is_empty() { 0 } else { 2 };
        if index == 7 || used + separator + pair_width <= available {
            used += separator + pair_width;
            selected.push(index);
        }
    }
    selected.sort_unstable();
    selected.into_iter().map(|index| hints[index].clone()).collect()
}

/// Complete, context-grouped interaction reference. Configurable rows and the
/// footer both resolve through `action_hint`, so remaps cannot make them drift.
pub fn help_rows(kb: &Keybindings) -> Vec<HelpRow> {
    let mut rows = Vec::new();
    let mut push_action = |section, action| {
        let hint = action_hint(kb, action, action.label());
        rows.push(HelpRow { section, key: hint.key, label: hint.label });
    };
    for action in [Action::NavDown, Action::NavUp, Action::ToggleExpand] {
        push_action("TREE NAVIGATION", action);
    }
    for action in [
        Action::JumpIn,
        Action::SpawnRoot,
        Action::SpawnChild,
        Action::Drop,
        Action::DropRecursive,
        Action::Quit,
        Action::Shutdown,
        Action::Search,
        Action::Help,
    ] {
        push_action("TREE ACTIONS", action);
    }
    rows.extend([
        HelpRow { section: "TREE ACTIONS", key: "ctrl-c".into(), label: "quit (fixed alias)" },
        HelpRow { section: "PANE FOCUS", key: "enter/o".into(), label: "jump in (fixed aliases)" },
        HelpRow { section: "PANE FOCUS", key: "ctrl-h".into(), label: "leave pane (only intercepted pane key)" },
        HelpRow { section: "SCROLLBACK (TREE FOCUS)", key: "ctrl-u/ctrl-d".into(), label: "half-page up/down" },
        HelpRow { section: "SCROLLBACK (TREE FOCUS)", key: "ctrl-y/ctrl-e".into(), label: "one line up/down" },
        HelpRow { section: "SCROLLBACK (TREE FOCUS)", key: "↑/↓".into(), label: "one wheel notch up/down" },
        HelpRow { section: "SCROLLBACK (TREE FOCUS)", key: "G".into(), label: "return to live bottom" },
        HelpRow { section: "MODALS", key: "enter".into(), label: "submit / confirm" },
        HelpRow { section: "MODALS", key: "esc".into(), label: "cancel" },
        HelpRow { section: "MODALS", key: "any key".into(), label: "close help" },
        HelpRow { section: "MOUSE", key: "tree click".into(), label: "select agent" },
        HelpRow { section: "MOUSE", key: "pane click".into(), label: "jump in" },
        HelpRow { section: "MOUSE", key: "wheel (tree)".into(), label: "scroll pane preview" },
        HelpRow { section: "MOUSE", key: "wheel (pane)".into(), label: "forward to agent, or scroll preview" },
        HelpRow { section: "BADGES", key: "$/!".into(), label: "limit/permission attention; no badge may mean unsupported" },
    ]);
    rows
}

fn render_help_popup(frame: &mut Frame, area: Rect, keybindings: &Keybindings) {
    let rows = help_rows(keybindings);
    let section_count = rows.iter().map(|row| row.section).collect::<HashSet<_>>().len();
    let height = (rows.len() + section_count) as u16 + 2;
    let Some(popup) = centered_rect(area, 72, height) else { return };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" help — any key closes ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = Vec::with_capacity(rows.len() + section_count);
    let mut section = None;
    for row in rows {
        if section != Some(row.section) {
            section = Some(row.section);
            lines.push(Line::from(Span::styled(
                row.section,
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{:>17}  ", row.key), Style::default().fg(Color::Yellow)),
            Span::styled(row.label, Style::default().fg(Color::White)),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders the centered text-input modal for the spawn-child prompt (`s`) —
/// the only prompt left that needs real typing room. `n` (spawn workspace)
/// dispatches immediately with no prompt at all, and `Search` renders inline
/// in the tree pane's own title instead (`render_agent_tree`).
fn render_spawn_modal(frame: &mut Frame, area: Rect, input: &InputState) {
    debug_assert!(matches!(input.action, PendingAction::SpawnChild { .. }));

    let Some(popup) = centered_rect(area, 56, 6) else { return };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" spawn child ")
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
        Paragraph::new(Line::from(Span::styled("name:", Style::default().fg(Color::DarkGray)))),
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

/// Renders the centered confirmation modal for `d`/`D`/`Q` — previously a
/// plain status-bar line, which was genuinely easy to miss (no visual weight
/// beyond wrapped text at the bottom of an already-narrow pane); now the same
/// bordered-popup treatment the spawn prompts get, red instead of yellow
/// since every action behind this modal is destructive. `message` is fully
/// composed by the caller (`tui::build_confirm_text`) — this function only
/// lays it out.
fn render_confirm_modal(frame: &mut Frame, area: Rect, message: &str) {
    let Some(popup) = centered_rect(area, 56, 7) else { return };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" confirm ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(message, Style::default().fg(Color::White))))
            .wrap(Wrap { trim: true }),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("y ", Style::default().fg(Color::Yellow)),
            Span::styled("confirm   ", Style::default().fg(Color::DarkGray)),
            Span::styled("n/Esc ", Style::default().fg(Color::Yellow)),
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

fn focused_border(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default().fg(term_pane::map_dto_color(theme.border_focused))
    } else {
        Style::default().fg(term_pane::map_dto_color(theme.border))
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

    // ── tree row metadata ────────────────────────────────────────────────────

    fn flat_node_with_adapter(name: &str, status: AgentStatus, adapter: &str, prefix: &str) -> FlatNode {
        FlatNode {
            id: AgentId::new(),
            name: name.to_string(),
            status,
            role: AgentRole::Child,
            repo: "repo".to_string(),
            branch: "main".to_string(),
            context_pct: None,
            model_name: None,
            attention: None,
            session_alive: true,
            has_children: false,
            prefix: prefix.to_string(),
            status_since: std::time::Instant::now(),
            adapter: adapter.to_string(),
        }
    }

    #[test]
    fn tree_row_uses_two_lines_without_a_fused_harness_glyph() {
        let theme = Theme::default();
        let node = flat_node_with_adapter("auth-module", AgentStatus::Idle, "opencode", "");
        let lines = tree_row(&node, false, false, 0, &theme, 40);
        assert_eq!(lines.len(), 2);
        let primary: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let secondary: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(primary.contains("◌ auth-module"), "expected badge then name, got: {primary}");
        assert!(!primary.contains('O'), "primary line must not contain a harness glyph: {primary}");
        assert_eq!(secondary.trim(), "main · opencode");
    }

    #[test]
    fn continuation_prefix_preserves_ancestor_connectors() {
        assert_eq!(continuation_prefix(""), "");
        assert_eq!(continuation_prefix("├ "), "│ ");
        assert_eq!(continuation_prefix("└ "), "  ");
        assert_eq!(continuation_prefix("│ └ "), "│   ");
    }

    #[test]
    fn tree_row_secondary_line_continues_to_the_next_sibling() {
        let mut root = node("workspace", vec![]);
        root.children.push(node("first-child", vec![]));
        root.children.push(node("second-child", vec![]));
        let mut tree = AgentTree::new();
        tree.add_root(root);

        let flat = tree.flatten();
        assert_eq!(flat[1].prefix, "├ ");
        let lines = tree_row(&flat[1], false, false, 0, &Theme::default(), 40);
        let secondary: String = lines[1].spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(secondary.starts_with("│   "), "expected a continuing guide, got: {secondary}");
    }

    #[test]
    fn tree_row_shell_workspace_omits_harness_metadata() {
        let theme = Theme::default();
        let node = flat_node_with_adapter("overseer", AgentStatus::Idle, "shell", "");
        let lines = tree_row(&node, false, false, 0, &theme, 40);
        let secondary: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(secondary.trim(), "main");
    }

    #[test]
    fn tree_row_narrow_width_does_not_panic() {
        let theme = Theme::default();
        for width in 0..8 {
            for adapter in ["claude", "opencode", "pi", "shell", ""] {
                for prefix in ["", "├ ", "└ ", "│ └ "] {
                    let node = flat_node_with_adapter("some-task", AgentStatus::Blocked, adapter, prefix);
                    let _ = tree_row(&node, false, false, 0, &theme, width);
                }
            }
        }
    }

    // ── attention badges (HARNESS-CAPABILITIES.md) ───────────────────────────

    fn test_attention(kind: AttentionKind) -> Attention {
        Attention { kind, message: None, retry_at: None, observed_at: std::time::SystemTime::now() }
    }

    #[test]
    fn provider_limit_badge_precedes_blocked_lifecycle() {
        assert_eq!(agent_badge(&AgentStatus::Blocked, Some(&test_attention(AttentionKind::RateLimit))), "$");
        assert_eq!(agent_badge(&AgentStatus::Blocked, Some(&test_attention(AttentionKind::QuotaLimit))), "$");
        assert_eq!(agent_badge(&AgentStatus::Blocked, Some(&test_attention(AttentionKind::Billing))), "$");
    }

    #[test]
    fn permission_badge_precedes_non_blocked_lifecycle() {
        assert_eq!(agent_badge(&AgentStatus::Running, Some(&test_attention(AttentionKind::Permission))), "!");
    }

    #[test]
    fn attention_badge_survives_narrow_tree_widths() {
        let mut tree = AgentTree::new();
        let mut root = node("long-agent-name", vec![]);
        root.status = AgentStatus::Blocked;
        root.attention = Some(test_attention(AttentionKind::RateLimit));
        tree.add_root(root);
        let flat = tree.flatten();
        for width in 0..6 {
            let lines = tree_row(&flat[0], false, false, 0, &Theme::default(), width);
            assert_eq!(lines[0].spans[1].content, "$", "badge must survive width {width}");
        }
    }

    #[test]
    fn metadata_prefers_model_over_harness_fallback() {
        assert_eq!(
            format_tree_metadata("ovsr/auth", Some("anthropic/claude-sonnet-5"), "claude", 80),
            "ovsr/auth · anthropic/claude-sonnet-5"
        );
    }

    #[test]
    fn metadata_composes_branch_and_harness_fallback() {
        assert_eq!(format_tree_metadata("ovsr/auth", None, "opencode", 80), "ovsr/auth · opencode");
        assert_eq!(format_tree_metadata("", None, "pi", 80), "pi");
    }

    #[test]
    fn metadata_omits_bare_shell_and_supports_a_blank_line() {
        assert_eq!(format_tree_metadata("main", None, "shell", 80), "main");
        assert_eq!(format_tree_metadata("", None, "shell", 80), "");
    }

    #[test]
    fn metadata_truncates_safely_at_narrow_widths() {
        assert_eq!(format_tree_metadata("long-branch", None, "claude", 1), "…");
        for width in 0..8 {
            assert!(format_tree_metadata("long-branch", Some("provider/long-model"), "claude", width)
                .graphemes(true)
                .count()
                <= width);
        }
        assert_eq!(
            format_tree_metadata("overseer/12345678", Some("anthropic/claude-sonnet-5"), "claude", 17),
            "overse… · anthro…"
        );
    }

    // ── format_tree_row / truncate_with_ellipsis ─────────────────────────────

    #[test]
    fn format_tree_row_fits_without_truncation_in_a_roomy_width() {
        let layout = format_tree_row("auth-module", "idle", None, 40);
        assert_eq!(layout.name, "auth-module");
        assert_eq!(layout.status_word, "idle");
    }

    #[test]
    fn format_tree_row_formats_running_status() {
        let layout = format_tree_row("write-tests", "running", None, 40);
        assert_eq!(layout.status_word, "running");
    }

    #[test]
    fn format_tree_row_truncates_long_name_with_ellipsis() {
        let layout = format_tree_row("a-very-long-task-description-here", "blocked", None, 20);
        assert!(layout.name.ends_with('…'));
        assert!(layout.name.chars().count() < "a-very-long-task-description-here".chars().count());
    }

    #[test]
    fn format_tree_row_narrow_width_does_not_panic() {
        for width in 0..6 {
            let layout = format_tree_row("some-task", "blocked", None, width);
            // The status block survives intact even if the name collapses.
            assert_eq!(layout.status_word, "blocked");
        }
    }

    #[test]
    fn format_tree_row_appends_age_to_the_status_word() {
        let layout = format_tree_row("update-docs", "blocked", Some("2m"), 40);
        assert_eq!(layout.status_word, "blocked 2m");
    }

    #[test]
    fn format_tree_row_age_survives_at_a_narrow_width() {
        for width in 0..6 {
            let layout = format_tree_row("some-task", "idle", Some("12m"), width);
            assert_eq!(layout.status_word, "idle 12m");
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
        let line = build_status_line(
            1, 0, 2, Some("drop 'agent'? (y/n)"), 160, &Theme::default(), &Keybindings::default(),
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("drop 'agent'? (y/n)"));
        assert!(!text.contains("jump in"));
    }

    #[test]
    fn build_status_line_without_prompt_shows_hints_and_counts() {
        let line = build_status_line(1, 0, 2, None, 160, &Theme::default(), &Keybindings::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1/2 running"));
        assert!(text.contains("jump in"));
    }

    #[test]
    fn build_status_line_shows_drop_on_screen_not_just_in_the_help_popup() {
        // Regression: PHASE5B.md's hint-line shortening dropped "d/D drop"
        // entirely, relying on the `?` popup alone — a real user reported
        // having no visible way to clean up a `done` agent as a result.
        let line = build_status_line(1, 0, 2, None, 160, &Theme::default(), &Keybindings::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("d drop"), "drop hint must stay visible on screen: {text}");
        assert!(text.contains("D drop+children"), "recursive drop hint must stay visible: {text}");
    }

    #[test]
    fn build_status_line_distinguishes_workspace_child_and_recursive_actions() {
        let line = build_status_line(1, 0, 2, None, 160, &Theme::default(), &Keybindings::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        for hint in ["n workspace", "s child", "d drop", "D drop+children"] {
            assert!(text.contains(hint), "missing distinct footer hint {hint:?}: {text}");
        }
        assert!(!text.contains("n/s spawn"));
        assert!(!text.contains("d/D drop"));
    }

    #[test]
    fn build_status_line_reflects_live_keybinding_remaps() {
        let kb = Keybindings {
            spawn_root: overseer_core::config::KeyBinding::Char('w'),
            spawn_child: overseer_core::config::KeyBinding::Char('c'),
            drop: overseer_core::config::KeyBinding::Char('x'),
            drop_recursive: overseer_core::config::KeyBinding::Char('X'),
            help: overseer_core::config::KeyBinding::Char('h'),
            ..Keybindings::default()
        };
        let line = build_status_line(1, 0, 2, None, 160, &Theme::default(), &kb);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        for hint in ["w workspace", "c child", "x drop", "X drop+children", "h help"] {
            assert!(text.contains(hint), "footer ignored remapped hint {hint:?}: {text}");
        }
        assert!(!text.contains("n workspace"));
    }

    #[test]
    fn build_status_line_with_blocked_shows_blocked_count() {
        let line = build_status_line(1, 2, 4, None, 160, &Theme::default(), &Keybindings::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1/4 running"));
        assert!(text.contains("2 blocked"));
    }

    #[test]
    fn build_status_line_without_blocked_omits_blocked_text() {
        let line = build_status_line(1, 0, 2, None, 160, &Theme::default(), &Keybindings::default());
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
            id: overseer_core::agent::AgentId::new(),
            name: name.to_string(),
            status: AgentStatus::Running,
            role: AgentRole::Root,
            repo: "repo".to_string(),
            branch: "main".to_string(),
            adapter: "claude".to_string(),
            cwd: std::path::PathBuf::from("."),
            context_pct: None,
            model_name: None,
            attention: None,
            session_alive: true,
            children,
            expanded: true,
            status_since: std::time::Instant::now(),
            last_status_pushed_at: None,
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

    // ── hit_test_tree ──────────────────────────────────────────────────────────

    fn click_tree() -> AgentTree {
        let mut root = node("implement-auth", vec![]);
        let mut child_a = node("auth-module", vec![node("grandchild", vec![])]);
        child_a.role = AgentRole::Child;
        child_a.expanded = false; // folded: its grandchild is hidden from flatten()
        let mut child_b = node("write-tests", vec![]);
        child_b.role = AgentRole::Child;
        root.children.push(child_a);
        root.children.push(child_b);
        let mut tree = AgentTree::new();
        tree.add_root(root);
        tree
    }

    fn ids(flat: &[FlatNode]) -> Vec<AgentId> {
        flat.iter().flat_map(|node| [node.id.clone(), node.id.clone()]).collect()
    }

    #[test]
    fn three_level_tree_indents_and_folds_grandchildren() {
        let grandchild = node("lookup", vec![]);
        let grandchild_id = grandchild.id.clone();
        let child = node("implementation", vec![grandchild]);
        let child_id = child.id.clone();
        let root = node("workspace", vec![child]);
        let mut tree = AgentTree::new();
        tree.add_root(root);

        let flat = tree.flatten();
        assert_eq!(flat.iter().map(|n| n.prefix.as_str()).collect::<Vec<_>>(), vec!["", "└ ", "  └ "]);
        assert_eq!(flat[2].id, grandchild_id);

        tree.find_mut(&child_id).unwrap().expanded = false;
        let folded = tree.flatten();
        assert_eq!(folded.len(), 2);
        assert!(!folded.iter().any(|n| n.id == grandchild_id));
    }

    #[test]
    fn hit_test_tree_selects_the_row_clicked_with_a_fold_active() {
        let tree = click_tree();
        let flat = tree.flatten();
        // Folded: root, auth-module, write-tests — the grandchild never
        // appears, so a naive "index into the full unfiltered tree" would be
        // off by one here if it didn't respect the fold.
        assert_eq!(flat.len(), 3);
        let rows = ids(&flat);
        let tree_rect = Rect { x: 0, y: 0, width: 30, height: 10 };

        // Both lines of item 0 ("implement-auth") select the root.
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 1), Some(rows[0].clone()));
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 2), Some(rows[0].clone()));
        // Both lines of item 1 ("auth-module").
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 3), Some(rows[2].clone()));
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 4), Some(rows[2].clone()));
        // Both lines of item 2 ("write-tests").
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 5), Some(rows[4].clone()));
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 6), Some(rows[4].clone()));
    }

    #[test]
    fn hit_test_tree_selects_the_row_clicked_during_search() {
        let tree = click_tree();
        let flat = tree.flatten();
        // Simulates what `render` passes when a search query is active: only
        // the matching rows (here, dropping "auth-module" at index 1), same
        // as `display_flat` after filtering by `visible`.
        let filtered: Vec<FlatNode> = flat.iter().filter(|n| n.name != "auth-module").cloned().collect();
        assert_eq!(filtered.len(), 2);
        let rows = ids(&filtered);
        let tree_rect = Rect { x: 0, y: 0, width: 30, height: 10 };

        // Screen rows 3-4 are now "write-tests" (item 1 in the filtered list),
        // not "auth-module" — hit-testing must follow the rows actually
        // drawn, not the full tree's positions.
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 3), Some(rows[2].clone()));
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 4), Some(rows[2].clone()));
        assert_ne!(hit_test_tree(tree_rect, &rows, 5, 3), Some(flat[1].id.clone()));
    }

    #[test]
    fn hit_test_tree_click_on_border_is_none() {
        let rows = ids(&click_tree().flatten());
        let tree_rect = Rect { x: 0, y: 0, width: 30, height: 10 };
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 0), None, "top border row");
        assert_eq!(hit_test_tree(tree_rect, &rows, 0, 1), None, "left border column");
        assert_eq!(hit_test_tree(tree_rect, &rows, 29, 1), None, "right border column");
    }

    #[test]
    fn hit_test_tree_click_past_the_last_row_is_none() {
        let rows = ids(&click_tree().flatten());
        let tree_rect = Rect { x: 0, y: 0, width: 30, height: 10 };
        // Three two-line items occupy screen rows 1..=6; row 7 is blank.
        assert_eq!(hit_test_tree(tree_rect, &rows, 5, 7), None);
    }

    #[test]
    fn hit_test_tree_click_outside_tree_rect_entirely_is_none() {
        let rows = ids(&click_tree().flatten());
        let tree_rect = Rect { x: 0, y: 0, width: 30, height: 10 };
        // e.g. a click that landed on the pane instead of the tree.
        assert_eq!(hit_test_tree(tree_rect, &rows, 50, 1), None);
    }

    // ── help_rows (PHASE5B.md Task 3) ─────────────────────────────────────────

    #[test]
    fn help_rows_covers_every_action_exactly_once() {
        let kb = Keybindings::default();
        let rows = help_rows(&kb);
        for action in Action::ALL {
            let key = kb.get(action).to_string();
            let count = rows.iter().filter(|row| row.key == key && row.label == action.label()).count();
            assert_eq!(count, 1, "{:?} should appear exactly once, appeared {count} times", action.label());
        }
    }

    #[test]
    fn help_rows_includes_the_fixed_non_configurable_keys() {
        let kb = Keybindings::default();
        let rows = help_rows(&kb);
        assert!(rows.iter().any(|row| row.key == "enter/o"));
        assert!(rows.iter().any(|row| row.key == "ctrl-c"));
        assert!(rows.iter().any(|row| row.key == "ctrl-h"));
    }

    #[test]
    fn help_rows_includes_scrollback_and_mouse_sections() {
        let rows = help_rows(&Keybindings::default());
        for key in ["ctrl-u/ctrl-d", "ctrl-y/ctrl-e", "↑/↓", "G"] {
            assert!(rows.iter().any(|row| row.section == "SCROLLBACK (TREE FOCUS)" && row.key == key));
        }
        for key in ["tree click", "pane click", "wheel (tree)", "wheel (pane)"] {
            assert!(rows.iter().any(|row| row.section == "MOUSE" && row.key == key));
        }
    }

    #[test]
    fn help_rows_reflects_a_remap() {
        let kb = Keybindings { spawn_root: overseer_core::config::KeyBinding::Char('a'), ..Keybindings::default() };
        let rows = help_rows(&kb);
        let spawn_root_row = rows.iter().find(|row| row.label == Action::SpawnRoot.label()).unwrap();
        assert_eq!(spawn_root_row.key, "a");
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

    // ── render_agent_detail ──────────────────────────────────────────────────

    #[test]
    fn render_agent_detail_shows_dash_for_empty_branch() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let flat_node = FlatNode {
            id: AgentId::new(),
            name: "scratch".to_string(),
            status: AgentStatus::Idle,
            role: AgentRole::Root,
            repo: "scratch".to_string(),
            branch: String::new(), // non-git root: no fake branch name
            context_pct: None,
            model_name: None,
            attention: None,
            session_alive: true,
            has_children: false,
            prefix: String::new(),
            status_since: std::time::Instant::now(),
            adapter: "claude".to_string(),
        };
        let theme = Theme::default();
        let selected = Some(flat_node);

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_agent_detail(frame, frame.area(), &selected, &theme);
            })
            .unwrap();

        let content: String =
            terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("branch: —"),
            "expected a dash placeholder for an empty branch, got: {content}"
        );
    }

    #[test]
    fn detail_pane_height_tracks_content_not_a_worst_case_constant() {
        let theme = Theme::default();
        let base = FlatNode {
            id: AgentId::new(),
            name: "scratch".to_string(),
            status: AgentStatus::Idle,
            role: AgentRole::Root,
            repo: "scratch".to_string(),
            branch: "main".to_string(),
            context_pct: None,
            model_name: None,
            attention: None,
            session_alive: true,
            has_children: false,
            prefix: String::new(),
            status_since: std::time::Instant::now(),
            adapter: "claude".to_string(),
        };

        // 6 fixed fields (name/status/since/repo/branch/id) + 2 border rows,
        // no attention block — this used to always reserve 17 rows
        // regardless of content, leaving a large dead gap under a short row.
        assert_eq!(detail_pane_height(&Some(base.clone()), &theme), 8);

        // No agent selected: one placeholder line + border.
        assert_eq!(detail_pane_height(&None, &theme), 3);

        // A full attention block (kind + message + retry) adds exactly 3 rows.
        let mut with_attention = base;
        with_attention.attention = Some(Attention {
            kind: AttentionKind::Permission,
            message: Some("needs approval".to_string()),
            retry_at: Some(std::time::SystemTime::now() + std::time::Duration::from_secs(30)),
            observed_at: std::time::SystemTime::now(),
        });
        assert_eq!(detail_pane_height(&Some(with_attention), &theme), 11);
    }
}
