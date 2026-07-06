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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_blocked_have_distinct_labels() {
        assert_eq!(AgentStatus::Idle.label(), "idle");
        assert_eq!(AgentStatus::Blocked.label(), "blocked");
        assert_ne!(AgentStatus::Idle.label(), AgentStatus::Blocked.label());
    }

    #[test]
    fn idle_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentStatus::Idle).unwrap(), "\"idle\"");
    }

    #[test]
    fn blocked_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&AgentStatus::Blocked).unwrap(), "\"blocked\"");
    }
}
