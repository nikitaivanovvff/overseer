use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use crate::{
    agent::{AgentStatus, AgentTree, FlatNode},
    app::Focus,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn render(frame: &mut Frame, focus: &Focus, tree: &AgentTree, tick: u64) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(outer[0]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(7)])
        .split(body[0]);

    let flat = tree.flatten();
    let selected = flat.get(tree.cursor).cloned();
    let (running, total) = tree.agent_counts();

    render_agent_tree(frame, focus == &Focus::Tree, tree.cursor, tick, left[0], &flat);
    render_agent_detail(frame, left[1], &selected);
    render_pane(frame, focus, body[1], &selected);
    render_status_bar(frame, focus, running, total, outer[1]);
}

fn render_agent_tree(
    frame: &mut Frame,
    focused: bool,
    cursor: usize,
    tick: u64,
    area: Rect,
    flat: &[FlatNode],
) {
    let items: Vec<ListItem> = flat
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let selected = i == cursor;
            let line = tree_row(node, selected, tick);
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
        .border_style(focused_border(focused));

    frame.render_widget(List::new(items).block(block), area);
}

fn tree_row(node: &FlatNode, selected: bool, tick: u64) -> Line<'static> {
    let name_style = if selected {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let badge = if node.status == AgentStatus::Running {
        let frame = (tick as usize / 2) % SPINNER_FRAMES.len();
        SPINNER_FRAMES[frame].to_string()
    } else {
        node.status.badge().to_string()
    };

    Line::from(vec![
        Span::styled(node.prefix.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(badge, node.status.style()),
        Span::raw(" "),
        Span::styled(node.name.clone(), name_style),
    ])
}

fn render_agent_detail(frame: &mut Frame, area: Rect, selected: &Option<FlatNode>) {
    let content = match selected {
        Some(node) => vec![
            Line::from(vec![
                Span::styled("task:   ", Style::default().fg(Color::DarkGray)),
                Span::raw(node.name.clone()),
            ]),
            Line::from(vec![
                Span::styled("status: ", Style::default().fg(Color::DarkGray)),
                Span::styled(node.status.label(), node.status.style()),
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

fn render_pane(frame: &mut Frame, focus: &Focus, area: Rect, selected: &Option<FlatNode>) {
    let focused = focus == &Focus::Pane;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    render_pane_header(frame, selected, chunks[0]);
    render_pane_body(frame, selected, focused, chunks[1]);
}

fn render_pane_header(frame: &mut Frame, selected: &Option<FlatNode>, area: Rect) {
    let line = match selected {
        Some(node) => Line::from(vec![
            Span::raw(" "),
            Span::styled("repo: ", Style::default().fg(Color::DarkGray)),
            Span::styled(node.repo.clone(), Style::default().fg(Color::Cyan)),
            Span::raw("   "),
            Span::styled("branch: ", Style::default().fg(Color::DarkGray)),
            Span::styled(node.branch.clone(), Style::default().fg(Color::Yellow)),
        ]),
        None => Line::from(Span::styled(
            " no agent selected",
            Style::default().fg(Color::DarkGray),
        )),
    };

    frame.render_widget(Paragraph::new(line), area);
}

fn render_pane_body(frame: &mut Frame, selected: &Option<FlatNode>, focused: bool, area: Rect) {
    let task_name = selected
        .as_ref()
        .map(|n| n.name.clone())
        .unwrap_or_else(|| "none".to_string());

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  task: "),
            Span::styled(task_name, Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  pane embedding — phase 5",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Esc to return to agents",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .title(" PANE ")
        .borders(Borders::ALL)
        .border_style(focused_border(focused));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_status_bar(
    frame: &mut Frame,
    focus: &Focus,
    running: usize,
    total: usize,
    area: Rect,
) {
    let hints: Vec<Span> = match focus {
        Focus::Tree => vec![
            Span::styled("j/k", Style::default().fg(Color::Yellow)),
            Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
            Span::styled("<space>", Style::default().fg(Color::Yellow)),
            Span::styled(" fold  ", Style::default().fg(Color::DarkGray)),
            Span::styled("o/↵", Style::default().fg(Color::Yellow)),
            Span::styled(" open  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ],
        Focus::Pane => vec![
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::styled(" → agents  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ],
    };

    let mut spans = vec![
        Span::styled(
            " OVERSEER ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{running}/{total} running"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("   "),
    ];
    spans.extend(hints);

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
