# Overseer

**An IDE for agents.** A terminal-native TUI for observing and steering a fleet of parallel AI coding agents from a single window — instead of juggling five terminal tabs. Built in Rust. Nvim-aesthetic; a single ordinary alt-screen app with no bundled multiplexer — each agent is a PTY Overseer owns directly, emulated in-process via `alacritty_terminal` and rendered straight into the same ratatui frame — with a Unix socket IPC layer that gives agents a lightweight API to register, report status, and spawn children — without MCP overhead.

The agents are already smart. Overseer does **not** reimplement what they do — it does not manage git worktrees, branches, or merges; agents handle their own isolation. Overseer is the observability, routing, and approval surface on top: see every agent's state at a glance, jump into any one to approve or intervene, or leave the parent to supervise its own children.

The usual shape is **one root per repository**. `n` spawns a root as a bare shell in a repo you choose (default: cwd) — Overseer doesn't launch an agent for you. You `cd`/run `claude` (or whatever) yourself, in your own time, exactly as you would without Overseer; the row appears in the tree immediately, named after the repo, and its status flips from `idle` to `running` the moment your agent starts reporting via its hooks. From there you talk to it in natural language — "implement X", "research Y", "write unit tests for Z" — and it fans the work out into child agents, each running in its own PTY (auto-launched via the configured adapter) and surfacing as its own row in the TUI. You can drop into any child for approval or a nudge, or ignore them and let the parent check on them periodically.

The hierarchy is intentionally **flat**: a parent (root) can spawn children, but children cannot spawn further agents. This keeps the tree readable, the user in control, and token costs predictable. A **child's** node name is the **task description** it was spawned with. A **root's** node name is the **repo name** — there's no task description at the point a bare shell is spawned, since no agent runs there until you start one yourself. The adapter (claude, aider, etc.) is shown in the detail panel; a not-yet-running root shows adapter `shell`.

---

## Mental Model

```
You (the user)
  └─ Overseer TUI                                                        ← one window, the whole fleet
       └─ Root  (name: overseer, adapter: shell → claude once you run it) ← bare shell in the repo checkout
            ├─ Child Agent A  (task: auth-module, adapter: claude)       ← own PTY, own branch
            └─ Child Agent B  (task: write-tests, adapter: aider)        ← own PTY, own branch
```

You spawn the root, run your own agent inside it, and talk to it directly; the agent then fans out children on your behalf. Each agent is a PTY Overseer launched (or, for the root, a bare shell it launched) and a row you can jump into. Branch/worktree isolation between children is the **agent's** job, not Overseer's — Overseer just launches the session and gets out of the way.

Agents know their role (`root` or `child`) via injected env vars and a **user-level skill** installed once with `overseer teach <agent>`. Claude Code hooks POST lifecycle events to the Unix socket to report status — zero agent context tokens consumed, nothing written into your repo.

---

## Architecture

```
overseer (binary)
├── ui/               Ratatui-based terminal UI
│   ├── mod           Tree|pane split (~25/75): agent tree, detail, status bar, spawn modal
│   └── term_pane     Paints the selected agent's live alacritty_terminal grid into the pane half
├── session/          PTY + terminal-emulator management
│   ├── pty           SessionManager: owns one alacritty_terminal Term + PTY per agent, keyed by AgentId
│   └── keys          Crossterm KeyEvent -> PTY escape-byte encoder (input path for a focused pane)
├── agent/            Agent model and lifecycle
│   ├── model         AgentNode, AgentStatus, AgentRole, AgentTree
│   ├── registry      AgentRegistry: in-memory tree of registered agents + their metadata
│   ├── adapters/     Pluggable per-agent-type behaviour
│   │   ├── mod       AgentAdapter trait (teach_files, spawn_command, env_inject)
│   │   ├── claude    Claude Code adapter (user-level skill + hooks, launch cmd)
│   │   └── generic   Fallback: raw shell command, env vars only (Phase 5)
│   ├── spawn         Orchestrates session launch + env injection + register
│   └── drop          Kills an agent's PTY (and, recursively, its subtree) + deregisters it
├── git/              Read-only git info via CLI (repo name, current branch) — no worktrees
├── ipc/              Unix socket server (tokio, newline-delimited JSON)
│   ├── server        Binds to $OVERSEER_SOCKET, accepts connections
│   ├── handlers      dispatch: register, status, list, agent, start, spawn
│   ├── protocol      Request / Response / AgentDto wire types (serde)
│   └── client        One-shot sync client used by CLI subcommands
└── config/           TOML config (~/.config/overseer/config.toml)
    ├── schema        Config struct (adapters, keybinds, theme, defaults)
    └── loader        Load + merge with CLI flags
```

---

## Key Components

### IPC Server

Unix domain socket at `/tmp/overseer-<session-id>.sock`. The only channel agents use to talk to Overseer — no MCP, no HTTP, no polling. The `overseer` binary doubles as the client: each subcommand opens the socket, sends one newline-delimited JSON request, prints the reply, and exits. Agents invoke these commands, not raw HTTP endpoints — it's a terminal app, the API is its CLI.

| Command | Args | Description |
|---------|------|-------------|
| `overseer teach` | `<agent> --uninstall?` | Install (or remove) the user-level skill + status hooks for an agent type. Run once at setup, not per launch. |
| `overseer start` | `--cwd?` | Register a root and launch a bare shell for it in its own PTY (default cwd: current directory). No adapter is launched — run your own agent inside it. |
| `overseer register` | `--role --parent-id? --adapter` | Called once on agent startup (usually wired via env, not by hand) |
| `overseer status` | `<status> --message?` | Push a status update for the calling agent. No-op (silent exit 0) when not running under Overseer. |
| `overseer spawn` | `--task --adapter?` | Request a child. Rejected if the caller is already a child. |
| `overseer drop` | `<id> --recursive?` | Kill the agent's PTY and deregister it. Overseer does not touch the agent's branch/worktree. |
| `overseer list` | — | List all agents |
| `overseer agent` | `<id>` | Get agent detail |

Identity (`OVERSEER_AGENT_ID`, socket path) comes from injected env, so commands don't pass it explicitly.

### Agent Adapter Trait

Two surfaces: **teach** (install-time, user-level artifacts) and **launch** (runtime command + env). Both pure — they return data; the `teach` / `start` handlers do the I/O.

```rust
pub trait AgentAdapter: Send + Sync {
    fn name(&self) -> &str;

    // teach (install-time): files written at the USER level, once
    fn user_config_dir(&self) -> Option<PathBuf>;      // e.g. ~/.claude
    fn teach_files(&self) -> Vec<InstalledFile>;       // skill + status hooks

    // launch (runtime): how to start one agent session
    fn spawn_command(&self, ctx: &LaunchContext) -> Command;
    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String>;
}
```

`InstalledFile` is a `(path, content, merge_strategy)` triple written under the agent's user config dir. The Claude Code adapter produces a user-level **skill** (`~/.claude/skills/overseer/SKILL.md`, how to operate inside Overseer) and merges status **hooks** into `~/.claude/settings.json`. Nothing is ever written into the user's repo.

### Agent Awareness (Claude Code Adapter)

Injected env vars per session (the *only* thing Overseer injects at launch):
- `OVERSEER_SOCKET` — Unix socket path
- `OVERSEER_AGENT_ID` — UUID
- `OVERSEER_ROLE` — `root` | `child`
- `OVERSEER_PARENT_ID` — parent UUID (absent for root)
- `OVERSEER_REPO` — repository name

Role behavior lives in the **user-level skill** installed by `overseer teach`, not in a per-launch file:
- Root agents: may spawn children via `overseer spawn --task "<...>"`.
- Child agents: spawning is not permitted; the agent sets up its own branch/worktree for isolation, does the task, and reports done.

User-level `~/.claude/settings.json` hooks (installed by `overseer teach`, shared across all sessions, no-op outside Overseer):
- `PostToolUse` → push status `running`
- `Stop` → push status `done`
- `SessionStart` → when `$OVERSEER_AGENT_ID` is set, point the agent at the Overseer skill; otherwise do nothing

### Workspace

Overseer does **not** manage workspaces. A parent runs in the repo's existing checkout; a child sets up its own git worktree/branch — agents already know how to do this. Overseer's only job is to launch the PTY in the repo and inject identity env. It never runs `git worktree`, never creates branches, and never merges. Integrating an agent's branch is the user's call, same as it would be without Overseer.

### Cleanup

Dropping an agent kills its PTY and deregisters it — that's all. Overseer does not delete branches or worktrees (it didn't create them). Recursive drop is depth-first, children before parent, so no session is orphaned. Root agents cannot be dropped via IPC — only via the TUI.

**v1 has no persistence.** Agents are child processes of the Overseer process itself — quitting (`q`, with anything registered) kills every one of them, no reattach, no daemon. That's an accepted regression versus the old tmux backend (which survived a TUI restart); a daemon split reusing the same `SessionManager` is the planned path back to persistence, not yet built. `q`/`Ctrl-C` confirm first whenever any agent is still registered, so this is never silent.

### TUI Layout

```
┌─────────────────┬──────────────────────────────────────────────────┐
│ AGENTS          │                                                  │
│ ◌ overseer      │                                                  │
│   ├ ● auth-mod  │     the selected agent's live grid, painted      │
│   ├ ○ tests     │     directly into this same ratatui frame by     │
│   └ ✓ docs      │     ui/term_pane — real color, real interaction  │
│ ○ refactor-api  │     once focused (Ctrl-l)                        │
├─────────────────┤                                                  │
│ task:   auth-mod│                                                  │
│ repo:   overseer│                                                  │
│ branch: ovsr/a  │                                                  │
│ status: running │                                                  │
└─────────────────┴──────────────────────────────────────────────────┘
 OVERSEER   3/6 running   j/k nav  Ctrl-l/↵ jump in  n/s spawn  d/D drop  q quit
```

Both columns are ratatui-rendered in one process, one window — `ui::render` does its own ~25/75 horizontal split every frame; there is no second pane, no multiplexer, nothing external compositing the right side. `ui::term_pane` locks the selected agent's `alacritty_terminal::Term` (owned by `SessionManager`) and paints its grid cell-by-cell into that half of the buffer.

Status badges: `●` running · `○` waiting · `◌` idle (spawned, nothing running there yet) · `✓` done · `✗` error · `…` spawning

**Keybinding house style: nvim.** Navigation follows nvim conventions — `j`/`k` within a list, `Ctrl-h`/`Ctrl-l` to move between panes, like nvim window navigation. New bindings should extend this vocabulary, not invent a parallel one — and must never require a prefix-key/chord model. One hard constraint: keys that agents' own TUIs rely on (e.g. `Ctrl-j` = Claude Code's insert-newline) must pass through to a focused agent pane untouched — `Ctrl-h` is the *only* key Overseer intercepts while a pane is focused (real Backspace still works: terminals send `DEL`, not `^H`).

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate agent tree (tree focus only) |
| `Ctrl-l` / `Enter` / `o` | Jump in — moves keyboard focus into the selected agent's pane, if it's alive |
| `Ctrl-h` | From inside a focused pane, jump back out to the tree — the only key a pane intercepts; everything else, Ctrl-c included, forwards to the agent |
| `n` | Spawn a root: a bare shell in a chosen repo (default cwd) — no agent launched, run your own |
| `s` | Spawn child under selected agent (adapter-launched, same as before) |
| `d` | Drop selected agent (confirm prompt) |
| `D` | Recursive drop — agent + all children (confirm prompt) |
| `q` / `Ctrl-C` | Quit — confirms first if any agent is registered (v1 has no persistence, this kills them) |

### Spawn Data Flow

```
Root agent runs: overseer spawn --task "write tests" --adapter claude

IPC server (spawn_blocking):
  → AgentRegistry::register(child, parent=caller)   // rejects if caller is a child
  → adapter = adapter_for(name)
  → SessionManager::launch(agent_id, cwd=repo, adapter.spawn_command(ctx),
                           adapter.env_inject(ctx))
  → replies: {"agent_id": "..."}

TUI re-renders with the new child visible under the parent. The child sets up its own
branch/worktree on startup, per the Overseer skill.
```

`overseer start` (launch a root) is a *different* path — no adapter, no task: it registers `role=root`, `status=idle`, names the node after the repo, and launches a bare shell (`$SHELL`) instead of `adapter.spawn_command(ctx)`. Whatever you run inside that shell (e.g. `claude`) inherits the injected identity env vars from the PTY itself and reports its own status via the same push hooks — Overseer never detects or launches it.

---

## Config

`~/.config/overseer/config.toml`

```toml
[defaults]
adapter = "claude"
spawn_policy = "auto"       # "auto" | "confirm"

[adapters.claude]
command = "claude"
extra_args = ["--dangerously-skip-permissions"]

[adapters.aider]
command = "aider"
extra_args = []

[keybindings]
spawn_child = "s"
spawn_root = "n"
drop = "d"
```

---

## Crate Stack

| Concern | Crate |
|---------|-------|
| TUI | `ratatui` |
| Async runtime | `tokio` |
| IPC server | `tokio` `UnixListener` + `serde_json` (newline-delimited JSON, no HTTP) |
| Git (read-only info) | `std::process::Command` (`git` CLI) — repo name, current branch only |
| Terminal backend | `alacritty_terminal` — PTY spawn + VT100/xterm emulation, in-process, no external multiplexer |
| Config | `toml` + `serde` |
| CLI | `clap` |
| Serialization | `serde_json` |
| UUID | `uuid` |
| Error handling | `anyhow` + `thiserror` |

**Runtime dependencies:** `git`. Standard on macOS/Linux. (No `tmux` — Overseer owns its own PTYs now.)

---

## Distribution

Single statically-linked binary. Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`. Homebrew tap: `nikita-ivanov/tap/overseer`. GitHub Actions handles cross-compile, release assets, and tap formula updates.

---

## Specs & Planning Docs

Implementation plans (`PHASE*.md`), research notes, and the task checklist live in **`.specs/`**, which is **gitignored** — they are local working documents that drive development, not part of the distributed repo. Every new spec/phase plan goes into `.specs/`; never commit one to the repo root. Code comments may cite them by name (e.g. "PHASE6.md §3.5") — resolve those against `.specs/` on the machine where the work happened.

---

## Best Practices

- **IPC is the only shared channel.** Agent ↔ overseer communication always goes through the Unix socket. Never write to shared in-process state from an agent context.
- **The "no grandchildren" rule lives in the IPC server,** in the `spawn` handler. Not in the TUI, not in adapters. One place, always enforced.
- **`SessionManager` is the only terminal-backend boundary.** No `alacritty_terminal` imports outside `session/` and the pane renderer (`ui/term_pane.rs`).
- **Parse functions are pure.** Functions like `parse_session_line` take a `&str` and return a value — no process spawning, no I/O. This makes them trivially testable.
- **`AgentNode` is a data model, not a handle.** It does not own a PTY. Session handles live in `SessionManager`, keyed by `AgentId`. Overseer holds no worktree state at all — that's the agent's.
- **Status is push, not pull.** Agent hooks POST status changes to the socket. Overseer never infers status from PTY output.
- **`ui/` is a render layer only.** No business logic. All state mutations go through `App` / `AgentTree` / `SessionManager` methods.

## What to Avoid

- **No MCP transport.** The choice of Unix socket + hooks is intentional — no token overhead, no plugin registry approval, works locally out of the box.
- **Don't let children spawn children.** It's a hard server-side constraint, not a UI hint. A child calling `spawn` is rejected, full stop. The tree is exactly two levels: roots and their children.
- **Don't hardcode adapter binary paths.** Always resolve through the adapter config so users can point to a custom binary or wrapper.
- **Don't add agent status polling.** If hooks aren't firing, the fix is in `overseer teach` (the installed hooks), not in adding a background poller.
- **Don't reimplement git.** No worktree creation, no branching, no merging, no `git worktree` anywhere. Agents own their isolation. Overseer's only git use is read-only display info (repo name, current branch).
- **Don't write into the user's repo.** All agent config (skill, hooks) is installed at the user level by `overseer teach`. Launch injects env only.
- **Don't skip the confirm prompt for `d`/`D`, or for quitting with agents registered.** Killing a running agent's session interrupts in-flight work — confirm first. v1 has no persistence, so quitting is exactly as destructive as a drop.
