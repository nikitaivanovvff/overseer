use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentId, AgentStatus, Attention};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// A top-level agent, one per repo. Wire/env value `root`
    /// (`OVERSEER_ROLE=root`) — presented to the user as *workspace*.
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
    pub model_name: Option<String>,
    pub attention: Option<Attention>,
    /// Ground truth for whether this agent's PTY process has actually
    /// exited, tracked independently of `status` (a self-reported `Done`
    /// does not mean the session died — see `AgentRegistry::mark_session_exited`).
    /// Defaults `true` at registration; the daemon's exit-code sweep is the
    /// only thing that ever flips it to `false`, and never back.
    pub session_alive: bool,
    pub children: Vec<AgentNode>,
    pub expanded: bool,
    /// When `status` last actually changed (ATTENTION.md) — reset by
    /// `AgentRegistry::set_status` only when the new value differs from the
    /// old one, so repeated same-status pushes (e.g. `PostToolUse` spam on a
    /// `running` agent) don't reset the clock. Shipped across the wire as an
    /// age (`AgentDto::status_secs`), never as this `Instant` itself.
    pub status_since: std::time::Instant,
    /// Wall-clock time the most recently *applied* `Request::Status` push was
    /// captured at the client (hook-invocation time, not daemon-arrival
    /// time) — see `AgentRegistry::set_status`'s staleness guard. `None`
    /// until the first push arrives (registration carries no timestamp of
    /// its own, so nothing to compare the first push against). Wall-clock
    /// (`SystemTime`), not `Instant`, because it must be comparable across
    /// the many independent short-lived OS processes each hook fire spawns —
    /// a monotonic clock has no shared origin across processes. Never sent
    /// across the wire; purely daemon-side bookkeeping.
    pub last_status_pushed_at: Option<std::time::SystemTime>,
}
