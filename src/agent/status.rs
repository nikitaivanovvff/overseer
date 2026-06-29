use ratatui::style::{Color, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Spawning,
    Running,
    Waiting,
    Done,
    Error,
}

impl AgentStatus {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Spawning => "…",
            Self::Running => "●",
            Self::Waiting => "○",
            Self::Done => "✓",
            Self::Error => "✗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Error => "error",
        }
    }

    pub fn style(&self) -> Style {
        match self {
            Self::Spawning => Style::default().fg(Color::Cyan),
            Self::Running => Style::default().fg(Color::Green),
            Self::Waiting => Style::default().fg(Color::Yellow),
            Self::Done => Style::default().fg(Color::Blue),
            Self::Error => Style::default().fg(Color::Red),
        }
    }
}
