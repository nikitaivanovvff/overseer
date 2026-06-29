use super::{AgentId, AgentStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRole {
    Root,
    Child,
}

#[derive(Debug, Clone)]
pub struct AgentNode {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub repo: String,
    pub branch: String,
    pub context_pct: Option<u8>,
    pub children: Vec<AgentNode>,
    pub expanded: bool,
}

impl AgentNode {
    pub fn new_root(name: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            id: AgentId::new(),
            name: name.into(),
            status: AgentStatus::Running,
            role: AgentRole::Root,
            repo: repo.into(),
            branch: "main".to_string(),
            context_pct: None,
            children: Vec::new(),
            expanded: true,
        }
    }

    pub fn new_child(name: impl Into<String>, repo: impl Into<String>) -> Self {
        let id = AgentId::new();
        let branch = format!("overseer/{}", id.short());
        Self {
            id,
            name: name.into(),
            status: AgentStatus::Running,
            role: AgentRole::Child,
            repo: repo.into(),
            branch,
            context_pct: None,
            children: Vec::new(),
            expanded: true,
        }
    }
}
