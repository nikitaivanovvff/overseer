use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::agent::adapters::{adapter_for, identity_env, AgentAdapter, AgentIdentity, LaunchContext};
use crate::agent::registry::{RegisterArgs, RegisterResult, RegistryError};
use crate::agent::{AgentId, AgentRegistry, AgentRole, AgentStatus};
use crate::config::{AdapterConfig, Config};
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
    /// Child-only tree-row label, distinct from `task`. Ignored for a root
    /// (still named after the repo — see `spawn_root`). Absent or
    /// blank falls back to `task` verbatim (`spawn_child_agent`).
    pub name: Option<String>,
    /// Child-only: which harness to launch (required, validated against
    /// `config.adapters`). Ignored for a root, which is always a bare shell.
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
    #[error("'{0}' adapter is not installed -- run `overseer install {0}` first")]
    AdapterNotInstalled(String),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("launch failed: {0}")]
    Launch(anyhow::Error),
}

/// Registers a new agent and launches its PTY session in one step.
/// The single shared orchestration entry point for root (`start`) and child (`spawn`)
/// launches — dispatches to a role-specific path since root is always a bare
/// shell, while a child is always adapter-driven.
pub fn spawn_agent(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    config: &Config,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    match req.role.clone() {
        AgentRole::Root => spawn_root(registry, sessions, socket, req),
        AgentRole::Child => spawn_child_agent(registry, sessions, socket, config, req, AgentStatus::Spawning),
    }
}

pub fn spawn_manual_child(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    config: &Config,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    spawn_child_agent(registry, sessions, socket, config, req, AgentStatus::Idle)
}

/// No `AgentAdapter`, no configured agent binary — just a plain shell in the
/// chosen repo, registered `Idle`. Whatever the user later runs inside (e.g.
/// `claude`) inherits the identity env vars via the PTY's normal
/// process-environment inheritance, and the existing PostToolUse/Stop hooks pick
/// it up from there — no new detection/polling code, this is pure push.
fn spawn_root(
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
        depth: 1,
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

/// Looks up `adapter_name`'s `AgentAdapter` impl + its resolved `config.adapters`
/// entry (command/extra_args) — the shared resolution `spawn_child_agent` needs
/// before it can launch. `is_installed()` guards against a launch that would
/// crash outright (HARNESSES.md).
fn resolve_adapter<'a>(
    config: &'a Config,
    adapter_name: &str,
) -> Result<(Box<dyn AgentAdapter>, &'a AdapterConfig), SpawnError> {
    let adapter = adapter_for(adapter_name)
        .ok_or_else(|| SpawnError::UnknownAdapter(adapter_name.to_string()))?;
    if !adapter.is_installed() {
        return Err(SpawnError::AdapterNotInstalled(adapter_name.to_string()));
    }
    let adapter_config = config
        .adapters
        .get(adapter_name)
        .ok_or_else(|| SpawnError::UnknownAdapter(adapter_name.to_string()))?;
    Ok((adapter, adapter_config))
}

/// The CLI flag each adapter's harness accepts to auto-approve permissions,
/// for `[defaults] im_not_afraid_of_agents` (README.md "Danger Zone"). `None`
/// for an adapter we don't have a live-verified flag for — never guess one.
fn auto_approve_flag(adapter_name: &str) -> Option<&'static str> {
    match adapter_name {
        "claude" => Some("--dangerously-skip-permissions"),
        "opencode" => Some("--auto"),
        _ => None,
    }
}

/// Appends `adapter_name`'s auto-approve flag to `extra_args` when
/// `im_not_afraid_of_agents` is enabled, unless it's already present. Toggle
/// off (the default) or an adapter with no known flag leaves `extra_args`
/// untouched.
fn apply_auto_approve_flag(
    adapter_name: &str,
    mut extra_args: Vec<String>,
    im_not_afraid_of_agents: bool,
) -> Vec<String> {
    if !im_not_afraid_of_agents {
        return extra_args;
    }
    if let Some(flag) = auto_approve_flag(adapter_name) {
        if !extra_args.iter().any(|a| a == flag) {
            extra_args.push(flag.to_string());
        }
    }
    extra_args
}

/// Child path — adapter-driven, registers `Running` since it auto-launches
/// immediately. `command`/`extra_args` come from the resolved config entry for
/// `req.adapter_name`, not the adapter name itself — a user can point "claude" at
/// a custom binary/wrapper. An adapter name absent from `config.adapters` is
/// `UnknownAdapter`, the same error a name with no `AgentAdapter` impl gets.
fn spawn_child_agent(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    config: &Config,
    req: SpawnRequest,
    initial_status: AgentStatus,
) -> Result<RegisterResult, SpawnError> {
    let (adapter, adapter_config) = resolve_adapter(config, &req.adapter_name)?;

    let name = req
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| req.task.clone());
    let args = RegisterArgs {
        id: None,
        name,
        role: AgentRole::Child,
        parent_id: req.parent_id.clone(),
        adapter: req.adapter_name.clone(),
        repo: req.repo.clone(),
        cwd: req.cwd.clone(),
        branch: req.branch,
        initial_status,
    };
    let result = registry.register(args)?;
    let depth = registry
        .with_tree(|tree| tree.depth(&result.id))
        .expect("newly registered child must be in tree");

    let launch_ctx = LaunchContext {
        agent_id: result.id.clone(),
        role: AgentRole::Child,
        parent_id: req.parent_id,
        socket: socket.to_path_buf(),
        cwd: req.cwd,
        repo: req.repo,
        command: adapter_config.command.clone(),
        extra_args: apply_auto_approve_flag(
            &req.adapter_name,
            adapter_config.extra_args.clone(),
            config.defaults.im_not_afraid_of_agents,
        ),
        task: req.task,
        depth,
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
    use crate::config::Config;
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
            task: String::new(),
            depth: 1,
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
            name: None,
            adapter_name: "claude".to_string(),
            cwd: PathBuf::from("/tmp"),
            repo: "overseer".to_string(),
            branch: None,
        }
    }

    #[test]
    fn spawn_agent_root_succeeds() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let result = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();
        assert_eq!(result.branch, "main");
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn spawn_agent_root_registers_idle_with_shell_adapter_label() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let result = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
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
        let config = Config::default();
        let mut req = base_request(AgentRole::Root, None);
        req.task = "this text should never become the name".to_string();
        req.repo = "distinct-repo-name".to_string();
        let result = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap();
        let dto = registry.get(&result.id).unwrap();
        assert_eq!(dto.name, "distinct-repo-name");
    }

    #[test]
    fn spawn_agent_root_ignores_a_supplied_name_too() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let mut req = base_request(AgentRole::Root, None);
        req.name = Some("should-be-ignored".to_string());
        req.repo = "distinct-repo-name".to_string();
        let result = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap();
        let dto = registry.get(&result.id).unwrap();
        assert_eq!(dto.name, "distinct-repo-name");
    }

    #[test]
    fn spawn_agent_child_with_name_registers_that_name_not_the_task() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let mut req = base_request(AgentRole::Child, Some(root.id));
        req.task = "write unit tests for the login flow".to_string();
        req.name = Some("login-tests".to_string());
        let child = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap();
        let dto = registry.get(&child.id).unwrap();
        assert_eq!(dto.name, "login-tests");
    }

    #[test]
    fn spawn_agent_child_blank_name_falls_back_to_task() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let mut req = base_request(AgentRole::Child, Some(root.id));
        req.task = "fallback task text".to_string();
        req.name = Some("   ".to_string());
        let child = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap();
        let dto = registry.get(&child.id).unwrap();
        assert_eq!(dto.name, "fallback task text");
    }

    #[test]
    fn spawn_agent_child_absent_name_falls_back_to_task() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        // base_request already sets name: None — this documents that as the
        // default fallback path, not just an implementation detail of the helper.
        let child = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Child, Some(root.id)),
        )
        .unwrap();
        let dto = registry.get(&child.id).unwrap();
        assert_eq!(dto.name, "do stuff"); // base_request's task text
    }

    #[test]
    fn resolve_shell_uses_env_var() {
        let _env = crate::test_env::EnvGuard::set("SHELL", "/bin/zsh");
        assert_eq!(resolve_shell(), "/bin/zsh");
    }

    #[test]
    fn resolve_shell_falls_back_to_bin_sh() {
        let _env = crate::test_env::EnvGuard::unset("SHELL");
        assert_eq!(resolve_shell(), "/bin/sh");
    }

    #[test]
    fn spawn_agent_child_succeeds_under_known_parent() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let child = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Child, Some(root.id)),
        )
        .unwrap();
        // Self-reported later by the child's own hook/plugin, never
        // synthesized at registration (see `Request::Status.branch`).
        assert_eq!(child.branch, "");
        assert_eq!(registry.snapshot().len(), 2);
    }

    #[test]
    fn spawn_agent_unknown_adapter_errors() {
        // Root no longer validates any adapter (it's always a bare shell), so this
        // now has to go through a child spawn — the only path that still consults one.
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let mut req = base_request(AgentRole::Child, Some(root.id));
        req.adapter_name = "nonexistent".to_string();
        let err = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap_err();
        assert!(matches!(err, SpawnError::UnknownAdapter(name) if name == "nonexistent"));
    }

    #[test]
    fn adapter_not_installed_error_message_points_at_the_install_command() {
        // Not run through the real adapter lookup (whether "opencode"
        // report is_installed() == true/false depends on this machine's own
        // ~/.config/opencode state -- non-deterministic across
        // environments). Just pins the error message shape a caller
        // (a root agent's own tool output) would actually see.
        let err = SpawnError::AdapterNotInstalled("opencode".to_string());
        assert_eq!(err.to_string(), "'opencode' adapter is not installed -- run `overseer install opencode` first");
    }

    #[test]
    fn spawn_agent_child_errors_when_adapter_missing_from_config() {
        // "claude" has an AgentAdapter impl, but if it's absent from the config's
        // adapters map, that's the *other* UnknownAdapter path (Task 3) — same
        // error variant, different lookup.
        let (registry, sessions) = make_registry_and_sessions();
        let empty_config =
            Config {
                defaults: Default::default(),
                adapters: Default::default(),
                notify: Default::default(),
                keybindings: Default::default(),
                theme: Default::default(),
            };
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &empty_config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        let req = base_request(AgentRole::Child, Some(root.id));
        let err = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &empty_config, req)
            .unwrap_err();
        assert!(matches!(err, SpawnError::UnknownAdapter(name) if name == "claude"));
    }

    #[test]
    fn spawn_agent_child_uses_configured_command_and_extra_args() {
        let (registry, sessions) = make_registry_and_sessions();
        let mut config = Config::default();
        config.adapters.insert(
            "claude".to_string(),
            crate::config::AdapterConfig {
                command: "/usr/local/bin/claude-wrapper".to_string(),
                extra_args: vec!["--dangerously-skip-permissions".to_string()],
            },
        );
        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        // spawn_child_agent's launch goes through a dry-run SessionManager, so we
        // can't observe the built Command directly here — this test exercises the
        // resolution path end-to-end (no error means the config lookup + launch
        // succeeded); ClaudeAdapter's own tests cover the resulting Command shape.
        let child = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Child, Some(root.id)),
        )
        .unwrap();
        assert!(registry.get(&child.id).is_some());
    }

    #[test]
    fn apply_auto_approve_flag_toggle_off_returns_extra_args_unchanged() {
        let result = apply_auto_approve_flag("claude", vec!["--foo".to_string()], false);
        assert_eq!(result, vec!["--foo".to_string()]);
    }

    #[test]
    fn apply_auto_approve_flag_appends_claude_flag_when_enabled() {
        let result = apply_auto_approve_flag("claude", vec![], true);
        assert_eq!(result, vec!["--dangerously-skip-permissions".to_string()]);
    }

    #[test]
    fn apply_auto_approve_flag_appends_opencode_flag_when_enabled() {
        let result = apply_auto_approve_flag("opencode", vec![], true);
        assert_eq!(result, vec!["--auto".to_string()]);
    }

    #[test]
    fn apply_auto_approve_flag_does_not_duplicate_an_already_present_flag() {
        let result = apply_auto_approve_flag(
            "claude",
            vec!["--dangerously-skip-permissions".to_string()],
            true,
        );
        assert_eq!(result, vec!["--dangerously-skip-permissions".to_string()]);
    }

    #[test]
    fn apply_auto_approve_flag_unknown_adapter_returns_extra_args_unchanged() {
        let result = apply_auto_approve_flag("aider", vec!["--foo".to_string()], true);
        assert_eq!(result, vec!["--foo".to_string()]);
    }

    #[test]
    fn spawn_agent_child_extra_args_include_auto_approve_flag_when_toggle_is_on() {
        let (registry, sessions) = make_registry_and_sessions();
        let mut config = Config::default();
        config.defaults.im_not_afraid_of_agents = true;

        let root = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap();

        // Same limitation as spawn_agent_child_uses_configured_command_and_extra_args
        // above: spawn_child_agent's launch goes through a dry-run SessionManager,
        // so the built LaunchContext/Command isn't directly observable here. This
        // proves the resolution path succeeds with the toggle on, then asserts the
        // exact value apply_auto_approve_flag computes for this config -- the same
        // call spawn_child_agent makes to build LaunchContext.extra_args at its one
        // production call site.
        let child = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Child, Some(root.id)),
        )
        .unwrap();
        assert!(registry.get(&child.id).is_some());

        let adapter_config = config.adapters.get("claude").unwrap();
        let resolved_extra_args = apply_auto_approve_flag(
            "claude",
            adapter_config.extra_args.clone(),
            config.defaults.im_not_afraid_of_agents,
        );
        assert_eq!(resolved_extra_args, vec!["--dangerously-skip-permissions".to_string()]);
    }

    #[test]
    fn spawn_agent_unknown_parent_errors() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let err = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Child, Some(AgentId::new())),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Registry(RegistryError::UnknownParent(_))));
    }

    #[test]
    fn spawn_agent_rolls_back_registration_on_launch_failure() {
        let registry = AgentRegistry::new();
        let failing_sessions = SessionManager::dry_run_failing_launch();
        let config = Config::default();
        let err = spawn_agent(
            &registry,
            &failing_sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            base_request(AgentRole::Root, None),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Launch(_)));
        // The failed launch must not leave a phantom "Running" node behind.
        assert!(registry.snapshot().is_empty());
    }

}
