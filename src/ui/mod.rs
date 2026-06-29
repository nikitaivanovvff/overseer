use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use crate::{
    agent::{AgentStatus, FlatNode},
    app::{App, Focus},
};

pub fn render(frame: &mut Frame, app: &App) {
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

    render_agent_tree(frame, app, left[0]);
    render_agent_detail(frame, app, left[1]);
    render_pane(frame, app, body[1]);
    render_status_bar(frame, app, outer[1]);
}

fn render_agent_tree(frame: &mut Frame, app: &App, area: Rect) {
    let flat = app.agent_tree.flatten();
    let focused = app.focus == Focus::Tree;

    let items: Vec<ListItem> = flat
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let selected = i == app.agent_tree.cursor;
            let line = tree_row(node, selected);
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

fn tree_row(node: &FlatNode, selected: bool) -> Line<'static> {
    let name_style = if selected {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    Line::from(vec![
        Span::styled(node.prefix.clone(), Style::default().fg(Color::DarkGray)),
        Span::styled(node.status.badge(), status_style(&node.status)),
        Span::raw(" "),
        Span::styled(node.name.clone(), name_style),
    ])
}

fn render_agent_detail(frame: &mut Frame, app: &App, area: Rect) {
    let content = match app.agent_tree.selected() {
        Some(node) => vec![
            Line::from(vec![
                Span::styled("task:   ", Style::default().fg(Color::DarkGray)),
                Span::raw(node.name),
            ]),
            Line::from(vec![
                Span::styled("status: ", Style::default().fg(Color::DarkGray)),
                Span::styled(node.status.label(), status_style(&node.status)),
            ]),
            Line::from(vec![
                Span::styled("repo:   ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    node.repo.unwrap_or_else(|| "—".to_string()),
                    Style::default().fg(Color::Cyan),
                ),
            ]),
            Line::from(vec![
                Span::styled("branch: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    node.branch.unwrap_or_else(|| "—".to_string()),
                    Style::default().fg(Color::Yellow),
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
        .border_style(Style::default().fg(Color::DarkGray));

    frame.render_widget(Paragraph::new(content).block(block), area);
}

fn render_pane(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Pane;
    let selected = app.agent_tree.selected();

    // Split pane area: 1-line repo/branch header + rest for the terminal
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    render_pane_header(frame, &selected, chunks[0]);
    render_pane_body(frame, &selected, focused, chunks[1]);
}

fn render_pane_header(frame: &mut Frame, selected: &Option<FlatNode>, area: Rect) {
    let line = match selected {
        Some(node) => {
            let repo = node.repo.clone().unwrap_or_else(|| "—".to_string());
            let branch = node.branch.clone().unwrap_or_else(|| "—".to_string());
            Line::from(vec![
                Span::raw(" "),
                Span::styled("repo: ", Style::default().fg(Color::DarkGray)),
                Span::styled(repo, Style::default().fg(Color::Cyan)),
                Span::raw("   "),
                Span::styled("branch: ", Style::default().fg(Color::DarkGray)),
                Span::styled(branch, Style::default().fg(Color::Yellow)),
            ])
        }
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
            "  press Tab to focus this panel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .title(" PANE ")
        .borders(Borders::ALL)
        .border_style(focused_border(focused));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let flat = app.agent_tree.flatten();
    let running = flat.iter().filter(|n| n.status == AgentStatus::Running).count();
    let total = flat.len();

    let bar = Line::from(vec![
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
        Span::styled("j/k", Style::default().fg(Color::Yellow)),
        Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("<space>", Style::default().fg(Color::Yellow)),
        Span::styled(" expand  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Tab", Style::default().fg(Color::Yellow)),
        Span::styled(" focus  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]);

    frame.render_widget(Paragraph::new(bar), area);
}

fn status_style(status: &AgentStatus) -> Style {
    match status {
        AgentStatus::Spawning => Style::default().fg(Color::Cyan),
        AgentStatus::Running => Style::default().fg(Color::Green),
        AgentStatus::Waiting => Style::default().fg(Color::Yellow),
        AgentStatus::Done => Style::default().fg(Color::Blue),
        AgentStatus::Error => Style::default().fg(Color::Red),
    }
}

fn focused_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}
