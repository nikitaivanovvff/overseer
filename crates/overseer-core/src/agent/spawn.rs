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
    /// `config.adapters`). Ignored for a root — see `root_adapter` instead.
    pub adapter_name: String,
    pub cwd: PathBuf,
    pub repo: String,
    /// Explicit branch override (used by `start --branch`-style callers). `None` lets
    /// the registry apply its default ("main" for root, "overseer/<id>" for child).
    pub branch: Option<String>,
    /// Root-only: `Some(name)` launches that adapter directly (empty task,
    /// same launch path a child uses) instead of a bare shell — the TUI's
    /// two-step `n` picker's second field. `None`/blank preserves the
    /// bare-shell default. Ignored for a child (which always uses
    /// `adapter_name` instead).
    pub root_adapter: Option<String>,
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
/// launches — dispatches to a role-specific path since root additionally branches
/// on whether an adapter was chosen (bare shell vs. adapter-driven), while a
/// child is always adapter-driven.
pub fn spawn_agent(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    config: &Config,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    match req.role.clone() {
        AgentRole::Root => spawn_root(registry, sessions, socket, config, req),
        AgentRole::Child => spawn_child_agent(registry, sessions, socket, config, req),
    }
}

/// Root path: bare shell by default, or — if `req.root_adapter` names one —
/// that adapter launched directly instead (the TUI's two-step `n` picker's
/// second step, once at least one adapter is `overseer_installed()`).
fn spawn_root(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    config: &Config,
    req: SpawnRequest,
) -> Result<RegisterResult, SpawnError> {
    match req.root_adapter.clone().filter(|n| !n.trim().is_empty()) {
        Some(adapter_name) => spawn_root_with_adapter(registry, sessions, socket, config, req, adapter_name),
        None => spawn_root_bare_shell(registry, sessions, socket, req),
    }
}

/// No `AgentAdapter`, no configured agent binary — just a plain shell in the
/// chosen repo, registered `Idle`. Whatever the user later runs inside (e.g.
/// `claude`) inherits the identity env vars via the PTY's normal
/// process-environment inheritance, and the existing PostToolUse/Stop hooks pick
/// it up from there — no new detection/polling code, this is pure push.
fn spawn_root_bare_shell(
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

/// Root path when an adapter was actually chosen (picker step 2, or a
/// hand-built `Request::Start { adapter: Some(_), .. }`) — launched like the
/// bare-shell root (a live, interactive shell as the PTY child), with the
/// adapter's own launch command auto-typed into it. Registers `Idle`, same
/// as the bare-shell root: the PTY child is a shell, byte-identical to a
/// human typing the command into a bare-shell root themselves, and the
/// harness's own `SessionStart`-equivalent hook flips status from there
/// (every adapter's install hook already branches on `$OVERSEER_ROLE` to
/// push `idle` for a root regardless of who typed the launch command).
///
/// Exec'ing the harness binary directly as the PTY child (the previous
/// approach) meant exiting the harness killed the whole PTY — the exit
/// watcher then flipped the workspace straight to `done`/`error` and the
/// pane froze on the harness's last frame. Going through a shell first means
/// exiting the harness drops back to a live shell prompt (workspace stays
/// up), and only exiting *that* shell ends the workspace — exactly how a
/// bare-shell root already behaves. Side benefit: the pane shows a live
/// shell prompt and the typed command within milliseconds instead of being
/// blank for the harness's entire boot time.
fn spawn_root_with_adapter(
    registry: &AgentRegistry,
    sessions: &SessionManager,
    socket: &Path,
    config: &Config,
    req: SpawnRequest,
    adapter_name: String,
) -> Result<RegisterResult, SpawnError> {
    let (adapter, adapter_config) = resolve_adapter(config, &adapter_name)?;

    let args = RegisterArgs {
        id: None,
        name: req.repo.clone(), // still named after the repo, same as the bare-shell root
        role: AgentRole::Root,
        parent_id: None,
        adapter: adapter_name, // label = the chosen adapter, not "shell"
        repo: req.repo.clone(),
        cwd: req.cwd.clone(),
        branch: req.branch,
        initial_status: AgentStatus::Idle,
    };
    let result = registry.register(args)?;

    let launch_ctx = LaunchContext {
        agent_id: result.id.clone(),
        role: AgentRole::Root,
        parent_id: None,
        socket: socket.to_path_buf(),
        cwd: req.cwd.clone(),
        repo: req.repo,
        command: adapter_config.command.clone(),
        extra_args: adapter_config.extra_args.clone(),
        task: String::new(), // a root has no task, same as the bare-shell path
    };
    // The harness invocation to type in, not to exec directly — see the
    // doc comment above for why exec'ing it here would recreate the bug.
    let harness_cmd = adapter.spawn_command(&launch_ctx);
    let env = adapter.env_inject(&launch_ctx);

    let shell_cmd = Command::new(resolve_shell());
    if let Err(e) = sessions.launch(result.id.clone(), &req.cwd, &shell_cmd, &env) {
        registry.remove(&result.id);
        return Err(SpawnError::Launch(e));
    }

    let command_line = format!("{}\n", shell_command_line(&harness_cmd));
    sessions.write(&result.id, command_line.into_bytes());

    Ok(result)
}

/// POSIX single-quote-quotes `cmd`'s program and each argument, joined by
/// spaces — safe to type verbatim into an interactive shell. Single-quoting
/// (rather than double-quoting or escaping) is the simplest form that's
/// correct for arbitrary bytes: the only special case is an embedded `'`,
/// which becomes `'\''` (close the quote, an escaped literal quote, reopen
/// the quote). Needed because some adapters' args are absolute paths that
/// may contain spaces (pi's `--extension <path>`, `--append-system-prompt
/// <path>`).
fn shell_command_line(cmd: &Command) -> String {
    let mut parts = Vec::new();
    parts.push(shell_quote(&cmd.get_program().to_string_lossy()));
    for arg in cmd.get_args() {
        parts.push(shell_quote(&arg.to_string_lossy()));
    }
    parts.join(" ")
}

/// Single-quotes `s` for a POSIX shell, escaping any embedded `'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Looks up `adapter_name`'s `AgentAdapter` impl + its resolved `config.adapters`
/// entry (command/extra_args) — the shared resolution both `spawn_child_agent`
/// and `spawn_root_with_adapter` need before they can launch. `is_installed()`
/// (not `overseer_installed()`) is the check here: this guards against a launch
/// that would crash outright (HARNESSES.md), not against "did the user ever run
/// `overseer install`" — the TUI's own picker already filters on the latter
/// before this is ever reached from that path.
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
        // Not Running: the PTY is launching but the agent hasn't reported activity
        // yet. SessionStart flips it to Running moments later (Claude adapter hook).
        initial_status: AgentStatus::Spawning,
    };
    let result = registry.register(args)?;

    let launch_ctx = LaunchContext {
        agent_id: result.id.clone(),
        role: AgentRole::Child,
        parent_id: req.parent_id,
        socket: socket.to_path_buf(),
        cwd: req.cwd,
        repo: req.repo,
        command: adapter_config.command.clone(),
        extra_args: adapter_config.extra_args.clone(),
        task: req.task,
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
            root_adapter: None,
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
        assert!(child.branch.starts_with("overseer/"));
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
        // Not run through the real adapter lookup (whether "pi"/"opencode"
        // report is_installed() == true/false depends on this machine's own
        // ~/.pi, ~/.config/opencode state -- non-deterministic across
        // environments). Just pins the error message shape a caller
        // (a root agent's own tool output) would actually see.
        let err = SpawnError::AdapterNotInstalled("pi".to_string());
        assert_eq!(err.to_string(), "'pi' adapter is not installed -- run `overseer install pi` first");
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

    // ── root-adapter launch (the `n` picker's second step) ───────────────────

    fn root_request_with_adapter(adapter: &str) -> SpawnRequest {
        let mut req = base_request(AgentRole::Root, None);
        req.root_adapter = Some(adapter.to_string());
        req
    }

    #[test]
    fn spawn_agent_root_with_adapter_registers_idle_with_that_adapter_label() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let result = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            root_request_with_adapter("claude"),
        )
        .unwrap();
        let dto = registry.get(&result.id).unwrap();
        // Adapter label is the chosen adapter, not "shell" — but status is
        // Idle, same as a bare-shell root: the PTY child launched here is a
        // live shell (the harness command is only typed into it), so this
        // root is in exactly the same "waiting, nothing has reported
        // activity yet" state a bare-shell root starts in.
        assert_eq!(dto.adapter, "claude");
        assert_eq!(dto.status, AgentStatus::Idle);
    }

    #[test]
    fn spawn_agent_root_with_adapter_still_names_node_after_repo_not_task() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let mut req = root_request_with_adapter("claude");
        req.task = "this text should never become the name".to_string();
        req.repo = "distinct-repo-name".to_string();
        let result = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap();
        let dto = registry.get(&result.id).unwrap();
        assert_eq!(dto.name, "distinct-repo-name");
    }

    #[test]
    fn spawn_agent_root_with_blank_adapter_falls_back_to_bare_shell() {
        // An empty/whitespace-only `root_adapter` means "no adapter was
        // actually chosen" — same as `None`, not "launch an adapter named ''".
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let mut req = base_request(AgentRole::Root, None);
        req.root_adapter = Some("   ".to_string());
        let result = spawn_agent(&registry, &sessions, &PathBuf::from("/tmp/overseer.sock"), &config, req)
            .unwrap();
        let dto = registry.get(&result.id).unwrap();
        assert_eq!(dto.adapter, "shell");
        assert_eq!(dto.status, AgentStatus::Idle);
    }

    #[test]
    fn spawn_agent_root_with_unknown_adapter_errors_and_registers_nothing() {
        let (registry, sessions) = make_registry_and_sessions();
        let config = Config::default();
        let err = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            root_request_with_adapter("nonexistent"),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::UnknownAdapter(name) if name == "nonexistent"));
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn spawn_agent_root_with_adapter_rolls_back_registration_on_launch_failure() {
        let registry = AgentRegistry::new();
        let failing_sessions = SessionManager::dry_run_failing_launch();
        let config = Config::default();
        let err = spawn_agent(
            &registry,
            &failing_sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            root_request_with_adapter("claude"),
        )
        .unwrap_err();
        assert!(matches!(err, SpawnError::Launch(_)));
        assert!(registry.snapshot().is_empty());
    }

    /// The PTY child launched is a shell (not the harness binary), and the
    /// harness invocation gets typed into it as a follow-up write, ending in
    /// a newline to submit it — this is what actually fixes ROOT-SHELL-FALLBACK:
    /// exiting the harness now drops to a live shell instead of killing the PTY.
    #[test]
    fn spawn_agent_root_with_adapter_launches_a_shell_and_types_the_harness_command() {
        let _env = crate::test_env::EnvGuard::set("SHELL", "/bin/zsh");
        let (registry, sessions) = make_registry_and_sessions();
        let mut config = Config::default();
        config.adapters.insert(
            "claude".to_string(),
            crate::config::AdapterConfig {
                command: "claude".to_string(),
                extra_args: vec!["--dangerously-skip-permissions".to_string()],
            },
        );
        let result = spawn_agent(
            &registry,
            &sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &config,
            root_request_with_adapter("claude"),
        )
        .unwrap();

        let typed = String::from_utf8(sessions.dry_run_written_bytes(&result.id)).unwrap();
        assert_eq!(typed, "'claude' '--dangerously-skip-permissions'\n");
    }

    // ── shell_command_line ────────────────────────────────────────────────────

    #[test]
    fn shell_command_line_quotes_plain_program_and_args() {
        let mut cmd = Command::new("claude");
        cmd.arg("--dangerously-skip-permissions");
        assert_eq!(shell_command_line(&cmd), "'claude' '--dangerously-skip-permissions'");
    }

    #[test]
    fn shell_command_line_quotes_args_containing_spaces() {
        let mut cmd = Command::new("pi");
        cmd.arg("--append-system-prompt").arg("/Users/me/My Documents/overseer-root.md");
        assert_eq!(
            shell_command_line(&cmd),
            "'pi' '--append-system-prompt' '/Users/me/My Documents/overseer-root.md'"
        );
    }

    #[test]
    fn shell_command_line_escapes_embedded_single_quotes() {
        let mut cmd = Command::new("claude");
        cmd.arg("it's a task");
        assert_eq!(shell_command_line(&cmd), r"'claude' 'it'\''s a task'");
    }
}
