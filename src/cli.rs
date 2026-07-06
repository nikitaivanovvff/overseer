//! CLI definition + client mode: parses `overseer <subcommand>`, builds the
//! matching wire `Request`, and sends it over the socket. `Install` is
//! special-cased before reaching `run_client` (see `main.rs`) since it needs
//! no socket at all.

use std::path::PathBuf;

use anyhow::Result;

use crate::agent;
use crate::agent::{AgentId, AgentStatus};
use crate::ipc;
use crate::ipc::protocol::Request;

#[derive(clap::Parser)]
pub struct Cli {
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,
    #[arg(long)]
    pub mock: bool,
    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(clap::Subcommand)]
pub enum Command {
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
    /// Install the adapter skill(s) + hooks at the user level (runs once, no
    /// socket needed). `teach` is kept as a hidden alias for muscle memory.
    #[command(alias = "teach")]
    Install {
        /// Adapter name to install (e.g. "claude").
        agent: String,
        /// Remove only the Overseer-managed entries instead of installing them.
        #[arg(long)]
        uninstall: bool,
    },
    /// Runs the daemon that owns the registry, sessions, and IPC socket across
    /// TUI restarts. Not meant to be run by hand — the TUI auto-spawns one,
    /// detached, the first time it can't reach the socket. Hidden from
    /// `--help` since it's an implementation detail, not a user workflow.
    #[command(hide = true)]
    Daemon,
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
    /// The kill switch: recursive-drops every agent, then exits the daemon.
    /// The TUI's own `Q` keybind confirms and sends this same request — this
    /// is the CLI path for scripting it or when the TUI isn't running.
    Shutdown,
}

/// Pushable statuses only — `Spawning` is set at registration time, never
/// pushed by a hook or agent.
#[derive(Clone, clap::ValueEnum)]
pub enum StatusArg {
    Running,
    Idle,
    Blocked,
    Done,
    Error,
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

pub fn resolve_socket(cli_socket: Option<PathBuf>) -> PathBuf {
    cli_socket
        .or_else(|| std::env::var("OVERSEER_SOCKET").ok().map(PathBuf::from))
        .unwrap_or_else(crate::daemon::default_socket_path)
}

pub fn run_client(socket: PathBuf, cmd: Command) -> Result<()> {
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

/// Reads the transcript at `path` and extracts a context %. `None` on any read
/// failure or if the transcript has no usage data yet (e.g. brand new) —
/// never fails the hook over this, it just means no pct on this push.
fn read_context_pct(path: &str) -> Option<u8> {
    let contents = std::fs::read_to_string(path).ok()?;
    agent::hook::context_pct_from_transcript(&contents)
}

/// Returns `Ok(None)` for the Status command when `$OVERSEER_AGENT_ID` is unset,
/// indicating a non-Overseer session where the hook should be a silent no-op.
fn build_request(cmd: Command) -> Result<Option<Request>> {
    match cmd {
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
            let mut context_pct = None;
            if from_hook {
                let payload = read_hook_payload();
                status = agent::hook::classify_hook_status(status, payload.as_ref());
                context_pct = payload
                    .as_ref()
                    .and_then(|p| p.transcript_path.as_deref())
                    .and_then(read_context_pct);
            }

            Ok(Some(Request::Status { agent_id, status, message, context_pct }))
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
        Command::Shutdown => Ok(Some(Request::Shutdown)),
        Command::Install { .. } => unreachable!("Install is handled before run_client"),
        Command::Daemon => unreachable!("Daemon is handled before run_client"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    #[test]
    fn build_request_status_no_env_var_returns_none() {
        let _env = EnvGuard::unset("OVERSEER_AGENT_ID");
        let cmd = Command::Status { status: StatusArg::Running, message: None, from_hook: false };
        let result = build_request(cmd).unwrap();
        assert!(result.is_none(), "Status without OVERSEER_AGENT_ID should be a silent no-op");
    }

    #[test]
    fn build_request_status_with_env_var_returns_request() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Status { status: StatusArg::Done, message: None, from_hook: false };
        let result = build_request(cmd).unwrap();
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Request::Status { .. }));
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
    fn build_request_shutdown_returns_shutdown() {
        let req = build_request(Command::Shutdown).unwrap().unwrap();
        assert!(matches!(req, Request::Shutdown));
    }

    #[test]
    fn build_request_spawn_without_env_var_is_error() {
        let _env = EnvGuard::unset("OVERSEER_AGENT_ID");
        let cmd = Command::Spawn { task: "write tests".to_string(), adapter: "claude".to_string() };
        assert!(build_request(cmd).is_err());
    }

    #[test]
    fn build_request_spawn_with_env_var_returns_spawn() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Spawn { task: "write tests".to_string(), adapter: "claude".to_string() };
        let req = build_request(cmd).unwrap().unwrap();
        assert!(matches!(req, Request::Spawn { parent_id, task, .. }
            if parent_id == id && task == "write tests"));
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

    // ── read_context_pct ──────────────────────────────────────────────────────

    #[test]
    fn read_context_pct_reads_and_parses_a_real_file() {
        let path = std::env::temp_dir().join(format!("overseer-transcript-test-{}", AgentId::new()));
        std::fs::write(
            &path,
            r#"{"message":{"usage":{"input_tokens":100000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        )
        .unwrap();
        assert_eq!(read_context_pct(path.to_str().unwrap()), Some(50));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_context_pct_missing_file_returns_none() {
        assert_eq!(read_context_pct("/nonexistent/transcript.jsonl"), None);
    }
}
