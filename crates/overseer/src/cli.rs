//! CLI definition + client mode: parses `overseer <subcommand>`, builds the
//! matching wire `Request`, and sends it over the socket. `Install` and
//! `Uninstall` are special-cased before reaching `run_client` (see
//! `main.rs`) since they need no socket at all.

use std::path::PathBuf;

use anyhow::Result;

use overseer_core::agent;
use overseer_core::agent::{AgentId, AgentStatus, Attention, AttentionKind, AttentionUpdate};
use overseer_core::ipc;
use overseer_core::ipc::protocol::Request;

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
        /// Read the Claude Code hook payload JSON from stdin to classify a
        /// `blocked` push as the idle nag vs. a real permission request.
        #[arg(long)]
        from_hook: bool,
        /// Self-identifies the calling session's actual harness (e.g.
        /// "claude"/"opencode") — only each adapter's own install hook
        /// passes this, once, at session start. Lets a bare-shell workspace
        /// stop looking like "shell" the moment a real harness actually runs
        /// inside it, which is what an omitted `--adapter` on a later
        /// `overseer spawn` inherits from.
        #[arg(long)]
        adapter: Option<String>,
        /// Set a structured reason requiring attention, independently of lifecycle.
        #[arg(long, value_enum)]
        attention: Option<AttentionArg>,
        /// Clear this attention reason after its structured resolution event.
        #[arg(long, value_enum)]
        clear_attention: Option<AttentionArg>,
        /// Provider-supplied Retry-After delay in seconds. Never inferred.
        #[arg(long)]
        retry_after: Option<u64>,
        /// Harness-computed context usage percentage (0-100).
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        context_pct: Option<u8>,
        /// Authoritative model identifier reported by the harness integration.
        #[arg(long)]
        model_name: Option<String>,
        /// Remove a stale context value when this harness cannot report one.
        #[arg(long)]
        clear_context: bool,
        /// Explicit branch override. Rarely needed — every push already
        /// auto-detects the current branch via `git rev-parse --abbrev-ref
        /// HEAD` run in this process's own cwd (see `detect_current_branch`),
        /// the same self-report mechanism both the Claude Code hook and the
        /// opencode plugin funnel through since both ultimately just invoke
        /// this CLI. This flag exists for parity with `--model-name` and any
        /// future caller that already knows its branch more cheaply.
        #[arg(long)]
        branch: Option<String>,
    },
    /// List all agents.
    List,
    /// Get agent detail.
    Agent {
        id: String,
    },
    /// Install the adapter skill(s) + hooks at the user level (runs once, no
    /// socket needed).
    Install {
        /// Adapter name to install (e.g. "claude").
        agent: String,
        /// Remove only the Overseer-managed entries instead of installing them.
        #[arg(long)]
        uninstall: bool,
    },
    /// Remove the adapter skill(s) + hooks installed at the user level (runs
    /// once, no socket needed). Equivalent to `overseer install <agent>
    /// --uninstall`.
    Uninstall {
        /// Adapter name to uninstall (e.g. "claude").
        agent: String,
    },
    /// Runs the daemon that owns the registry, sessions, and IPC socket across
    /// TUI restarts. Not meant to be run by hand — the TUI auto-spawns one,
    /// detached, the first time it can't reach the socket. Hidden from
    /// `--help` since it's an implementation detail, not a user workflow.
    #[command(hide = true)]
    Daemon,
    /// Start a workspace: a bare shell in a repo (server-side launch via the
    /// running TUI), registered immediately and named after the repo. Run
    /// your own agent inside it whenever you're ready — Overseer picks up
    /// its status via the existing push hooks, no adapter is launched on
    /// your behalf.
    Start {
        /// Repo root to start in (default: current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Request a child agent. Caller identity comes from $OVERSEER_AGENT_ID — rejected
    /// if the caller is itself a child (flat tree: workspaces + children only).
    Spawn {
        /// The child's entire initial prompt — as long as it needs to be.
        #[arg(long)]
        task: String,
        /// Short tree-row label (1-3 words, kebab-case), distinct from
        /// `--task`. Falls back to `--task` verbatim if omitted or blank.
        #[arg(long)]
        name: Option<String>,
        /// Adapter to use. Defaults to the spawning agent's own adapter when
        /// omitted (an opencode workspace's children run opencode too, unless told
        /// otherwise) — never a fixed "claude" default, which would silently
        /// launch the wrong harness for a non-claude workspace.
        #[arg(long)]
        adapter: Option<String>,
    },
    /// Kill the agent's session and deregister it. Workspaces can only be
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
    /// Last-resort forceful cleanup for a daemon `shutdown` can't reach:
    /// wedged/deadlocked and never replying, or already crashed with a
    /// stale socket/lockfile left behind. Tries the same graceful
    /// `Request::Shutdown` first, bounded by a short timeout — only
    /// escalates to signal-killing the daemon process (and any orphaned
    /// agent PTY processes it leaves behind, found by process ancestry
    /// since Overseer keeps no on-disk agent-pid registry) if that doesn't
    /// get a response in time. Reach for `shutdown` (or `Q` in the TUI)
    /// first; this is the fallback for when that's already failed you.
    Kill,
    /// Submit `--text` into the agent's PTY as a prompt, press Enter, and
    /// exit — the scriptable counterpart to typing into a pane in the TUI.
    /// Lets a workspace (or a cron job/script) nudge an idle or blocked
    /// child without a real interactive terminal (AGENTS.md "Attention
    /// Surfacing" leaves re-prompting to a human or the workspace deciding —
    /// this is how that decision gets carried out non-interactively). Not a
    /// new capability: any agent holding `OVERSEER_SOCKET` could already
    /// write into any other agent's PTY via the wire protocol's `Write`
    /// (AGENTS.md "Security") — this just gives that a documented, one-shot
    /// command instead of a hand-rolled socket script.
    Prompt {
        /// Agent id to prompt.
        id: String,
        /// The text to submit as a prompt.
        #[arg(long)]
        text: String,
    },
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

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum AttentionArg {
    Permission,
    RateLimit,
    QuotaLimit,
    Billing,
    ProviderError,
}

impl From<AttentionArg> for AttentionKind {
    fn from(value: AttentionArg) -> Self {
        match value {
            AttentionArg::Permission => AttentionKind::Permission,
            AttentionArg::RateLimit => AttentionKind::RateLimit,
            AttentionArg::QuotaLimit => AttentionKind::QuotaLimit,
            AttentionArg::Billing => AttentionKind::Billing,
            AttentionArg::ProviderError => AttentionKind::ProviderError,
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

pub fn resolve_socket(cli_socket: Option<PathBuf>) -> PathBuf {
    cli_socket
        .or_else(|| std::env::var("OVERSEER_SOCKET").ok().map(PathBuf::from))
        .unwrap_or_else(overseer_core::daemon::default_socket_path)
}

pub fn run_client(socket: PathBuf, cmd: Command, pushed_at: std::time::SystemTime) -> Result<()> {
    // `Prompt` doesn't map to a single `Request` — it's a short stateful
    // sequence (attach, discard the snapshot, two writes) handled entirely
    // by `ipc::client::prompt`, so it's intercepted here rather than routed
    // through `build_request`/`ipc::client::send`'s one-request/response flow.
    let cmd = match cmd {
        Command::Prompt { id, text } => {
            let agent_id = id.parse::<AgentId>().map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            return ipc::client::prompt(&socket, &agent_id, &text);
        }
        other => other,
    };

    let req = match build_request(cmd, pushed_at)? {
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

/// Self-reports the given cwd's current git branch by running `git rev-parse
/// --abbrev-ref HEAD` there — the agent-process-side mirror of the daemon's
/// read-only `GitClient::current_branch` (`git.rs`), reused directly rather
/// than reimplemented. `None` on any failure (not a git repo, `git` missing,
/// zero-commit unborn-branch repos, detached-HEAD edge cases) — a `Status`
/// push must never fail just because branch detection didn't pan out; the
/// node simply keeps whatever branch it already had.
fn detect_current_branch(cwd: &std::path::Path) -> Option<String> {
    overseer_core::git::GitClient::new().current_branch(cwd).ok()
}

/// Self-reports the given cwd's repo: the git repo root's basename
/// (`git rev-parse --show-toplevel`, mirroring `GitClient::repo_name`) when
/// inside one — stable across `cd`s within that repo, since it's the root,
/// not the cwd itself — else the bare directory's own basename via
/// `dir_basename`, same "honest name over a faked one" fallback `Request::Start`
/// already uses. Unlike `detect_current_branch`, this never returns `None`:
/// there's always *some* honest directory name to report, even outside git.
fn detect_current_repo(cwd: &std::path::Path) -> String {
    overseer_core::git::GitClient::new()
        .repo_name(cwd)
        .unwrap_or_else(|_| overseer_core::git::dir_basename(cwd))
}

/// Returns `Ok(None)` for the Status command when `$OVERSEER_AGENT_ID` is unset,
/// indicating a non-Overseer session where the hook should be a silent no-op.
///
/// `pushed_at` is captured by the caller (`main.rs`, as early in the
/// process's life as possible) rather than here, so that clap parsing and
/// `--from-hook`'s transcript read below don't themselves widen the
/// scheduling-jitter window the staleness guard exists to close (STATUS-RACE.md).
fn build_request(cmd: Command, pushed_at: std::time::SystemTime) -> Result<Option<Request>> {
    match cmd {
        Command::Status {
            status,
            message,
            from_hook,
            adapter,
            attention,
            clear_attention,
            retry_after,
            context_pct,
            mut model_name,
            clear_context,
            mut branch,
        } => {
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
                let payload = read_hook_payload();
                status = agent::hook::classify_hook_status(status, payload.as_ref());
                if model_name.is_none() {
                    model_name = payload
                        .and_then(|payload| payload.transcript_path)
                        .and_then(|path| std::fs::read_to_string(path).ok())
                        .and_then(|raw| agent::hook::latest_model_from_transcript(&raw));
                }
            }
            // Self-reported, not `--from-hook`-gated: every `overseer status`
            // invocation is itself the agent process (or a hook/plugin
            // subprocess spawned with the same cwd — verified live for
            // Claude Code, whose hook JSON `cwd` field and the hook
            // subprocess's own OS cwd are identical), so a plain
            // `git rev-parse --abbrev-ref HEAD` here is exactly as
            // authoritative for opencode (which never sets --from-hook) as
            // for Claude. An explicit `--branch` always wins.
            if branch.is_none() {
                branch = std::env::current_dir().ok().and_then(|cwd| detect_current_branch(&cwd));
            }
            // Only a root/workspace ever self-reports a repo/name top-up — a
            // child's `repo` is fixed at spawn time and its `name` is a
            // given/task-derived label, neither of which should drift just
            // because a status push happened to fire from some other
            // directory (e.g. a worktree the child set up for itself). Gated
            // here, not in the registry alone, so a child's own hook fires
            // don't even pay for the extra `git` subprocess call.
            let repo = if std::env::var("OVERSEER_ROLE").ok().as_deref() == Some("root") {
                std::env::current_dir().ok().map(|cwd| detect_current_repo(&cwd))
            } else {
                None
            };
            let attention = match (attention, clear_attention) {
                (Some(_), Some(_)) => return Err(anyhow::anyhow!("--attention and --clear-attention are mutually exclusive")),
                (Some(kind), None) => Some(AttentionUpdate::Set {
                    attention: Attention {
                        kind: kind.into(),
                        message: message.clone(),
                        retry_at: retry_after.map(|seconds| pushed_at + std::time::Duration::from_secs(seconds)),
                        observed_at: pushed_at,
                    },
                }),
                (None, Some(kind)) => Some(AttentionUpdate::Clear { kind: kind.into() }),
                (None, None) => None,
            };

            Ok(Some(Request::Status {
                agent_id,
                status,
                message,
                context_pct,
                clear_context,
                model_name,
                attention,
                adapter,
                branch,
                repo,
                pushed_at,
            }))
        }
        Command::List => Ok(Some(Request::List)),
        Command::Agent { id } => {
            let agent_id = id
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            Ok(Some(Request::Agent { agent_id }))
        }
        Command::Start { cwd } => Ok(Some(Request::Start { cwd })),
        Command::Spawn { task, name, adapter } => {
            let parent_id_str = std::env::var("OVERSEER_AGENT_ID").map_err(|_| {
                anyhow::anyhow!("overseer spawn must be run from an agent session (missing $OVERSEER_AGENT_ID)")
            })?;
            let parent_id = parent_id_str
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid $OVERSEER_AGENT_ID: {e}"))?;
            let cwd = std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("failed to resolve current directory: {e}"))?;
            Ok(Some(Request::Spawn { parent_id, task, name, adapter, cwd }))
        }
        Command::Drop { id, recursive } => {
            let agent_id = id
                .parse::<AgentId>()
                .map_err(|e| anyhow::anyhow!("invalid agent id: {e}"))?;
            Ok(Some(Request::Drop { agent_id, recursive }))
        }
        Command::Shutdown => Ok(Some(Request::Shutdown)),
        Command::Install { .. } => unreachable!("Install is handled before run_client"),
        Command::Uninstall { .. } => unreachable!("Uninstall is handled before run_client"),
        Command::Daemon => unreachable!("Daemon is handled before run_client"),
        Command::Kill => unreachable!("Kill is handled before run_client"),
        Command::Prompt { .. } => unreachable!("Prompt is handled before build_request in run_client"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_core::test_env::EnvGuard;

    #[test]
    fn build_request_status_no_env_var_returns_none() {
        let _env = EnvGuard::unset("OVERSEER_AGENT_ID");
        let cmd = Command::Status {
            status: StatusArg::Running,
            message: None,
            from_hook: false,
            adapter: None,
            attention: None,
            clear_attention: None,
            retry_after: None,
            context_pct: None,
            model_name: None,
            clear_context: false,
            branch: None,
        };
        let result = build_request(cmd, std::time::SystemTime::now()).unwrap();
        assert!(result.is_none(), "Status without OVERSEER_AGENT_ID should be a silent no-op");
    }

    // ── branch self-report ────────────────────────────────────────────────────

    #[test]
    fn build_request_status_explicit_branch_wins_over_auto_detection() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Status {
            status: StatusArg::Running,
            message: None,
            from_hook: false,
            adapter: None,
            attention: None,
            clear_attention: None,
            retry_after: None,
            context_pct: None,
            model_name: None,
            clear_context: false,
            branch: Some("ovsr/explicit".to_string()),
        };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Status { branch: Some(b), .. } if b == "ovsr/explicit"));
    }

    #[test]
    fn detect_current_branch_reads_the_real_branch_of_a_git_repo() {
        let dir = std::env::temp_dir().join(format!("overseer-test-branch-detect-{}", AgentId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git").args(args).current_dir(&dir).status().unwrap().success());
        };
        run(&["init", "-q", "-b", "ovsr/probe-branch", "."]);
        run(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);

        let branch = detect_current_branch(&dir);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(branch.as_deref(), Some("ovsr/probe-branch"));
    }

    #[test]
    fn detect_current_branch_non_git_dir_returns_none() {
        let dir = std::env::temp_dir().join(format!("overseer-test-branch-detect-non-git-{}", AgentId::new()));
        std::fs::create_dir_all(&dir).unwrap();

        let branch = detect_current_branch(&dir);

        std::fs::remove_dir_all(&dir).ok();
        assert!(branch.is_none(), "a non-git directory must not report a fake branch");
    }

    // ── repo self-report ─────────────────────────────────────────────────────

    #[test]
    fn detect_current_repo_reads_the_repo_roots_basename_even_from_a_subdirectory() {
        let dir = std::env::temp_dir().join(format!("overseer-test-repo-detect-{}", AgentId::new()));
        let subdir = dir.join("src").join("nested");
        std::fs::create_dir_all(&subdir).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q", "."]).current_dir(&dir).status().unwrap().success());

        // Detected from deep inside the repo, not its own root -- stays
        // pinned to the repo's own directory name (`git rev-parse
        // --show-toplevel`), never the subdirectory being cd'd into.
        let repo = detect_current_repo(&subdir);

        let expected = dir.file_name().unwrap().to_string_lossy().to_string();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(repo, expected);
    }

    #[test]
    fn detect_current_repo_non_git_dir_falls_back_to_the_bare_directory_name() {
        let dir = std::env::temp_dir().join(format!("overseer-test-repo-detect-non-git-{}", AgentId::new()));
        std::fs::create_dir_all(&dir).unwrap();

        let repo = detect_current_repo(&dir);

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(repo, dir.file_name().unwrap().to_string_lossy(), "outside git, the bare directory name is always honest, never a fake");
    }

    #[test]
    fn build_request_status_self_reports_repo_only_for_a_root() {
        let id = AgentId::new();
        let _env = EnvGuard::set_all(&[("OVERSEER_AGENT_ID", &id.0.to_string()), ("OVERSEER_ROLE", "root")]);
        let cmd = Command::Status {
            status: StatusArg::Running,
            message: None,
            from_hook: false,
            adapter: None,
            attention: None,
            clear_attention: None,
            retry_after: None,
            context_pct: None,
            model_name: None,
            clear_context: false,
            branch: None,
        };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        let expected = detect_current_repo(&std::env::current_dir().unwrap());
        assert!(matches!(req, Request::Status { repo: Some(r), .. } if r == expected));
    }

    #[test]
    fn build_request_status_never_self_reports_repo_for_a_child() {
        let id = AgentId::new();
        let _env = EnvGuard::set_all(&[("OVERSEER_AGENT_ID", &id.0.to_string()), ("OVERSEER_ROLE", "child")]);
        let cmd = Command::Status {
            status: StatusArg::Running,
            message: None,
            from_hook: false,
            adapter: None,
            attention: None,
            clear_attention: None,
            retry_after: None,
            context_pct: None,
            model_name: None,
            clear_context: false,
            branch: None,
        };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Status { repo: None, .. }), "a child must never self-report a repo/name change");
    }

    #[test]
    fn build_request_status_with_env_var_returns_request() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Status {
            status: StatusArg::Done,
            message: None,
            from_hook: false,
            adapter: None,
            attention: None,
            clear_attention: None,
            retry_after: None,
            context_pct: None,
            model_name: None,
            clear_context: false,
            branch: None,
        };
        let result = build_request(cmd, std::time::SystemTime::now()).unwrap();
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Request::Status { .. }));
    }

    #[test]
    fn build_request_status_sets_normalized_attention_and_retry_time() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let pushed_at = std::time::SystemTime::now();
        let cmd = Command::Status {
            status: StatusArg::Running,
            message: Some("limited".to_string()),
            from_hook: false,
            adapter: None,
            attention: Some(AttentionArg::RateLimit),
            clear_attention: None,
            retry_after: Some(30),
            context_pct: None,
            model_name: None,
            clear_context: false,
            branch: None,
        };
        let request = build_request(cmd, pushed_at).unwrap().unwrap();
        match request {
            Request::Status { attention: Some(AttentionUpdate::Set { attention }), .. } => {
                assert_eq!(attention.kind, AttentionKind::RateLimit);
                assert_eq!(attention.message.as_deref(), Some("limited"));
                assert_eq!(attention.observed_at, pushed_at);
                assert_eq!(attention.retry_at, Some(pushed_at + std::time::Duration::from_secs(30)));
            }
            other => panic!("expected attention update, got {other:?}"),
        }
    }

    #[test]
    fn build_request_start_returns_start() {
        let cmd = Command::Start { cwd: Some(PathBuf::from("/tmp/myrepo")) };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Start { cwd } if cwd == Some(PathBuf::from("/tmp/myrepo"))));
    }

    #[test]
    fn build_request_list_returns_list() {
        let req = build_request(Command::List, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::List));
    }

    #[test]
    fn cli_parses_kill_command() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["overseer", "kill"]).unwrap();
        assert!(matches!(cli.cmd, Some(Command::Kill)));
    }

    #[test]
    fn build_request_shutdown_returns_shutdown() {
        let req = build_request(Command::Shutdown, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Shutdown));
    }

    #[test]
    fn build_request_spawn_without_env_var_is_error() {
        let _env = EnvGuard::unset("OVERSEER_AGENT_ID");
        let cmd = Command::Spawn { task: "write tests".to_string(), name: None, adapter: Some("claude".to_string()) };
        assert!(build_request(cmd, std::time::SystemTime::now()).is_err());
    }

    #[test]
    fn build_request_spawn_with_env_var_returns_spawn() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Spawn { task: "write tests".to_string(), name: None, adapter: Some("claude".to_string()) };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Spawn { parent_id, task, .. }
            if parent_id == id && task == "write tests"));
    }

    #[test]
    fn build_request_spawn_with_name_threads_it_through() {
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Spawn {
            task: "write unit tests for the login flow".to_string(),
            name: Some("login-tests".to_string()),
            adapter: Some("claude".to_string()),
        };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Spawn { name: Some(n), .. } if n == "login-tests"));
    }

    #[test]
    fn build_request_spawn_without_adapter_flag_leaves_it_none() {
        // No `--adapter` on the CLI must reach the wire as `None`, not a
        // fixed "claude" default — the handler is what decides the actual
        // default (the caller's own adapter), not clap.
        let id = AgentId::new();
        let _env = EnvGuard::set("OVERSEER_AGENT_ID", &id.0.to_string());
        let cmd = Command::Spawn { task: "write tests".to_string(), name: None, adapter: None };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Spawn { adapter: None, .. }));
    }

    #[test]
    fn build_request_drop_returns_drop() {
        let id = AgentId::new();
        let cmd = Command::Drop { id: id.0.to_string(), recursive: true };
        let req = build_request(cmd, std::time::SystemTime::now()).unwrap().unwrap();
        assert!(matches!(req, Request::Drop { agent_id, recursive: true } if agent_id == id));
    }

    #[test]
    fn build_request_drop_invalid_id_is_error() {
        let cmd = Command::Drop { id: "not-a-uuid".to_string(), recursive: false };
        assert!(build_request(cmd, std::time::SystemTime::now()).is_err());
    }

    // ── prompt (CLI parsing) ────────────────────────────────────────────────
    //
    // `Prompt` doesn't go through `build_request` (see `run_client`'s early
    // return), so it's exercised via clap parsing directly instead of the
    // `build_request`-based pattern the other commands use above. The
    // client-side attach/write sequence itself is covered by
    // `ipc::client::prompt`'s integration test in `ipc::tests`.

    #[test]
    fn cli_parses_prompt_command() {
        use clap::Parser;
        let id = AgentId::new();
        let cli =
            Cli::try_parse_from(["overseer", "prompt", &id.0.to_string(), "--text", "keep going"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Command::Prompt { id: parsed_id, text })
                if parsed_id == id.0.to_string() && text == "keep going"
        ));
    }

    #[test]
    fn cli_prompt_requires_text_flag() {
        use clap::Parser;
        let id = AgentId::new();
        assert!(
            Cli::try_parse_from(["overseer", "prompt", &id.0.to_string()]).is_err(),
            "prompt without --text should fail to parse"
        );
    }

}
