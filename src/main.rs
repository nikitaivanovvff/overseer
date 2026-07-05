use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::{
    io,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

mod agent;
mod app;
mod config;
mod git;
mod ipc;
mod session;
mod settings;
mod ui;

use agent::{AgentId, AgentRegistry, AgentRole, AgentStatus, AgentTree};
use agent::adapters::{adapter_for, MergeStrategy};
use agent::drop::drop_agent;
use app::{App, ConfirmAction, ConfirmState, Focus, InputState, PendingAction};
use config::Config;
use git::GitClient;
use ipc::handlers::dispatch;
use ipc::protocol::Request;
use ipc::AppCtx;
use session::SessionManager;

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
        /// Read the Claude Code hook payload JSON from stdin: classifies a
        /// `blocked` push as the idle nag vs. a real permission request, and
        /// (once a transcript is available) attaches context %. Never fails the
        /// hook — malformed/missing stdin just means less context on the push.
        #[arg(long)]
        from_hook: bool,
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
    /// Start a root: a bare shell in a repo (server-side launch via the running
    /// TUI), registered immediately and named after the repo. Run your own agent
    /// inside it whenever you're ready — Overseer picks up its status via the
    /// existing push hooks, no adapter is launched on your behalf.
    Start {
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
    /// Kill the agent's session and deregister it. Root agents can only be
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

/// Pushable statuses only — `Spawning` is set at registration time, never
/// pushed by a hook or agent.
#[derive(Clone, clap::ValueEnum)]
enum StatusArg {
    Running,
    Idle,
    Blocked,
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
            StatusArg::Running => AgentStatus::Running,
            StatusArg::Idle => AgentStatus::Idle,
            StatusArg::Blocked => AgentStatus::Blocked,
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

/// `--mock` is inert demo data: it must never spawn a real PTY.
fn session_manager_for(mock: bool) -> SessionManager {
    if mock { SessionManager::dry_run() } else { SessionManager::new() }
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
        sessions: Arc::new(session_manager_for(mock)),
        socket: socket.clone(),
        git: Arc::new(GitClient::new()),
        config: Arc::new(Config::load()),
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

    let mut app = App::new(ctx);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
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

/// Reads and parses the hook payload JSON from stdin. `None` on any I/O or parse
/// failure — `--from-hook` must never fail the hook over malformed stdin.
fn read_hook_payload() -> Option<agent::hook::HookPayload> {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    agent::hook::parse_hook_payload(&raw)
}

/// Pure. Only a `blocked` push needs classification — every other status
/// already means what it says. `Notification` fires for both a real permission
/// request and the ~60s idle nag; a missing/unparsed payload leaves `blocked`
/// as-is (the safer default — a permission prompt actually pending).
fn classify_hook_status(status: AgentStatus, payload: Option<&agent::hook::HookPayload>) -> AgentStatus {
    if status != AgentStatus::Blocked {
        return status;
    }
    match payload.and_then(|p| p.message.as_deref()) {
        Some(msg) if agent::hook::is_idle_nag(msg) => AgentStatus::Idle,
        _ => AgentStatus::Blocked,
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
        Command::Status { status, message, from_hook } => {
            let agent_id_str = match std::env::var("OVERSEER_AGENT_ID") {
                Ok(s) => s,
                // Not in an Overseer session — hook must be a silent no-op.
                Err(_) => return Ok(None),
            };
            let agent_id = agent_id_str
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid $OVERSEER_AGENT_ID: {e}"))?;

            let mut status: AgentStatus = status.into();
            if from_hook {
                status = classify_hook_status(status, read_hook_payload().as_ref());
            }

            Ok(Some(Request::Status { agent_id, status, message }))
        }
        Command::List => Ok(Some(Request::List)),
        Command::Agent { id } => {
            let agent_id = id
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            Ok(Some(Request::Agent { agent_id }))
        }
        Command::Start { cwd } => Ok(Some(Request::Start { cwd })),
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
    let mut last_pane_size: Option<(u16, u16)> = None;

    loop {
        let tick = app.tick;

        let prompt = build_prompt(app);
        let input = app.input.as_ref();
        let pane_focused = app.focus == Focus::Pane;
        let mut pane_rect = Rect::default();
        app.ctx.registry.with_tree(|tree| {
            terminal.draw(|f| {
                pane_rect =
                    ui::render(f, tree, tick, prompt.as_deref(), input, &app.ctx.sessions, pane_focused);
            })
        })?;
        // Every agent shares one PTY size — resize on layout
        // change (including the very first draw, sizing new sessions before
        // the user ever gets to spawn one).
        let pane_size = (pane_rect.width, pane_rect.height);
        if last_pane_size != Some(pane_size) {
            app.ctx.sessions.resize_all(pane_size.0 as usize, pane_size.1 as usize);
            last_pane_size = Some(pane_size);
        }

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                    match app.focus {
                        Focus::Pane => handle_pane_key(app, key),
                        Focus::Tree => {
                            if app.input.is_some() {
                                handle_input_key(app, key);
                            } else if app.confirm.is_some() {
                                if !handle_confirm_key(app, key) {
                                    break;
                                }
                            } else if !handle_tree_key(app, key) {
                                break;
                            }
                        }
                    }
                }
                Event::Paste(text) if app.focus == Focus::Pane => forward_paste(app, &text),
                _ => {}
            }
        }

        app.tick();
    }
    Ok(())
}

/// `Focus::Tree` key handling (nav, spawn, drop, jump-in, quit). Returns
/// `false` to request the run loop break (quit).
fn handle_tree_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => return start_quit(app),
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => return start_quit(app),
        _ => {}
    }

    match key.code {
        KeyCode::Char('j') | KeyCode::Down if key.modifiers == KeyModifiers::NONE => {
            app.ctx.registry.with_tree_mut(|t| t.move_down());
        }
        KeyCode::Char('k') | KeyCode::Up if key.modifiers == KeyModifiers::NONE => {
            app.ctx.registry.with_tree_mut(|t| t.move_up());
        }
        KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
            app.ctx.registry.with_tree_mut(|t| t.toggle_expand());
        }
        KeyCode::Enter | KeyCode::Char('o') if key.modifiers == KeyModifiers::NONE => {
            jump_in(app);
        }
        KeyCode::Char('l') if key.modifiers == KeyModifiers::CONTROL => {
            jump_in(app);
        }
        KeyCode::Char('n') if key.modifiers == KeyModifiers::NONE => {
            app.status_message = None;
            let default_cwd = std::env::current_dir()
                .map(|p| display_path_from_home(&p))
                .unwrap_or_default();
            app.input = Some(InputState {
                action: PendingAction::SpawnRoot,
                buffer: default_cwd,
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
    }
    true
}

/// `Ctrl-l`/`Enter`/`o` on a selected, live agent moves focus into its pane
/// — the same path serves read-only preview and jump-in,
/// this just starts routing keys to the PTY instead of the tree.
fn jump_in(app: &mut App) {
    let Some(id) = app.ctx.registry.with_tree(|t| t.selected()).map(|n| n.id) else { return };
    if app.ctx.sessions.is_alive(&id) {
        app.focus = Focus::Pane;
    } else {
        app.status_message = Some("agent is not running".to_string());
    }
}

/// `q`/`Ctrl-C` from the tree: quit immediately if nothing would be lost,
/// otherwise ask for confirmation first — v1 has no persistence, quitting
/// kills every live agent. Returns `false` only when it's
/// safe to quit immediately (the confirm path breaks the loop itself, from
/// `handle_confirm_key`, once the user answers).
fn start_quit(app: &mut App) -> bool {
    if count_live_agents(app) == 0 {
        return false;
    }
    app.status_message = None;
    app.confirm = Some(ConfirmState { action: ConfirmAction::Quit });
    true
}

fn count_live_agents(app: &App) -> usize {
    app.ctx.registry.snapshot().iter().filter(|a| app.ctx.sessions.is_alive(&a.id)).count()
}

fn quit_all_agents(app: &App) {
    for agent in app.ctx.registry.snapshot() {
        app.ctx.sessions.kill(&agent.id);
    }
}

/// `Focus::Pane` key handling: `Ctrl-h` is the only intercepted key — it
/// returns focus to the tree. Everything else, modifiers included, encodes
/// to bytes and forwards to the agent's PTY untouched (Ctrl-c reaches the
/// agent as an interrupt, never quits Overseer while a pane is focused).
fn handle_pane_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('h') && key.modifiers == KeyModifiers::CONTROL {
        app.focus = Focus::Tree;
        return;
    }

    let Some(id) = app.ctx.registry.with_tree(|t| t.selected()).map(|n| n.id) else {
        app.focus = Focus::Tree;
        return;
    };
    let mode = app.ctx.sessions.with_term(&id, |term| *term.mode()).unwrap_or_default();
    if let Some(bytes) = session::keys::encode_key(&key, mode) {
        app.ctx.sessions.write(&id, bytes);
    }
}

fn forward_paste(app: &mut App, text: &str) {
    let Some(id) = app.ctx.registry.with_tree(|t| t.selected()).map(|n| n.id) else { return };
    let mode = app.ctx.sessions.with_term(&id, |term| *term.mode()).unwrap_or_default();
    app.ctx.sessions.write(&id, session::keys::encode_paste(text, mode));
}

/// Builds the status-bar override text for the active confirm prompt, or the
/// last status message. `None` means the status bar should show its normal
/// hints. Spawn input (`n`/`s`) no longer goes through here — it renders as
/// its own modal (`ui::render_spawn_modal`), driven directly by `app.input`.
fn build_prompt(app: &App) -> Option<String> {
    if let Some(confirm) = &app.confirm {
        return Some(match &confirm.action {
            ConfirmAction::Drop { agent_id, recursive } => {
                let name = app
                    .ctx
                    .registry
                    .get(agent_id)
                    .map(|d| d.name)
                    .unwrap_or_else(|| "?".to_string());
                let descendants = app
                    .ctx
                    .registry
                    .with_tree(|t| t.subtree_ids_postorder(agent_id))
                    .map(|ids| ids.len().saturating_sub(1))
                    .unwrap_or(0);
                let suffix = if *recursive && descendants > 0 {
                    format!(" + {descendants} children")
                } else {
                    String::new()
                };
                format!("drop '{name}'{suffix}? (y/n)")
            }
            ConfirmAction::Quit => {
                let n = count_live_agents(app);
                let noun = if n == 1 { "agent" } else { "agents" };
                format!("{n} {noun} running and will be killed — quit? (y/n)")
            }
        });
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
        app.confirm = Some(ConfirmState { action: ConfirmAction::Drop { agent_id: node.id, recursive } });
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
///
/// `SpawnRoot`'s empty-buffer semantics differ from `SpawnChild`'s: an empty repo
/// path means "use cwd" (not "cancel") — the buffer is prefilled with cwd on `n`
/// already, so an empty buffer only happens if the user deliberately cleared it.
fn submit_input(app: &mut App, input: InputState) {
    match input.action {
        PendingAction::SpawnRoot => {
            let path_text = input.buffer.trim();
            let cwd = if path_text.is_empty() {
                std::env::current_dir().ok()
            } else {
                Some(expand_repo_path(path_text))
            };
            dispatch_and_report(app, Request::Start { cwd });
        }
        PendingAction::SpawnChild { parent_id } => {
            let task = input.buffer.trim().to_string();
            if task.is_empty() {
                app.status_message = Some("spawn cancelled: empty task".to_string());
                return;
            }
            let Some(parent) = app.ctx.registry.get(&parent_id) else {
                app.status_message = Some("spawn failed: parent no longer exists".to_string());
                return;
            };
            dispatch_and_report(app, Request::Spawn { parent_id, task, adapter: None, cwd: parent.cwd });
        }
    }
}

fn dispatch_and_report(app: &mut App, req: Request) {
    let resp = dispatch(&app.ctx, req);
    app.status_message = if resp.ok {
        None
    } else {
        Some(format!("spawn failed: {}", resp.error.unwrap_or_else(|| "unknown error".to_string())))
    };
}

/// Renders `path` relative to `$HOME` (e.g. `~/projects/overseer`) for display in
/// the spawn-root modal — the inverse of `expand_repo_path`. Falls back to the
/// absolute path unchanged when `path` isn't under home (or home can't be resolved).
fn display_path_from_home(path: &std::path::Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Expands a leading `~/` using `$HOME`, since `PathBuf` does no shell expansion
/// on its own and the repo-path prompt is otherwise typed like a shell argument.
fn expand_repo_path(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(text)
}

/// Returns `false` to request the run loop break (a confirmed quit).
fn handle_confirm_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(confirm) = app.confirm.take() else { return true };
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => match confirm.action {
            ConfirmAction::Drop { agent_id, recursive } => {
                match drop_agent(&app.ctx.registry, &app.ctx.sessions, &agent_id, recursive, true) {
                    Ok(()) => app.status_message = None,
                    Err(e) => app.status_message = Some(format!("drop failed: {e}")),
                }
                true
            }
            ConfirmAction::Quit => {
                quit_all_agents(app);
                false
            }
        },
        KeyCode::Char('n') | KeyCode::Esc => {
            app.status_message = None;
            true
        }
        _ => {
            // Not a recognized response — keep the confirm prompt open.
            app.confirm = Some(confirm);
            true
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
        let cmd = Command::Status { status: StatusArg::Running, message: None, from_hook: false };
        let result = build_request(cmd).unwrap();
        assert!(result.is_none(), "Status without OVERSEER_AGENT_ID should be a silent no-op");
    }

    #[test]
    fn build_request_status_with_env_var_returns_request() {
        let id = AgentId::new();
        std::env::set_var("OVERSEER_AGENT_ID", id.0.to_string());
        let cmd = Command::Status { status: StatusArg::Done, message: None, from_hook: false };
        let result = build_request(cmd).unwrap();
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Request::Status { .. }));
        std::env::remove_var("OVERSEER_AGENT_ID");
    }

    #[test]
    fn build_request_start_returns_start() {
        let cmd = Command::Start { cwd: Some(PathBuf::from("/tmp/myrepo")) };
        let req = build_request(cmd).unwrap().unwrap();
        assert!(matches!(req, Request::Start { cwd } if cwd == Some(PathBuf::from("/tmp/myrepo"))));
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
    fn mock_mode_never_gets_a_real_session_manager() {
        assert!(session_manager_for(true).is_dry_run());
        assert!(!session_manager_for(false).is_dry_run());
    }

    // ── classify_hook_status ──────────────────────────────────────────────────

    #[test]
    fn classify_hook_status_leaves_non_blocked_untouched() {
        assert_eq!(classify_hook_status(AgentStatus::Running, None), AgentStatus::Running);
        assert_eq!(classify_hook_status(AgentStatus::Idle, None), AgentStatus::Idle);
    }

    #[test]
    fn classify_hook_status_no_payload_stays_blocked() {
        assert_eq!(classify_hook_status(AgentStatus::Blocked, None), AgentStatus::Blocked);
    }

    #[test]
    fn classify_hook_status_permission_request_stays_blocked() {
        let payload = agent::hook::HookPayload {
            transcript_path: None,
            message: Some("Claude needs your permission to use Bash".to_string()),
        };
        assert_eq!(classify_hook_status(AgentStatus::Blocked, Some(&payload)), AgentStatus::Blocked);
    }

    #[test]
    fn classify_hook_status_idle_nag_downgrades_to_idle() {
        let payload = agent::hook::HookPayload {
            transcript_path: None,
            message: Some("Claude is waiting for your input".to_string()),
        };
        assert_eq!(classify_hook_status(AgentStatus::Blocked, Some(&payload)), AgentStatus::Idle);
    }

    // ── display_path_from_home ───────────────────────────────────────────────

    #[test]
    fn display_path_from_home_under_home_uses_tilde() {
        let home = dirs::home_dir().expect("test requires resolvable $HOME");
        let path = home.join("projects/overseer");
        assert_eq!(display_path_from_home(&path), "~/projects/overseer");
    }

    #[test]
    fn display_path_from_home_outside_home_is_unchanged() {
        let path = PathBuf::from("/opt/other/repo");
        assert_eq!(display_path_from_home(&path), "/opt/other/repo");
    }

    #[test]
    fn display_path_from_home_home_itself_is_tilde() {
        let home = dirs::home_dir().expect("test requires resolvable $HOME");
        assert_eq!(display_path_from_home(&home), "~");
    }

    // ── quit guard ────────────────────────────────────────────────────────────

    fn register_agent(app: &App, id: Option<AgentId>) -> AgentId {
        use crate::agent::RegisterArgs;
        app.ctx
            .registry
            .register(RegisterArgs {
                id,
                name: "agent".to_string(),
                role: AgentRole::Root,
                parent_id: None,
                adapter: "shell".to_string(),
                repo: "repo".to_string(),
                cwd: PathBuf::from("/tmp"),
                branch: None,
                initial_status: AgentStatus::Idle,
            })
            .unwrap()
            .id
    }

    fn app_with_sessions(sessions: SessionManager) -> App {
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(sessions),
            socket: PathBuf::from("/tmp/overseer-quit-test.sock"),
            git: Arc::new(GitClient::new()),
            config: Arc::new(Config::default()),
            watch_sessions: false,
        });
        App::new(ctx)
    }

    #[test]
    fn count_live_agents_reflects_session_liveness_not_registry_size() {
        let live_id = AgentId::new();
        let sessions =
            SessionManager::dry_run_with_live_sessions([live_id.clone()].into_iter().collect());
        let app = app_with_sessions(sessions);
        register_agent(&app, Some(live_id));
        register_agent(&app, None); // registered, but its session isn't in the live set

        assert_eq!(count_live_agents(&app), 1);
    }

    #[test]
    fn start_quit_returns_false_immediately_when_nothing_is_live() {
        let mut app = app_with_sessions(SessionManager::dry_run());
        register_agent(&app, None); // registered but dead — nothing to lose

        assert!(!start_quit(&mut app));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn start_quit_asks_for_confirmation_when_an_agent_is_live() {
        let live_id = AgentId::new();
        let sessions =
            SessionManager::dry_run_with_live_sessions([live_id.clone()].into_iter().collect());
        let mut app = app_with_sessions(sessions);
        register_agent(&app, Some(live_id));

        assert!(start_quit(&mut app));
        assert!(matches!(app.confirm.as_ref().unwrap().action, ConfirmAction::Quit));
    }

    #[test]
    fn confirming_quit_breaks_the_loop() {
        let mut app = app_with_sessions(SessionManager::dry_run());
        app.confirm = Some(ConfirmState { action: ConfirmAction::Quit });

        let should_continue = handle_confirm_key(&mut app, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(!should_continue);
    }

    #[test]
    fn cancelling_quit_keeps_running_and_clears_the_prompt() {
        let mut app = app_with_sessions(SessionManager::dry_run());
        app.confirm = Some(ConfirmState { action: ConfirmAction::Quit });

        let should_continue = handle_confirm_key(&mut app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        assert!(should_continue);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn quit_prompt_text_pluralizes_correctly() {
        let live_id = AgentId::new();
        let sessions =
            SessionManager::dry_run_with_live_sessions([live_id.clone()].into_iter().collect());
        let mut app = app_with_sessions(sessions);
        register_agent(&app, Some(live_id));
        app.confirm = Some(ConfirmState { action: ConfirmAction::Quit });

        let prompt = build_prompt(&app).unwrap();
        assert!(prompt.contains("1 agent running and will be killed"), "{prompt}");
    }
}
