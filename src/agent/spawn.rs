use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agent::adapters::{adapter_for, AgentAdapter, LaunchContext};
use crate::agent::registry::{RegisterArgs, RegisterResult, RegistryError};
use crate::agent::{AgentId, AgentRegistry, AgentRole};
use crate::session::TmuxClient;

/// Derives a deterministic tmux session name from an agent id.
/// Phase 4 drop / Phase 5 focus can locate the session without a registry.
pub fn tmux_session_name(id: &AgentId) -> String {
    format!("overseer-{}", id.short())
}

/// Launches `ctx` in a new tmux session using the given adapter.
/// Pure I/O boundary: all logic lives in the adapter; this function only orchestrates.
pub fn launch(
    ctx: &LaunchContext,
    adapter: &dyn AgentAdapter,
    tmux: &TmuxClient,
) -> anyhow::Result<()> {
    let session = tmux_session_name(&ctx.agent_id);
    let cmd = adapter.spawn_command(ctx);
    let env = adapter.env_inject(ctx);
    tmux.launch(&session, &ctx.cwd, &cmd, &env)
}

/// Everything needed to register + launch one agent. Used by both `Request::Start`
/// (role=Root, no parent) and `Request::Spawn` (role=Child, parent=caller).
pub struct SpawnRequest {
    pub role: AgentRole,
    pub parent_id: Option<AgentId>,
    pub task: String,
    pub adapter_name: String,
    pub cwd: PathBuf,
    pub repo: String,
    /// Explicit branch override (used by `start --branch`-style callers). `None` lets
    /// the registry apply its default ("main" for root, "overseer/<id>" for child).
    pub branch: Option<String>,
}

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("unknown adapter: {0}")]
    UnknownAdapter(String),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("launch failed: {0}")]
    Launch(anyhow::Error),
}

/// Registers a new agent and launches its tmux session in one step.
/// The single shared orchestration path for root (`start`) and child (`spawn`) launches.
pub fn spawn_agent(
    registry: &AgentRegistry,
    tmux: &TmuxClient,
    socket: &Path,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    let adapter = adapter_for(&req.adapter_name)
        .ok_or_else(|| SpawnError::UnknownAdapter(req.adapter_name.clone()))?;

    let args = RegisterArgs {
        id: None,
        name: req.task.clone(),
        role: req.role.clone(),
        parent_id: req.parent_id.clone(),
        adapter: req.adapter_name.clone(),
        repo: req.repo,
        cwd: req.cwd.clone(),
        branch: req.branch,
    };
    let result = registry.register(args)?;

    let launch_ctx = LaunchContext {
        agent_id: result.id.clone(),
        role: req.role,
        parent_id: req.parent_id,
        socket: socket.to_path_buf(),
        cwd: req.cwd,
        task: req.task,
        command: req.adapter_name,
        extra_args: vec![],
    };

    if let Err(e) = launch(&launch_ctx, adapter.as_ref(), tmux) {
        // Don't leave a phantom "Running" node behind with no session backing it.
        registry.remove(&result.id);
        return Err(SpawnError::Launch(e));
    }
    Ok(result)
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

    fn make_registry_and_tmux() -> (AgentRegistry, TmuxClient) {
        (AgentRegistry::new(), TmuxClient::dry_run())
    }

    fn base_request(role: AgentRole, parent_id: Option<AgentId>) -> SpawnRequest {
        SpawnRequest {
            role,
            parent_id,
            task: "do stuff".to_string(),
            adapter_name: "claude".to_string(),
            cwd: PathBuf::from("/tmp"),
            repo: "overseer".to_string(),
            branch: None,
        }
    }

    #[test]
    fn spawn_agent_root_succeeds() {
        let (registry, tmux) = make_registry_and_tmux();
        let result = spawn_agent(
            &registry,
            &tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap();
        assert_eq!(result.branch, "main");
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn spawn_agent_child_succeeds_under_known_parent() {
        let (registry, tmux) = make_registry_and_tmux();
        let root = spawn_agent(
            &registry,
            &tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let child = spawn_agent(
            &registry,
            &tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Child, Some(root.id)),
        )
        .unwrap();
        assert!(child.branch.starts_with("overseer/"));
        assert_eq!(registry.snapshot().len(), 2);
    }

    #[test]
    fn spawn_agent_unknown_adapter_errors() {
        let (registry, tmux) = make_registry_and_tmux();
        let mut req = base_request(AgentRole::Root, None);
        req.adapter_name = "nonexistent".to_string();
        let err = spawn_agent(&registry, &tmux, &PathBuf::from("/tmp/overseer.sock"), req)
            .unwrap_err();
        assert!(matches!(err, SpawnError::UnknownAdapter(name) if name == "nonexistent"));
    }

    #[test]
    fn spawn_agent_unknown_parent_errors() {
        let (registry, tmux) = make_registry_and_tmux();
        let err = spawn_agent(
            &registry,
            &tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Child, Some(AgentId::new())),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Registry(RegistryError::UnknownParent(_))));
    }

    #[test]
    fn spawn_agent_rolls_back_registration_on_launch_failure() {
        let registry = AgentRegistry::new();
        let failing_tmux = TmuxClient::dry_run_failing_launch();
        let err = spawn_agent(
            &registry,
            &failing_tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Launch(_)));
        // The failed launch must not leave a phantom "Running" node behind.
        assert!(registry.snapshot().is_empty());
    }
}
