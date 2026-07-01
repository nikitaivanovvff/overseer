use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, sync::Arc, time::Duration};

mod agent;
mod app;
mod git;
mod ipc;
mod session;
mod settings;
mod ui;

use agent::{AgentId, AgentRegistry, AgentRole, AgentStatus, AgentTree};
use agent::adapters::{adapter_for, MergeStrategy};
use agent::drop::drop_agent;
use app::{App, ConfirmState, Focus, InputState, PendingAction};
use git::GitClient;
use ipc::handlers::dispatch;
use ipc::protocol::Request;
use ipc::AppCtx;
use session::TmuxClient;

#[derive(clap::Parser)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[arg(long)]
    mock: bool,
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    Register {
        #[arg(long)]
        role: RoleArg,
        #[arg(long)]
        name: String,
        #[arg(long)]
        parent_id: Option<String>,
        #[arg(long, default_value = "claude")]
        adapter: String,
        #[arg(long, default_value = "overseer")]
        repo: String,
    },
    /// Push a status update. Agent identity comes from $OVERSEER_AGENT_ID.
    /// When $OVERSEER_AGENT_ID is unset (non-Overseer session), exits 0 silently.
    Status {
        status: StatusArg,
        #[arg(long)]
        message: Option<String>,
    },
    List,
    Agent {
        id: String,
    },
    /// Install the adapter skill + hooks at the user level (runs once, no socket needed).
    Teach {
        /// Adapter name to teach (e.g. "claude").
        agent: String,
        /// Remove only the Overseer-managed entries instead of installing them.
        #[arg(long)]
        uninstall: bool,
    },
    /// Start a root agent in a new tmux session (server-side launch via the running TUI).
    Start {
        /// Task description — becomes the agent name in the TUI.
        #[arg(long)]
        task: String,
        /// Adapter to use (default: claude).
        #[arg(long, default_value = "claude")]
        adapter: String,
        /// Repo root to start in (default: current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Request a child agent. Caller identity comes from $OVERSEER_AGENT_ID — rejected
    /// if the caller is itself a child (flat tree: roots + children only).
    Spawn {
        /// Task description — becomes the child's name in the TUI.
        #[arg(long)]
        task: String,
        /// Adapter to use (default: claude).
        #[arg(long, default_value = "claude")]
        adapter: String,
    },
    /// Kill the agent's tmux session and deregister it. Root agents can only be
    /// dropped through the TUI, not this command.
    Drop {
        /// Agent id to drop.
        id: String,
        /// Also drop all of the agent's children (children before parent).
        #[arg(long)]
        recursive: bool,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum RoleArg {
    Root,
    Child,
}

#[derive(Clone, clap::ValueEnum)]
enum StatusArg {
    Spawning,
    Running,
    Waiting,
    Done,
    Error,
}

impl From<RoleArg> for AgentRole {
    fn from(r: RoleArg) -> Self {
        match r {
            RoleArg::Root => AgentRole::Root,
            RoleArg::Child => AgentRole::Child,
        }
    }
}

impl From<StatusArg> for AgentStatus {
    fn from(s: StatusArg) -> Self {
        match s {
            StatusArg::Spawning => AgentStatus::Spawning,
            StatusArg::Running => AgentStatus::Running,
            StatusArg::Waiting => AgentStatus::Waiting,
            StatusArg::Done => AgentStatus::Done,
            StatusArg::Error => AgentStatus::Error,
        }
    }
}

fn resolve_socket(cli_socket: Option<PathBuf>) -> PathBuf {
    cli_socket
        .or_else(|| std::env::var("OVERSEER_SOCKET").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp/overseer.sock"))
}

fn main() -> Result<()> {
    use clap::Parser;
    let cli = Cli::parse();
    let socket = resolve_socket(cli.socket);

    match cli.cmd {
        None => run_tui(socket, cli.mock),
        Some(Command::Teach { agent, uninstall }) => run_teach(&agent, uninstall),
        Some(cmd) => run_client(socket, cmd),
    }
}

/// `--mock` is inert demo data: it must never launch a real tmux session.
fn tmux_client_for(mock: bool) -> TmuxClient {
    if mock { TmuxClient::dry_run() } else { TmuxClient::new() }
}

fn run_tui(socket: PathBuf, mock: bool) -> Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_panic(info);
    }));

    let registry = Arc::new(if mock {
        AgentRegistry::from_tree(AgentTree::with_mock_data())
    } else {
        AgentRegistry::new()
    });

    let ctx = Arc::new(AppCtx {
        registry,
        tmux: Arc::new(tmux_client_for(mock)),
        socket: socket.clone(),
        git: Arc::new(GitClient::new()),
        watch_sessions: !mock,
    });

    let socket_clone = socket.clone();
    let ipc_ctx = ctx.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        if let Err(e) = ipc::serve_blocking(ipc_ctx, socket_clone, Some(ready_tx)) {
            eprintln!("IPC server error: {e}");
        }
    });
    ready_rx.recv().ok();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(ctx);
    let res = run_app(&mut terminal, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
    let _ = terminal.show_cursor();
    let _ = std::fs::remove_file(&socket);

    res
}

// ── teach ─────────────────────────────────────────────────────────────────────

fn run_teach(agent_name: &str, uninstall: bool) -> Result<()> {
    let adapter = adapter_for(agent_name)
        .ok_or_else(|| anyhow::anyhow!("unknown adapter: '{agent_name}'"))?;

    let config_dir = adapter
        .user_config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve user config dir for '{agent_name}'"))?;

    if uninstall {
        for file in adapter.teach_files() {
            let full_path = config_dir.join(&file.path);
            match file.merge {
                MergeStrategy::Overwrite => {
                    if full_path.exists() {
                        std::fs::remove_file(&full_path)
                            .with_context(|| format!("failed to remove {}", full_path.display()))?;
                        println!("removed  {}", full_path.display());
                    }
                }
                MergeStrategy::JsonMerge => {
                    if full_path.exists() {
                        let raw = std::fs::read_to_string(&full_path)
                            .with_context(|| format!("failed to read {}", full_path.display()))?;
                        let mut json: serde_json::Value = serde_json::from_str(&raw)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        settings::remove_hooks(&mut json);
                        let out = serde_json::to_string_pretty(&json)?;
                        std::fs::write(&full_path, out + "\n")
                            .with_context(|| format!("failed to write {}", full_path.display()))?;
                        println!("updated  {} (removed overseer hooks)", full_path.display());
                    }
                }
            }
        }
        println!("uninstalled '{agent_name}' adapter");
    } else {
        for file in adapter.teach_files() {
            let full_path = config_dir.join(&file.path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            match file.merge {
                MergeStrategy::Overwrite => {
                    std::fs::write(&full_path, &file.content)
                        .with_context(|| format!("failed to write {}", full_path.display()))?;
                    println!("wrote    {}", full_path.display());
                }
                MergeStrategy::JsonMerge => {
                    let existing_raw = if full_path.exists() {
                        std::fs::read_to_string(&full_path)
                            .with_context(|| format!("failed to read {}", full_path.display()))?
                    } else {
                        "{}".to_string()
                    };
                    let mut existing: serde_json::Value =
                        serde_json::from_str(&existing_raw).unwrap_or_else(|_| serde_json::json!({}));
                    let overlay: serde_json::Value =
                        serde_json::from_str(&file.content).context("adapter returned invalid JSON")?;
                    settings::merge_hooks(&mut existing, &overlay);
                    let out = serde_json::to_string_pretty(&existing)?;
                    std::fs::write(&full_path, out + "\n")
                        .with_context(|| format!("failed to write {}", full_path.display()))?;
                    println!("merged   {}", full_path.display());
                }
            }
        }
        println!("installed '{agent_name}' adapter → config dir: {}", config_dir.display());
    }

    Ok(())
}

// ── client ────────────────────────────────────────────────────────────────────

fn run_client(socket: PathBuf, cmd: Command) -> Result<()> {
    let req = match build_request(cmd)? {
        Some(r) => r,
        None => return Ok(()), // silent no-op (Status outside an Overseer session)
    };

    let resp = match ipc::client::send(&socket, &req) {
        Ok(r) => r,
        // Status is hook-invoked: if the socket is unreachable, exit silently.
        Err(_) if matches!(req, Request::Status { .. }) => return Ok(()),
        Err(e) => return Err(e),
    };

    if resp.ok {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        Ok(())
    } else {
        let error = resp.error.unwrap_or_else(|| "unknown error".to_string());
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Returns `Ok(None)` for the Status command when `$OVERSEER_AGENT_ID` is unset,
/// indicating a non-Overseer session where the hook should be a silent no-op.
fn build_request(cmd: Command) -> Result<Option<Request>> {
    match cmd {
        Command::Register { role, name, parent_id, adapter, repo } => {
            let parent_id = match parent_id {
                Some(s) => Some(
                    s.parse::<AgentId>()
                        .map_err(|e| anyhow::anyhow!("invalid --parent-id: {e}"))?,
                ),
                None => None,
            };
            Ok(Some(Request::Register {
                id: None,
                name,
                role: role.into(),
                parent_id,
                adapter: Some(adapter),
                repo: Some(repo),
            }))
        }
        Command::Status { status, message } => {
            let agent_id_str = match std::env::var("OVERSEER_AGENT_ID") {
                Ok(s) => s,
                // Not in an Overseer session — hook must be a silent no-op.
                Err(_) => return Ok(None),
            };
            let agent_id = agent_id_str
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid $OVERSEER_AGENT_ID: {e}"))?;
            Ok(Some(Request::Status { agent_id, status: status.into(), message }))
        }
        Command::List => Ok(Some(Request::List)),
        Command::Agent { id } => {
            let agent_id = id
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            Ok(Some(Request::Agent { agent_id }))
        }
        Command::Start { task, adapter, cwd } => Ok(Some(Request::Start {
            task,
            adapter: Some(adapter),
            cwd,
        })),
        Command::Spawn { task, adapter } => {
            let parent_id_str = std::env::var("OVERSEER_AGENT_ID").map_err(|_| {
                anyhow::anyhow!("overseer spawn must be run from an agent session (missing $OVERSEER_AGENT_ID)")
            })?;
            let parent_id = parent_id_str
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid $OVERSEER_AGENT_ID: {e}"))?;
            let cwd = std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("failed to resolve current directory: {e}"))?;
            Ok(Some(Request::Spawn { parent_id, task, adapter: Some(adapter), cwd }))
        }
        Command::Drop { id, recursive } => {
            let agent_id = id
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            Ok(Some(Request::Drop { agent_id, recursive }))
        }
        Command::Teach { .. } => unreachable!("Teach is handled before run_client"),
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        let tick = app.tick;
        let focus = app.focus.clone();
        let prompt = build_prompt(app);
        app.ctx.registry.with_tree(|tree| {
            terminal.draw(|f| ui::render(f, &focus, tree, tick, prompt.as_deref()))
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if app.input.is_some() {
                    handle_input_key(app, key);
                } else if app.confirm.is_some() {
                    handle_confirm_key(app, key);
                } else {
                    match key.code {
                        KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => break,
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => break,
                        _ => {}
                    }

                    match app.focus {
                        Focus::Tree => match key.code {
                            KeyCode::Char('j') | KeyCode::Down
                                if key.modifiers == KeyModifiers::NONE =>
                            {
                                app.ctx.registry.with_tree_mut(|t| t.move_down());
                            }
                            KeyCode::Char('k') | KeyCode::Up
                                if key.modifiers == KeyModifiers::NONE =>
                            {
                                app.ctx.registry.with_tree_mut(|t| t.move_up());
                            }
                            KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                                app.ctx.registry.with_tree_mut(|t| t.toggle_expand());
                            }
                            KeyCode::Enter | KeyCode::Char('o')
                                if key.modifiers == KeyModifiers::NONE =>
                            {
                                app.focus = Focus::Pane;
                            }
                            KeyCode::Char('n') if key.modifiers == KeyModifiers::NONE => {
                                app.status_message = None;
                                app.input = Some(InputState {
                                    action: PendingAction::SpawnRoot,
                                    buffer: String::new(),
                                });
                            }
                            KeyCode::Char('s') if key.modifiers == KeyModifiers::NONE => {
                                start_spawn_child_input(app);
                            }
                            KeyCode::Char('d') if key.modifiers == KeyModifiers::NONE => {
                                start_drop_confirm(app, false);
                            }
                            KeyCode::Char('D') => start_drop_confirm(app, true),
                            _ => {}
                        },
                        Focus::Pane => {
                            if key.code == KeyCode::Esc {
                                app.focus = Focus::Tree;
                            }
                        }
                    }
                }
            }
        }

        app.tick();
    }
    Ok(())
}

/// Builds the status-bar override text for the active input/confirm prompt, or the
/// last status message. `None` means the status bar should show its normal hints.
fn build_prompt(app: &App) -> Option<String> {
    if let Some(input) = &app.input {
        let label = match input.action {
            PendingAction::SpawnRoot => "spawn root",
            PendingAction::SpawnChild { .. } => "spawn child",
        };
        return Some(format!("{label} task: {}_", input.buffer));
    }

    if let Some(confirm) = &app.confirm {
        let name = app
            .ctx
            .registry
            .get(&confirm.agent_id)
            .map(|d| d.name)
            .unwrap_or_else(|| "?".to_string());
        let descendants = app
            .ctx
            .registry
            .with_tree(|t| t.subtree_ids_postorder(&confirm.agent_id))
            .map(|ids| ids.len().saturating_sub(1))
            .unwrap_or(0);
        let suffix = if confirm.recursive && descendants > 0 {
            format!(" + {descendants} children")
        } else {
            String::new()
        };
        return Some(format!("drop '{name}'{suffix}? (y/n)"));
    }

    app.status_message.clone()
}

/// `s`: opens a task-input prompt for the selected node, whatever its role. Whether
/// it's actually eligible to take a child is decided by the server's `Spawn` handler
/// alone (AGENTS.md: the "no grandchildren" rule lives there, "not in the TUI, not as
/// a UI hint") — a rejection surfaces through `submit_input`'s normal error handling.
fn start_spawn_child_input(app: &mut App) {
    if let Some(node) = app.ctx.registry.with_tree(|t| t.selected()) {
        app.status_message = None;
        app.input = Some(InputState {
            action: PendingAction::SpawnChild { parent_id: node.id },
            buffer: String::new(),
        });
    }
}

fn start_drop_confirm(app: &mut App, recursive: bool) {
    if let Some(node) = app.ctx.registry.with_tree(|t| t.selected()) {
        app.status_message = None;
        app.confirm = Some(ConfirmState { agent_id: node.id, recursive });
    }
}

fn handle_input_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.input = None;
        }
        KeyCode::Backspace => {
            if let Some(input) = app.input.as_mut() {
                input.buffer.pop();
            }
        }
        KeyCode::Enter => {
            if let Some(input) = app.input.take() {
                submit_input(app, input);
            }
        }
        KeyCode::Char(c)
            if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
        {
            if let Some(input) = app.input.as_mut() {
                input.buffer.push(c);
            }
        }
        _ => {}
    }
}

/// Dispatches the composed `Start`/`Spawn` request in-process — same call the IPC
/// socket handler would make, so the grandchild check applies uniformly.
fn submit_input(app: &mut App, input: InputState) {
    let task = input.buffer.trim().to_string();
    if task.is_empty() {
        app.status_message = Some("spawn cancelled: empty task".to_string());
        return;
    }

    let req = match input.action {
        PendingAction::SpawnRoot => {
            let cwd = std::env::current_dir().ok();
            Request::Start { task, adapter: None, cwd }
        }
        PendingAction::SpawnChild { parent_id } => {
            let Some(parent) = app.ctx.registry.get(&parent_id) else {
                app.status_message = Some("spawn failed: parent no longer exists".to_string());
                return;
            };
            Request::Spawn { parent_id, task, adapter: None, cwd: parent.cwd }
        }
    };

    let resp = dispatch(&app.ctx, req);
    app.status_message = if resp.ok {
        None
    } else {
        Some(format!("spawn failed: {}", resp.error.unwrap_or_else(|| "unknown error".to_string())))
    };
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    let Some(confirm) = app.confirm.take() else { return };
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            match drop_agent(&app.ctx.registry, &app.ctx.tmux, &confirm.agent_id, confirm.recursive, true) {
                Ok(()) => app.status_message = None,
                Err(e) => app.status_message = Some(format!("drop failed: {e}")),
            }
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            app.status_message = None;
        }
        _ => {
            // Not a recognized response — keep the confirm prompt open.
            app.confirm = Some(confirm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;

    #[test]
    fn build_request_status_no_env_var_returns_none() {
        // Ensure $OVERSEER_AGENT_ID is unset for this test.
        std::env::remove_var("OVERSEER_AGENT_ID");
        let cmd = Command::Status { status: StatusArg::Running, message: None };
        let result = build_request(cmd).unwrap();
        assert!(result.is_none(), "Status without OVERSEER_AGENT_ID should be a silent no-op");
    }

    #[test]
    fn build_request_status_with_env_var_returns_request() {
        let id = AgentId::new();
        std::env::set_var("OVERSEER_AGENT_ID", id.0.to_string());
        let cmd = Command::Status { status: StatusArg::Done, message: None };
        let result = build_request(cmd).unwrap();
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Request::Status { .. }));
        std::env::remove_var("OVERSEER_AGENT_ID");
    }

    #[test]
    fn build_request_start_returns_start() {
        let cmd = Command::Start {
            task: "do stuff".to_string(),
            adapter: "claude".to_string(),
            cwd: None,
        };
        let req = build_request(cmd).unwrap().unwrap();
        assert!(matches!(req, Request::Start { task, .. } if task == "do stuff"));
    }

    #[test]
    fn build_request_list_returns_list() {
        let req = build_request(Command::List).unwrap().unwrap();
        assert!(matches!(req, Request::List));
    }

    #[test]
    fn build_request_spawn_without_env_var_is_error() {
        std::env::remove_var("OVERSEER_AGENT_ID");
        let cmd = Command::Spawn { task: "write tests".to_string(), adapter: "claude".to_string() };
        assert!(build_request(cmd).is_err());
    }

    #[test]
    fn build_request_spawn_with_env_var_returns_spawn() {
        let id = AgentId::new();
        std::env::set_var("OVERSEER_AGENT_ID", id.0.to_string());
        let cmd = Command::Spawn { task: "write tests".to_string(), adapter: "claude".to_string() };
        let req = build_request(cmd).unwrap().unwrap();
        assert!(matches!(req, Request::Spawn { parent_id, task, .. }
            if parent_id == id && task == "write tests"));
        std::env::remove_var("OVERSEER_AGENT_ID");
    }

    #[test]
    fn build_request_drop_returns_drop() {
        let id = AgentId::new();
        let cmd = Command::Drop { id: id.0.to_string(), recursive: true };
        let req = build_request(cmd).unwrap().unwrap();
        assert!(matches!(req, Request::Drop { agent_id, recursive: true } if agent_id == id));
    }

    #[test]
    fn build_request_drop_invalid_id_is_error() {
        let cmd = Command::Drop { id: "not-a-uuid".to_string(), recursive: false };
        assert!(build_request(cmd).is_err());
    }

    #[test]
    fn mock_mode_never_gets_a_real_tmux_client() {
        assert!(tmux_client_for(true).is_dry_run());
        assert!(!tmux_client_for(false).is_dry_run());
    }
}
