use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::agent::adapters::{adapter_for, identity_env, AgentAdapter, AgentIdentity, LaunchContext};
use crate::agent::registry::{RegisterArgs, RegisterResult, RegistryError};
use crate::agent::{AgentId, AgentRegistry, AgentRole, AgentStatus};
use crate::session::SessionManager;

/// Launches `ctx` in a new PTY session using the given adapter.
/// Pure I/O boundary: all logic lives in the adapter; this function only orchestrates.
pub fn launch(
    ctx: &LaunchContext,
    adapter: &dyn AgentAdapter,
    sessions: &SessionManager,
) -> anyhow::Result<()> {
    let cmd = adapter.spawn_command(ctx);
    let env = adapter.env_inject(ctx);
    sessions.launch(ctx.agent_id.clone(), &ctx.cwd, &cmd, &env)
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

/// Registers a new agent and launches its PTY session in one step.
/// The single shared orchestration entry point for root (`start`) and child (`spawn`)
/// launches — dispatches to a role-specific path since they no longer share a
/// launch mechanism (root is a bare shell, child is adapter-driven).
pub fn spawn_agent(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    match req.role.clone() {
        AgentRole::Root => spawn_root_shell(registry, sessions, socket, req),
        AgentRole::Child => spawn_child_agent(registry, sessions, socket, req),
    }
}

/// Root path: no `AgentAdapter`, no configured agent binary — just a plain shell
/// in the chosen repo, registered `Idle`. Whatever the user later runs inside
/// (e.g. `claude`) inherits the identity env vars via the PTY's normal
/// process-environment inheritance, and the existing PostToolUse/Stop hooks pick
/// it up from there — no new detection/polling code, this is pure push.
fn spawn_root_shell(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    let args = RegisterArgs {
        id: None,
        name: req.repo.clone(), // tree label = repo name, not typed task text
        role: AgentRole::Root,
        parent_id: None,
        adapter: "shell".to_string(), // honest label: nothing else was launched
        repo: req.repo.clone(),
        cwd: req.cwd.clone(),
        branch: req.branch,
        initial_status: AgentStatus::Idle,
    };
    let result = registry.register(args)?;

    let identity = AgentIdentity {
        agent_id: &result.id,
        role: &AgentRole::Root,
        parent_id: None,
        socket,
        repo: &req.repo,
    };
    let env = identity_env(&identity);
    let cmd = Command::new(resolve_shell());

    if let Err(e) = sessions.launch(result.id.clone(), &req.cwd, &cmd, &env) {
        registry.remove(&result.id);
        return Err(SpawnError::Launch(e));
    }
    Ok(result)
}

/// `$SHELL`, falling back to `/bin/sh`. No args — an interactive, non-login shell.
fn resolve_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Child path — unchanged from before the root/child split: adapter-driven,
/// registers `Running` since it auto-launches immediately.
fn spawn_child_agent(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    let adapter = adapter_for(&req.adapter_name)
        .ok_or_else(|| SpawnError::UnknownAdapter(req.adapter_name.clone()))?;

    let args = RegisterArgs {
        id: None,
        name: req.task.clone(),
        role: AgentRole::Child,
        parent_id: req.parent_id.clone(),
        adapter: req.adapter_name.clone(),
        repo: req.repo.clone(),
        cwd: req.cwd.clone(),
        branch: req.branch,
        initial_status: AgentStatus::Running,
    };
    let result = registry.register(args)?;

    let launch_ctx = LaunchContext {
        agent_id: result.id.clone(),
        role: AgentRole::Child,
        parent_id: req.parent_id,
        socket: socket.to_path_buf(),
        cwd: req.cwd,
        repo: req.repo,
        command: req.adapter_name,
        extra_args: vec![],
    };

    if let Err(e) = launch(&launch_ctx, adapter.as_ref(), sessions) {
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
    fn launch_dry_run_succeeds() {
        let adapter = ClaudeAdapter::with_bin(PathBuf::from("/usr/local/bin/overseer"));
        let sessions = SessionManager::dry_run();
        let ctx = LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Root,
            parent_id: None,
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/tmp"),
            repo: "myrepo".to_string(),
            command: "claude".to_string(),
            extra_args: vec![],
        };
        launch(&ctx, &adapter, &sessions).unwrap();
    }

    fn make_registry_and_sessions() -> (AgentRegistry, SessionManager) {
        (AgentRegistry::new(), SessionManager::dry_run())
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
        let (registry, sessions) = make_registry_and_sessions();
        let result = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap();
        assert_eq!(result.branch, "main");
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn spawn_agent_root_registers_idle_with_shell_adapter_label() {
        let (registry, sessions) = make_registry_and_sessions();
        let result = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap();
        let dto = registry.get(&result.id).unwrap();
        assert_eq!(dto.status, AgentStatus::Idle);
        assert_eq!(dto.adapter, "shell");
    }

    #[test]
    fn spawn_agent_root_names_node_after_repo_ignoring_task() {
        let (registry, sessions) = make_registry_and_sessions();
        let mut req = base_request(AgentRole::Root, None);
        req.task = "this text should never become the name".to_string();
        req.repo = "distinct-repo-name".to_string();
        let result = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), req)
            .unwrap();
        let dto = registry.get(&result.id).unwrap();
        assert_eq!(dto.name, "distinct-repo-name");
    }

    #[test]
    fn resolve_shell_uses_env_var() {
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(resolve_shell(), "/bin/zsh");
        std::env::remove_var("SHELL");
    }

    #[test]
    fn resolve_shell_falls_back_to_bin_sh() {
        std::env::remove_var("SHELL");
        assert_eq!(resolve_shell(), "/bin/sh");
    }

    #[test]
    fn spawn_agent_child_succeeds_under_known_parent() {
        let (registry, sessions) = make_registry_and_sessions();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let child = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Child, Some(root.id)),
        )
        .unwrap();
        assert!(child.branch.starts_with("overseer/"));
        assert_eq!(registry.snapshot().len(), 2);
    }

    #[test]
    fn spawn_agent_unknown_adapter_errors() {
        // Root no longer validates any adapter (it's always a bare shell), so this
        // now has to go through a child spawn — the only path that still consults one.
        let (registry, sessions) = make_registry_and_sessions();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let mut req = base_request(AgentRole::Child, Some(root.id));
        req.adapter_name = "nonexistent".to_string();
        let err = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), req)
            .unwrap_err();
        assert!(matches!(err, SpawnError::UnknownAdapter(name) if name == "nonexistent"));
    }

    #[test]
    fn spawn_agent_unknown_parent_errors() {
        let (registry, sessions) = make_registry_and_sessions();
        let err = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Child, Some(AgentId::new())),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Registry(RegistryError::UnknownParent(_))));
    }

    #[test]
    fn spawn_agent_rolls_back_registration_on_launch_failure() {
        let registry = AgentRegistry::new();
        let failing_sessions = SessionManager::dry_run_failing_launch();
        let err = spawn_agent(
            &registry,
            &failing_sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            base_request(AgentRole::Root, None),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Launch(_)));
        // The failed launch must not leave a phantom "Running" node behind.
        assert!(registry.snapshot().is_empty());
    }
}
