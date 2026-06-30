use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, sync::Arc, time::Duration};

mod agent;
mod app;
mod ipc;
mod session;
mod ui;

use agent::{AgentId, AgentRegistry, AgentRole, AgentStatus};
use app::{App, Focus};
use ipc::protocol::Request;

#[derive(clap::Parser)]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
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
    Status {
        status: StatusArg,
        #[arg(long)]
        message: Option<String>,
    },
    List,
    Agent {
        id: String,
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
        None => run_tui(socket),
        Some(cmd) => run_client(socket, cmd),
    }
}

fn run_tui(socket: PathBuf) -> Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_panic(info);
    }));

    let registry = Arc::new(AgentRegistry::new());
    let reg_clone = registry.clone();
    let socket_clone = socket.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    std::thread::spawn(move || {
        if let Err(e) = ipc::serve_blocking(reg_clone, socket_clone, Some(ready_tx)) {
            eprintln!("IPC server error: {e}");
        }
    });
    ready_rx.recv().ok();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(registry);
    let res = run_app(&mut terminal, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
    let _ = terminal.show_cursor();
    let _ = std::fs::remove_file(&socket);

    res
}

fn run_client(socket: PathBuf, cmd: Command) -> Result<()> {
    let req = build_request(cmd)?;
    let resp = ipc::client::send(&socket, &req)?;

    if resp.ok {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        Ok(())
    } else {
        let error = resp.error.unwrap_or_else(|| "unknown error".to_string());
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn build_request(cmd: Command) -> Result<Request> {
    match cmd {
        Command::Register { role, name, parent_id, adapter, repo } => {
            let parent_id = match parent_id {
                Some(s) => Some(
                    s.parse::<AgentId>()
                        .map_err(|e| anyhow::anyhow!("invalid --parent-id: {e}"))?,
                ),
                None => None,
            };
            Ok(Request::Register {
                id: None,
                name,
                role: role.into(),
                parent_id,
                adapter: Some(adapter),
                repo: Some(repo),
            })
        }
        Command::Status { status, message } => {
            let raw = std::env::var("OVERSEER_AGENT_ID")
                .context("$OVERSEER_AGENT_ID not set (required for 'status' command)")?;
            let agent_id = raw
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid $OVERSEER_AGENT_ID: {e}"))?;
            Ok(Request::Status { agent_id, status: status.into(), message })
        }
        Command::List => Ok(Request::List),
        Command::Agent { id } => {
            let agent_id = id
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            Ok(Request::Agent { agent_id })
        }
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        let tick = app.tick;
        let focus = &app.focus;
        app.registry.with_tree(|tree| {
            terminal.draw(|f| ui::render(f, focus, tree, tick))
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
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
                            app.registry.with_tree_mut(|t| t.move_down());
                        }
                        KeyCode::Char('k') | KeyCode::Up
                            if key.modifiers == KeyModifiers::NONE =>
                        {
                            app.registry.with_tree_mut(|t| t.move_up());
                        }
                        KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                            app.registry.with_tree_mut(|t| t.toggle_expand());
                        }
                        KeyCode::Enter | KeyCode::Char('o')
                            if key.modifiers == KeyModifiers::NONE =>
                        {
                            app.focus = Focus::Pane;
                        }
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

        app.tick();
    }
    Ok(())
}
