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
    /// When `status` last actually changed (ATTENTION.md) — reset by
    /// `AgentRegistry::set_status` only when the new value differs from the
    /// old one, so repeated same-status pushes (e.g. `PostToolUse` spam on a
    /// `running` agent) don't reset the clock. Shipped across the wire as an
    /// age (`AgentDto::status_secs`), never as this `Instant` itself.
    pub status_since: std::time::Instant,
}
