use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Registered, session launching / agent not reporting yet.
    Spawning,
    /// Actively working (tool use / responding).
    Running,
    /// Needs the user — a permission prompt is pending.
    Blocked,
    /// Finished responding, awaiting further prompting/attention. Also the
    /// bare-shell root state before the user runs anything inside it.
    Idle,
    /// The agent explicitly declared the task complete (`overseer status done`).
    /// Never inferred — see AGENTS.md "Status is push, not pull".
    Done,
    /// Process died unexpectedly.
    Error,
}

impl AgentStatus {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Spawning => "…",
            Self::Running => "●",
            Self::Blocked => "!",
            Self::Idle => "◌",
            Self::Done => "✓",
            Self::Error => "✗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Error => "error",
        }
    }

    pub fn style(&self) -> Style {
        match self {
            Self::Spawning => Style::default().fg(Color::Cyan),
            Self::Running => Style::default().fg(Color::Green),
            Self::Blocked => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
        assert_ne!(AgentStatus::Idle.badge(), AgentStatus::Blocked.badge());
        assert_ne!(AgentStatus::Idle.label(), AgentStatus::Blocked.label());
    }

    #[test]
    fn idle_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentStatus::Idle).unwrap(), "\"idle\"");
    }

    #[test]
    fn blocked_is_red_and_bold() {
        assert_eq!(AgentStatus::Blocked.badge(), "!");
        assert_eq!(AgentStatus::Blocked.label(), "blocked");
        assert_eq!(
            AgentStatus::Blocked.style(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn blocked_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentStatus::Blocked).unwrap(), "\"blocked\"");
    }
}
