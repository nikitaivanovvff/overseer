# Overseer

A terminal-native IDE for orchestrating AI agent hierarchies. Built in Rust. Nvim-aesthetic TUI backed by tmux, with a Unix socket IPC layer that gives agents a lightweight API to register themselves, report status, and spawn children — without MCP overhead.

The hierarchy is intentionally **flat**: root agents can spawn child agents, but children cannot spawn further agents. This keeps the tree readable, the user in control, and token costs predictable. The node name in the tree is the **task description**, not the agent binary. The adapter (claude, aider, etc.) is shown in the detail panel.

---

## Mental Model

```
You (the user)
  └─ Overseer TUI
       ├─ Root Agent  (task: implement-auth, adapter: claude)      ← worktree on branch main
       │    ├─ Child Agent A  (task: auth-module, adapter: claude) ← worktree on branch overseer/a3f2
       │    └─ Child Agent B  (task: write-tests, adapter: aider)  ← worktree on branch overseer/b8e1
       └─ Root Agent 2  (task: refactor-api, adapter: claude)      ← worktree on branch main
```

Agents know their role (`root` or `child`) via injected env vars, an injected `CLAUDE.md`, and Claude Code hooks that POST lifecycle events to the Unix socket — zero agent context tokens consumed.

---

## Architecture

```
overseer (binary)
├── tui/              Ratatui-based terminal UI
│   ├── layout        Left panel (agent tree) + right panel (active tmux pane embed)
│   ├── agent_tree    Renders hierarchy, status badges, keyboard nav
│   └── status_bar    Global status, keybind hints
├── session/          Tmux management
│   ├── tmux          Control-mode client (sends tmux commands, parses events)
│   └── registry      In-memory map of AgentSession → tmux session/window/pane
├── agent/            Agent model and lifecycle
│   ├── model         AgentNode, AgentStatus, AgentRole, AgentTree
│   ├── adapters/     Pluggable per-agent-type behaviour
│   │   ├── trait     AgentAdapter (spawn_cmd, env_inject, workspace_files, status_parse)
│   │   ├── claude    Claude Code adapter (hooks, CLAUDE.md injection)
│   │   └── generic   Fallback: raw shell command, env vars only
│   └── spawn         Orchestrates worktree + session + env + file injection
├── workspace/        Git worktree management
│   ├── worktree      Create / delete / list worktrees via git2 or git CLI
│   └── branch        Branch naming: overseer/<agent-id>
├── ipc/              Unix socket HTTP server (tokio + axum)
│   ├── server        Binds to $OVERSEER_SOCKET, serves agent API
│   └── handlers      /register, /status, /spawn, /agents, /agents/:id
└── config/           TOML config (~/.config/overseer/config.toml)
    ├── schema        Config struct (adapters, keybinds, theme, defaults)
    └── loader        Load + merge with CLI flags
```

---

## Key Components

### IPC Server

Unix domain socket at `/tmp/overseer-<session-id>.sock`. The only channel agents use to talk to Overseer — no MCP, no polling.

| Method | Path | Body | Description |
|--------|------|------|-------------|
| `POST` | `/register` | `{agent_id, role, parent_id?, adapter}` | Called once on agent startup |
| `PATCH` | `/agents/:id/status` | `{status, message?}` | Push status update |
| `POST` | `/agents/:id/spawn` | `{task, adapter?, branch_hint?}` | Request a child. Returns `403` if caller is a child. |
| `DELETE` | `/agents/:id` | `{recursive?: bool}` | Kill session, delete worktree + branch |
| `GET` | `/agents` | — | List all agents |
| `GET` | `/agents/:id` | — | Get agent detail |

### Agent Adapter Trait

```rust
pub trait AgentAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn spawn_command(&self, ctx: &SpawnContext) -> Command;
    fn env_inject(&self, ctx: &SpawnContext) -> HashMap<String, String>;
    fn workspace_files(&self, ctx: &SpawnContext) -> Vec<WorkspaceFile>;
}
```

`WorkspaceFile` is a `(relative_path, content)` pair. Claude Code adapter produces a `CLAUDE.md` and a `.claude/settings.json` with hooks wired to the IPC socket.

### Agent Awareness (Claude Code Adapter)

Injected env vars per session:
- `OVERSEER_SOCKET` — Unix socket path
- `OVERSEER_AGENT_ID` — UUID
- `OVERSEER_ROLE` — `root` | `child`
- `OVERSEER_PARENT_ID` — parent UUID (absent for root)
- `OVERSEER_BRANCH` — git branch for this workspace
- `OVERSEER_REPO` — repository name

Injected `CLAUDE.md`:
- Root agents: instruction to spawn children via `curl $OVERSEER_SOCKET/agents/$OVERSEER_AGENT_ID/spawn`
- Child agents: explicit note that spawning is not permitted; complete the task and report done

Injected `.claude/settings.json` hooks:
- `PostToolUse` → PATCH status to `running`
- `Stop` → PATCH status to `done`

### Workspace

Each agent gets an isolated git worktree under `.overseer-worktrees/agent-<id>/` on branch `overseer/<short-id>`. On completion: **merged**, **archived** (worktree removed, branch kept), or **dropped** (both deleted).

### Cleanup

Recursive by default: depth-first, leaves first, so no orphaned worktrees or branches. Sequence per agent: SIGTERM session → `git worktree remove --force` → `git branch -D` → deregister. Root agents cannot be deleted via IPC — only via TUI.

### TUI Layout

```
┌─────────────────────────────────────────────────────────────────┐
│ OVERSEER  session: project-x                      [q]uit [?]help │
├─────────────────┬───────────────────────────────────────────────┤
│ AGENTS          │ repo: overseer   branch: overseer/a3f2        │
│                 ├───────────────────────────────────────────────┤
│ ● implement-auth│                                               │
│   ├ ● auth-mod  │       active tmux pane (embedded or           │
│   ├ ○ tests     │       switched-to via tmux control mode)      │
│   └ ✓ docs      │                                               │
│ ○ refactor-api  │                                               │
├─────────────────┤                                               │
│ task:   auth-mod│                                               │
│ repo:   overseer│                                               │
│ branch: ovsr/a  │                                               │
│ status: running │                                               │
└─────────────────┴───────────────────────────────────────────────┘
```

Status badges: `●` running · `○` waiting · `✓` done · `✗` error · `…` spawning

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate agent tree |
| `Enter` | Focus selected agent's pane |
| `n` | Spawn new root agent |
| `s` | Spawn child under selected agent |
| `m` | Merge selected agent's branch |
| `d` | Drop selected agent (confirm prompt) |
| `D` | Recursive drop — agent + all children (confirm prompt) |
| `Tab` | Toggle focus: tree ↔ pane |
| `q` | Quit (agents keep running in tmux) |
| `Q` | Quit + kill all sessions |

### Spawn Data Flow

```
Root agent calls: curl -X POST $OVERSEER_SOCKET/agents/$OVERSEER_AGENT_ID/spawn
                       -d '{"task": "write tests", "adapter": "claude"}'

IPC server:
  → WorkspaceManager::create_worktree(parent_branch, new_branch)
  → AgentAdapter::spawn_command(ctx)
  → SessionManager::create_session(cmd, env, cwd=worktree_path)
  → AgentRegistry::insert(node, parent=caller)
  → responds: {"agent_id": "...", "branch": "overseer/b5c1"}

TUI re-renders with new child visible under the root.
```

---

## Config

`~/.config/overseer/config.toml`

```toml
[defaults]
adapter = "claude"
spawn_policy = "auto"       # "auto" | "confirm"
branch_prefix = "overseer"
worktree_dir = ".overseer-worktrees"
max_depth = 1               # root + children only; raise to allow deeper nesting

[adapters.claude]
command = "claude"
extra_args = ["--dangerously-skip-permissions"]

[adapters.aider]
command = "aider"
extra_args = []

[keybindings]
spawn_child = "s"
spawn_root = "n"
merge = "m"
drop = "d"
```

---

## Crate Stack

| Concern | Crate |
|---------|-------|
| TUI | `ratatui` |
| Async runtime | `tokio` |
| IPC HTTP server | `axum` (over UnixListener) |
| Git worktrees | `git2` |
| Tmux control | `std::process::Command` (tmux -C) |
| Config | `toml` + `serde` |
| CLI | `clap` |
| Serialization | `serde_json` |
| UUID | `uuid` |
| Error handling | `anyhow` + `thiserror` |

**Runtime dependencies:** `git`, `tmux`, `curl`. All standard on macOS/Linux.

---

## Distribution

Single statically-linked binary. Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`. Homebrew tap: `nikita-ivanov/tap/overseer`. GitHub Actions handles cross-compile, release assets, and tap formula updates.

---

## Best Practices

- **IPC is the only shared channel.** Agent ↔ overseer communication always goes through the Unix socket. Never write to shared in-process state from an agent context.
- **Depth enforcement lives in the IPC server,** in the `/spawn` handler. Not in the TUI, not in adapters. One place, always enforced.
- **`TmuxClient` is the only tmux boundary.** No raw `Command::new("tmux")` outside of `session/tmux.rs`.
- **Parse functions are pure.** Functions like `parse_session_line` take a `&str` and return a value — no process spawning, no I/O. This makes them trivially testable.
- **`AgentNode` is a data model, not a handle.** It does not own a tmux session or a worktree path. Those live in `SessionRegistry` and `WorkspaceManager`.
- **Status is push, not pull.** Agent hooks POST status changes to the socket. Overseer never polls tmux pane output to infer status.
- **`tui.rs` is a render layer only.** No business logic. All state mutations go through `App` / `AgentTree` methods.

## What to Avoid

- **No MCP transport.** The choice of Unix socket + hooks is intentional — no token overhead, no plugin registry approval, works locally out of the box.
- **Don't let children spawn children.** `max_depth` is a hard server-side constraint, not a UI hint. A child calling `/spawn` gets a `403`, full stop.
- **Don't hardcode adapter binary paths.** Always resolve through the adapter config so users can point to a custom binary or wrapper.
- **Don't add agent status polling.** If hooks aren't firing, the fix is in the hook injection, not in adding a background poller.
- **Don't put worktree paths or branch names in `AgentNode`.** The node is for the registry and UI. `WorkspaceManager` owns the filesystem layout.
- **Don't skip the confirm prompt for `d`/`D`.** Deleting a worktree + branch is destructive and irreversible.
