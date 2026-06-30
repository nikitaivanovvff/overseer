use anyhow::Result;

use crate::agent::adapters::{AgentAdapter, LaunchContext};
use crate::agent::AgentId;
use crate::session::TmuxClient;

/// Derives a deterministic tmux session name from an agent id.
/// Phase 4 drop / Phase 5 focus can locate the session without a registry.
pub fn tmux_session_name(id: &AgentId) -> String {
    format!("overseer-{}", id.short())
}

/// Launches `ctx` in a new tmux session using the given adapter.
/// Pure I/O boundary: all logic lives in the adapter; this function only orchestrates.
pub fn launch(ctx: &LaunchContext, adapter: &dyn AgentAdapter, tmux: &TmuxClient) -> Result<()> {
    let session = tmux_session_name(&ctx.agent_id);
    let cmd = adapter.spawn_command(ctx);
    let env = adapter.env_inject(ctx);
    tmux.launch(&session, &ctx.cwd, &cmd, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRole};
    use crate::agent::adapters::claude::ClaudeAdapter;
    use crate::agent::adapters::LaunchContext;
    use std::path::PathBuf;

    #[test]
    fn session_name_is_deterministic() {
        let id = AgentId::new();
        let name1 = tmux_session_name(&id);
        let name2 = tmux_session_name(&id);
        assert_eq!(name1, name2);
        assert!(name1.starts_with("overseer-"));
    }

    #[test]
    fn session_name_differs_across_ids() {
        let a = tmux_session_name(&AgentId::new());
        let b = tmux_session_name(&AgentId::new());
        assert_ne!(a, b);
    }

    #[test]
    fn launch_dry_run_succeeds() {
        let adapter = ClaudeAdapter::with_bin(PathBuf::from("/usr/local/bin/overseer"));
        let tmux = TmuxClient::dry_run();
        let ctx = LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Root,
            parent_id: None,
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/tmp"),
            task: "test task".to_string(),
            command: "claude".to_string(),
            extra_args: vec![],
        };
        launch(&ctx, &adapter, &tmux).unwrap();
    }
}
