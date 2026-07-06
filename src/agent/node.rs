use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentId, AgentStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    pub adapter: String,
    pub cwd: PathBuf,
    pub context_pct: Option<u8>,
    pub children: Vec<AgentNode>,
    pub expanded: bool,
}
