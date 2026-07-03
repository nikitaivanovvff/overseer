use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Spawning,
    Running,
    Waiting,
    /// Session launched, but no agent process has reported activity yet — the
    /// bare-shell root state before the user runs anything inside it. Distinct
    /// from `Waiting` (reserved for "an agent inside needs your approval").
    Idle,
    Done,
    Error,
}

impl AgentStatus {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Spawning => "…",
            Self::Running => "●",
            Self::Waiting => "○",
            Self::Idle => "◌",
            Self::Done => "✓",
            Self::Error => "✗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Error => "error",
        }
    }

    pub fn style(&self) -> Style {
        match self {
            Self::Spawning => Style::default().fg(Color::Cyan),
            Self::Running => Style::default().fg(Color::Green),
            Self::Waiting => Style::default().fg(Color::Yellow),
            Self::Idle => Style::default().fg(Color::DarkGray),
            Self::Done => Style::default().fg(Color::Blue),
            Self::Error => Style::default().fg(Color::Red),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_has_distinct_badge_label_and_style() {
        assert_eq!(AgentStatus::Idle.badge(), "◌");
        assert_eq!(AgentStatus::Idle.label(), "idle");
        assert_eq!(AgentStatus::Idle.style(), Style::default().fg(Color::DarkGray));
        assert_ne!(AgentStatus::Idle.badge(), AgentStatus::Waiting.badge());
        assert_ne!(AgentStatus::Idle.label(), AgentStatus::Waiting.label());
    }

    #[test]
    fn idle_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentStatus::Idle).unwrap(), "\"idle\"");
    }
}
