# Overseer

**An IDE for agents.** A terminal-native TUI for observing and steering a fleet of parallel AI coding agents from a single window — instead of juggling five terminal tabs. Built in Rust. Nvim-aesthetic; a single ordinary alt-screen app with no bundled multiplexer — each agent is a PTY Overseer owns directly, emulated in-process and rendered straight into the same ratatui frame — with a Unix socket IPC layer that gives agents a lightweight API to register, report status, and spawn children — without MCP overhead.

The agents are already smart. Overseer does **not** reimplement what they do — it does not manage git worktrees, branches, or merges; agents handle their own isolation. Overseer is the observability, routing, and approval surface on top: see every agent's state at a glance, jump into any one to approve or intervene, or leave the parent to supervise its own children.

The usual shape is **one workspace per repository**. `n` spawns a workspace in a repo you choose (default: cwd). If you've run `overseer install <agent>` for at least one harness, `n` first asks which to launch — every installed adapter plus a "bare terminal" choice — then prompts for the repo path; picking an adapter opens a shell in that repo with that harness's launch command auto-typed and submitted for you — the same PTY shape as a bare shell, just with the command typed in for you instead of by hand, so exiting the harness later drops you back to a live shell instead of ending the workspace. If nothing is installed yet, `n` skips straight to the repo-path prompt and gives you a bare shell, exactly as it always did — Overseer doesn't force an agent on you. Either way the row appears immediately, named after the repo, and its status flips `idle` → `running` the moment your agent starts reporting via its hooks (a bare shell's own `idle` sticks until you run something yourself). From there you talk to it in natural language and it fans work out into child agents, each in its own PTY (auto-launched via the configured adapter), surfacing as its own row. Drop into any child for approval or a nudge, or let the parent check on them periodically.

The hierarchy is intentionally **flat**: a parent (workspace) can spawn children, but children cannot spawn further agents. This keeps the tree readable, the user in control, and token costs predictable. A **child's** node name is the short label it was given at spawn (`--name`) — falling back to its task text verbatim if none was given, since the task can be a whole paragraph and a name shouldn't have to be. A **workspace's** node name is the **repo name** — there's no task description, since a workspace is either a bare shell with nothing running yet or an adapter launched with an empty task, same as a human just opening it. The adapter (claude, opencode, pi) is shown in the detail panel; a workspace spawned as a bare shell shows adapter `shell` until you run something inside it yourself, while a workspace spawned via the picker's adapter choice shows that adapter immediately.

---

## Mental Model

```
You (the user)
  └─ Overseer TUI                                                        ← one window, the whole fleet
       └─ Workspace  (name: overseer, adapter: shell → claude once you run it) ← bare shell in the repo checkout
            ├─ Child Agent A  (task: auth-module, adapter: claude)       ← own PTY, own branch
            └─ Child Agent B  (task: write-tests, adapter: opencode)     ← own PTY, own branch
```

You spawn the workspace — either as a bare shell you run your own agent inside, or, once you've installed at least one adapter, by picking it directly from `n`'s picker — and talk to it directly; it fans out children on your behalf. Every agent (workspace or child) is a PTY Overseer launched and a row you can jump into. Branch/worktree isolation is the **agent's** job, not Overseer's.

Agents know their role (`root` (the wire value for a workspace) or `child`) via injected env vars and a **user-level skill** installed once with `overseer install <agent>`. Claude Code hooks POST lifecycle events to the Unix socket to report status — zero agent context tokens, nothing written into your repo.

The registry and every agent's PTY live in a **daemon** process, not the TUI — an `overseer` launch attaches as a client (auto-spawning one if it isn't running). Quitting the TUI detaches; the daemon and every agent keep running, and a later launch reattaches to exactly what was there before (see "Daemon + Attach Protocol").

---

## Glossary

Canonical names for the things this doc (and conversation about Overseer) refers to. When a user or doc says one of these, this is what it means — see the "TUI Layout" diagram below for where each sits on screen.

| Term | Also called | What it is | Code anchor |
|------|-------------|------------|-------------|
| **Agent structure** pane | tree, agent tree, sidebar, workspaces pane | The left-column list of workspaces with their agents nested under them, titled `WORKSPACES`. Selection/navigation (`j`/`k`), folds, and search all act here. | `ui::render_agent_tree`, `RenderLayout::tree_rect`/`tree_rows` |
| **Agent pane** | pane, terminal pane, live pane | The right column: the selected agent's live terminal, painted cell-by-cell from a `GridSnapshot`. Read-only preview while tree-focused; interactive once jumped in. | `ui::term_pane::render_term_pane`, `RenderLayout::pane_rect` |
| **Details** pane | detail panel | The block under the agent structure showing the selected agent's `task`/`name`, `repo`, `branch`, `status`, `since`, and context %. | `ui::render_agent_detail` |
| **Footer** | status bar | The bottom line: `OVERSEER` brand, fleet summary (`N running · M blocked`), hotkey hints, and transient confirm/error messages. | `ui::render_status_bar` |
| **Workspace** | root — the wire/env value | A top-level agent, one per repo: the shell/harness you talk to directly. Spawned with `n`/`overseer start`. | `AgentRole::Root` |
| **Child** | — | An agent a workspace spawned for one task; cannot spawn further agents. | `AgentRole::Child` |
| **Harness** | adapter, agent type | The AI CLI an agent runs (claude, opencode, pi) and the `AgentAdapter` that knows how to install/launch it. | `agent::adapters` |
| **Jump in** | focus the pane | Moving keyboard focus into the agent pane (`Ctrl-l`/`Enter`/`o`/click); every key but `Ctrl-h` then forwards to the agent. | `tui::jump_in`, `Focus::Pane` |
| **Daemon** | — | The background process owning the registry and every PTY; the TUI is a detachable client of it. | `src/daemon.rs` |

---

## Architecture

```
overseer daemon (background, one per user, auto-spawned by the TUI)
├── AgentRegistry, SessionManager, Config, git/   ← owned by the daemon, not the TUI
├── IPC socket  $XDG_RUNTIME_DIR/overseer/daemon.sock
│               (fallback /tmp/overseer-$UID/daemon.sock), mode 0700 dir, flock-guarded
├── one-shot requests: status/list/agent/start/spawn/drop/shutdown
└── attach connections: long-lived streams of registry events + rendered terminal snapshots

overseer (TUI) = attach client                    overseer <subcommand> = one-shot client
overseer --mock = fully in-process demo data, never spawns a real PTY, never touches a daemon
```

```
overseer (binary)
├── ui/               Ratatui-based terminal UI
│   ├── mod           Tree|pane split (~25/75): agent tree, detail, status bar, spawn modal
│   └── term_pane     Paints the selected agent's pane from a GridSnapshot — the only render
│                     currency, in both --mock and daemon-attached modes
├── session/          PTY + terminal-emulator management, keyed by AgentId
│   ├── pty           SessionManager: owns one PTY + terminal emulator per agent — the only file
│   │                 in the crate that imports alacritty_terminal. Renders GridSnapshot DTOs and
│   │                 tracks a per-agent content-generation counter (bumped on new PTY output)
│   └── keys          Crossterm KeyEvent -> PTY escape-byte encoder, parameterized by the neutral
│                     TermModes struct (input path for a focused pane)
├── agent/            Agent model and lifecycle
│   ├── model         AgentNode, AgentStatus, AgentRole, AgentTree
│   ├── registry      AgentRegistry: in-memory tree of registered agents + a broadcast channel
│   │                 of RegistryEvent (Registered/Removed/StatusChanged/Shutdown) for attach clients
│   ├── hook          Pure Claude Code hook-payload parsing: blocked-vs-idle-nag
│   │                 classification, context % from transcript JSONL
│   ├── adapters/     Pluggable per-agent-type behaviour
│   │   ├── mod       AgentAdapter trait (install_files, spawn_command, env_inject)
│   │   ├── claude    Claude Code adapter (user-level skills + hooks, launch cmd)
│   │   ├── opencode  opencode adapter (auto-loaded plugin.js + instructions array, --prompt launch)
│   │   └── pi        pi adapter (--extension-loaded hook + --append-system-prompt, no blocked support)
│   ├── spawn         Orchestrates session launch + env injection + register
│   └── drop          Kills an agent's PTY (and, recursively, its subtree) + deregisters it
├── git/              Read-only git info via CLI (repo name, current branch) — no worktrees
├── daemon            Daemon process bootstrap: socket path resolution, flock lockfile,
│                     detached auto-spawn (setsid) with retry/backoff for the TUI to attach to
├── ipc/              Unix socket server (tokio, newline-delimited JSON)
│   ├── server        Binds the socket; one-shot request/response *and* the attach event-stream
│   │                 loop (Watch/Unwatch/Write/Resize inward, AttachEvent outward); session-exit watcher
│   ├── handlers      dispatch: status, list, agent, start, spawn, drop, tui_drop, shutdown
│   ├── protocol      Request / Response / AgentDto / AttachEvent / GridSnapshot wire types (serde)
│   └── client        One-shot sync client used by CLI subcommands and daemon reachability probes
├── app               App: Backend enum (Mock | Daemon) unifying tree access, session I/O, and
│                     dispatch behind one API so tui.rs/ui/ don't branch on which backend is live
└── config/           TOML config (~/.config/overseer/config.toml): Config{defaults, adapters,
                      notify, keybindings, theme}. Missing/invalid file falls back to a built-in
                      default; per-field a bad value falls back to that field's own default too
                      (a stderr warning, never a hard error).
```

---

## Key Components

### IPC Server

Unix domain socket at `$XDG_RUNTIME_DIR/overseer/daemon.sock` (falling back to `/tmp/overseer-$UID/daemon.sock`), owned by the daemon — one stable, per-user socket every repo's TUI and every agent's CLI shares. The only channel agents use to talk to Overseer — no MCP, no HTTP, no polling. `overseer` doubles as the client: each subcommand opens the socket, sends one newline-delimited JSON request, prints the reply, exits.

| Command | Args | Description |
|---------|------|-------------|
| `overseer install` | `<agent> --uninstall?` | Install (or remove) the user-level skill(s) + status hooks for an agent type. Run once at setup, not per launch. |
| `overseer uninstall` | `<agent>` | Remove the user-level skill(s) + status hooks for an agent type. Direct top-level equivalent of `overseer install <agent> --uninstall`, both call the same `install::run_install`. |
| `overseer daemon` | — | Runs the daemon itself: binds the socket, serves requests, streams attach events, watches session exits. Hidden from `--help` — not a user workflow, the TUI spawns one automatically. |
| `overseer start` | `--cwd?` | Register a workspace and launch a bare shell for it in its own PTY (default cwd: current directory). No adapter is launched — run your own agent inside it. `--cwd` must exist and be a directory, but doesn't have to be a git repo: outside one, the workspace is named after the directory itself with an empty (`—` in the TUI) branch, rather than faking a repo/branch pair. The wire request (`Request::Start`) also accepts an optional adapter to launch directly instead; only the TUI's `n` picker uses that today, this CLI subcommand doesn't expose an `--adapter` flag. |
| `overseer status` | `<status> --message? --from-hook?` | Push a status update for the calling agent. No-op (silent exit 0) when not running under Overseer. `--from-hook` reads the Claude Code hook payload from stdin to classify a `blocked` push (idle nag vs. real permission request) and attach context % — Claude-specific; opencode's plugin and pi's extension push plain `overseer status <s>`, no `--from-hook`, since their own events are already precise. Each push carries a `pushed_at` wall-clock timestamp captured at process start (`main.rs`, before argument parsing) — every hook fire is its own short-lived process making its own socket connection with no ordering between connections, so a push that fired earlier can otherwise arrive later and clobber a fresher one; `AgentRegistry::set_status` drops any push older than the newest one already applied for that agent. |
| `overseer spawn` | `--task --name? --adapter?` | Request a child. Rejected if the caller is already a child. `--task` is the child's entire initial prompt; `--name` is a short, distinct tree-row label (falls back to `--task` verbatim if omitted or blank). |
| `overseer drop` | `<id> --recursive?` | Kill the agent's PTY and deregister it. Overseer does not touch the agent's branch/worktree. Workspaces are rejected here — only the TUI's `d`/`D` (a distinct wire request, see below) can drop one. |
| `overseer shutdown` | — | The kill switch: recursive-drops every workspace, then the daemon process exits. Same request the TUI's `Q` sends after its confirm. Unchanged, no timeout — assumes a healthy daemon; if it can't reach one, use `kill` instead. |
| `overseer kill` | — | Last-resort forceful cleanup for a daemon `shutdown` can't reach — wedged/deadlocked and never replying, or already crashed with a stale socket/lockfile left behind. Tries the exact same graceful `Request::Shutdown` first, bounded by a ~5s timeout (with retries against the narrow lock-acquired-before-socket-bound startup race) — only escalates to `SIGKILL`ing the daemon pid (read from the `flock`-guarded `daemon.pid` lockfile, see below) if that doesn't answer in time. Also `SIGKILL`s any orphaned agent PTY processes found as the daemon's direct children (best-effort, via `ps` — see "Daemon death is total" below for why the daemon pid alone isn't enough), then removes the stale socket/lockfile so a fresh daemon can bind cleanly. A separate subcommand rather than added behavior on `shutdown`, so existing callers of `shutdown` see no new destructive-by-default timeout/signal-kill. |
| `overseer list` | — | List all agents |
| `overseer agent` | `<id>` | Get agent detail |
| `overseer prompt` | `<id> --text "<text>"` | Submit `--text` into the agent's PTY as a prompt and press Enter, non-interactively, then exit — the scriptable counterpart to typing into a pane in the TUI. Lets a workspace (or a cron job/script) nudge an idle or blocked child without a real interactive terminal (see "Attention Surfacing" below). Internally opens its own `Attach` connection, discards the initial `Snapshot`, and sends two separate `Write`s (text, then a delayed `\r`) — `Write` is only honored on an attach connection, not the one-shot `dispatch` path, so this isn't a thin wrapper over a single request. |

Identity (`OVERSEER_AGENT_ID`, socket path) comes from injected env, so commands don't pass it explicitly.

### Daemon + Attach Protocol

The daemon owns `AgentRegistry` and `SessionManager`; the TUI is its first, richest client. On startup the TUI connects to the socket, or spawns `overseer daemon` detached (`setsid`, stdio to a log file next to the socket) and retries with backoff. A `flock` lockfile (`daemon.pid`) makes a second daemon on the same socket fail fast instead of racing for the bind.

**`overseer kill`: the forceful fallback for when the graceful path can't reach a daemon at all.** `overseer shutdown`/`Q` depend on a healthy daemon actually processing `Request::Shutdown` — wedged (deadlocked, `kill -STOP`'d) or already-crashed daemons don't. `overseer kill` (`src/kill.rs`) covers that gap: it reads `daemon.pid`'s `flock` state, not just its contents, to tell "alive but unresponsive" (something holds the lock) apart from "already dead" (nothing does, only stale files remain, which get removed so a fresh daemon can bind). For the former, it tries the identical `Request::Shutdown` first, bounded by a short timeout — only once that fails does it `SIGKILL` the daemon pid directly. Critically, that alone doesn't reclaim everything: each agent's PTY child calls `setsid()` before exec (own session/process-group, decoupled from the daemon's), so it survives the daemon's death as an orphan rather than dying with it — the daemon pid's own `ppid` links to those children still work though, so `overseer kill` walks `ps` for the daemon's direct children and `SIGKILL`s them too. This is why "Daemon death is total" (Cleanup, below) is a guarantee about the *graceful* paths only (`shutdown`/`Q`, which drop every agent explicitly before the daemon exits) — a forceful `kill -9` of the daemon process alone does not actually take its agents with it, contrary to what that line might suggest in isolation.

`Request::Attach` upgrades a connection: one `AttachEvent::Snapshot` (every agent, as of that instant), then a stream:

| Event | When |
|-------|------|
| `AgentRegistered` / `AgentRemoved` | A `start`/`spawn`/`drop` mutates the registry — pushed from `AgentRegistry`'s broadcast channel, not polled |
| `StatusChanged` | Any status push (hook, explicit `overseer status`, exit sweep) |
| `Output` | The **watched** agent's rendered terminal grid — see below |
| `Shutdown` | The daemon is exiting (`overseer shutdown`/`Q`) — treated like the connection dropping |

The same connection accepts `Watch { agent_id }` / `Unwatch` (the TUI watches whichever agent is selected, switching on cursor move; `Watch` sends an immediate grid so switching feels instant), `Write { agent_id, data }` (keystrokes/paste), and `Resize { cols, lines }` (every agent shares one PTY size). `Start`/`Spawn`/`Drop`/`Status`/`List`/`Agent` stay one-shot, additive to the attach connection.

`Output` streams a rendered `GridSnapshot` DTO (cells, colors, cursor, the two `TermModes` bits key encoding needs), not raw PTY bytes (see "What to Avoid" for why), whenever `SessionManager`'s per-agent generation counter says the watched agent's screen changed. `ui::term_pane::paint_grid_snapshot` paints it directly.

Workspace-drop is `Request::TuiDrop`, distinct from `Request::Drop`, sent only by the TUI's `d`/`D` handling — a safety rail against *accidental* misuse (a script or supervising agent calling the documented CLI taking out a whole workspace tree it doesn't own), not an authorization boundary between agents — see "Security" below for what actually is.

`--mock` never touches any of this: fully in-process, its own throwaway socket, seeded demo data, no real PTYs.

### Security

Every agent under one daemon fully trusts every other agent. `agent_id` is a plain, caller-supplied field on every IPC request — never checked against the identity of the connection sending it, because the protocol has no notion of connection identity (no `SO_PEERCRED` check, no per-agent auth handshake). Any agent holding `OVERSEER_SOCKET` can `Write` into any other agent's PTY (including the workspace's own shell — real cross-agent code execution, not a UI nuisance), forge any agent's `Status`, `Drop` any non-workspace agent, or `Shutdown` the whole daemon. `overseer prompt` is a documented, scriptable path to that same `Write` capability (attach + two writes under the hood) — not a new one; the underlying wire protocol already let any agent write into any other agent's PTY before this command existed. This is a deliberate, accepted trade-off, not an oversight (see `.specs/SECURITY-AUDIT.md` F4) — the isolation Overseer provides is organizational (a tree you can see and `drop`), not a sandbox between siblings. **Do not run mutually-distrusting agents under one daemon.**

Cross-user isolation relies on the socket directory being owner-only (`0700`, validated rather than blindly chmod'd — a pre-existing dir at the predictable `/tmp/overseer-$UID` fallback path is checked for real-directory/ownership/mode before being trusted) and the socket node itself being `0600`.

### Agent Adapter Trait

Two surfaces: **install** (install-time, user-level artifacts) and **launch** (runtime command + env). Both pure — they return data; the `install` / `start` handlers do the I/O.

```rust
pub trait AgentAdapter: Send + Sync {
    fn name(&self) -> &str;

    // install (install-time): files written at the USER level, once
    fn user_config_dir(&self) -> Option<PathBuf>;      // e.g. ~/.claude
    fn install_files(&self) -> Vec<InstalledFile>;     // skill(s) + status hooks
    fn legacy_paths(&self) -> Vec<PathBuf> { vec![] }  // superseded layout to delete

    // launch (runtime): how to start one agent session
    fn spawn_command(&self, ctx: &LaunchContext) -> Command;
    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String>;
}
```

`InstalledFile` is a `(path, content, merge_strategy)` triple, one of three `MergeStrategy` variants:
- `Overwrite` — Overseer owns the file outright (a skill, a plugin/extension script).
- `JsonMerge` — Claude-specific: merges into `~/.claude/settings.json`'s `hooks` object-of-arrays, tagging Overseer's entries with `_overseer: true` so uninstall removes exactly those. Also recognizes and cleans up pre-tagging-era Overseer hooks, via two independent legacy signatures: entries containing both `OVERSEER_AGENT_ID` and `Follow the overseer` (the old SessionStart printf text), or entries invoking our own binary's `status` subcommand at all (`overseer status `) — the latter catches untagged `PostToolUse`/`Stop` duplicates from before `--from-hook` classification existed, found live racing a correctly-tagged push and intermittently winning (a bare `status done` on every `Stop` could force a perfectly healthy workspace to `done`). Both are treated as ours so upgrading from an install that predates the tag converges instead of leaving orphaned duplicates behind (see `is_overseer_entry` in `src/settings.rs`).
- `JsonArrayMerge { key, entries }` — generic: merges/removes string `entries` into/from a named top-level JSON array (opencode's `instructions`); uninstall removes exactly `entries` back out.

`legacy_paths()` names a previous install layout to delete outright rather than leave to rot. Nothing is ever written into the user's repo, for any adapter.

Adding a fourth adapter is a repeatable recipe: `.claude/skills/adding-harness-support/SKILL.md` walks through it, including a "verify against the installed binary, not the docs" gate. (`aider` appears elsewhere in this doc purely as a config-shape example — no `AgentAdapter` impl, not a real launch target.)

### Agent Awareness

Injected env vars per session (the *only* thing Overseer injects at launch):
- `OVERSEER_SOCKET` — Unix socket path
- `OVERSEER_AGENT_ID` — UUID
- `OVERSEER_ROLE` — `root` (the wire value for a workspace) | `child`
- `OVERSEER_PARENT_ID` — parent UUID (absent for a workspace)
- `OVERSEER_REPO` — repository name
- `OVERSEER_TASK` — the child's assignment, verbatim (children only; absent for a workspace). Also delivered as the child's initial prompt — the env var just lets it re-read the assignment mid-session.

Role behavior lives in **user-level content installed by `overseer install`** (a skill, a plain instructions file — whatever the harness itself loads), matched to `$OVERSEER_ROLE`:
- Workspaces: may spawn children via `overseer spawn --name "<short-kebab-name>" --task "<full, self-contained task description>" [--adapter claude|opencode|pi]`. Cross-harness spawning is supported — a claude workspace may spawn an opencode or pi child and vice versa.
- Child agents: spawning is not permitted; the agent sets up its own branch/worktree, does the task, and reports completion explicitly (`overseer status done`) — never inferred.

Three harnesses, three status-wiring mechanisms — each verified against the installed binary, not just its docs:

**Claude Code** — user-level `~/.claude/settings.json` hooks (shared across sessions, no-op outside Overseer, all passing `--from-hook`, which reads the Claude-specific hook-payload JSON from stdin):

| Hook | Pushes | Why |
|------|--------|-----|
| `SessionStart` | `idle` for a workspace, `running` for a child (branches on `$OVERSEER_ROLE`) | A workspace is a bare shell the human ran `claude` inside themselves — freshly started, it's waiting on the human to type a prompt, so `running` here would be misleading before the first prompt is even submitted; `UserPromptSubmit` is what flips it. A child's task is delivered as its initial prompt, so it's already working the instant it launches (registered `Spawning` — see "Spawn Data Flow") — this is what flips it to `running`. Both branches self-identify the adapter and print a pointer at the role-specific skill. |
| `UserPromptSubmit` | `running` | The point real work actually begins — covers both a session's first prompt and the user prompting again after the agent had gone `idle`. |
| `PostToolUse` | `running` | Actively working. |
| `Stop` | `idle` | Finished responding — **not** done. No hook ever pushes `done`; the only paths there are an explicit `overseer status done` from the agent, or a clean PTY exit (see Cleanup below). |
| `Notification` | `blocked`, downgraded to `idle` for the ~60s idle nag | Fires for both a real permission prompt and the nag; `--from-hook` classifies which via the payload's message text. |

**opencode** — a plugin at `~/.config/opencode/plugin/overseer.js`, auto-loaded (opencode scans that directory itself, no `opencode.jsonc` entry needed). Role instructions (`overseer-root.md`/`overseer-child.md`) merge into `opencode.jsonc`'s `instructions` array unconditionally — each file's own "only applies when `$OVERSEER_ROLE=...`" opening line makes loading both, every session, harmless:

| opencode event | Pushes | Why |
|------|--------|-----|
| `session.created` | `idle` for a workspace, `running` for a child (branches on `$OVERSEER_ROLE`) | A workspace is a bare shell waiting on the human to prompt it; a child's task is already its initial prompt, so it's working the instant it launches. Same reasoning as Claude's `SessionStart`. |
| `session.status` (`status.type === "busy"`) | `running` | The actual "agent is actively working" signal — confirmed live; better grounded than proxying through `tool.execute.after`, which only fires around tool calls. |
| `session.idle` | `idle` | Finished responding. |
| `permission.ask` *(a separate hook, not the generic event bus)* | `blocked` | The moment a permission prompt appears. Never sets the hook's own `output.status` — Overseer only observes, the human still decides. |
| `permission.replied` | `running` | The prompt resolved either way; work resumes. |
| `session.error` | *(nothing)* | The exit watcher owns `error`, not a lifecycle push. |

**pi** — an extension loaded via `pi --extension <absolute-path>` at spawn time (bypasses pi's own package manager/`settings.json` entirely, so install/uninstall is just "write/delete one file"). Role instructions are selected **per role** at spawn time via `--append-system-prompt <path>`, so only the correct doc is ever loaded:

| pi event | Pushes | Why |
|------|--------|-----|
| `session_start` | `idle` for a workspace, `running` for a child (branches on `$OVERSEER_ROLE`) | Mirrors Claude's `SessionStart`: a workspace is waiting on the human to prompt it; a child's task is already its initial prompt, so it's working the instant it launches. |
| `agent_start` | `running` | A turn begins. |
| `agent_end` | `idle` | A turn ends. |
| `session_shutdown` | *(nothing)* | The exit watcher owns `error`. |

**pi never pushes `blocked`** — no permission-request event exists in its `ExtensionEvent` union (permission gates are opt-in extensions in pi, not part of the base install). Documented as a caveat in `pi_root.md`, not faked with a different event.

Status meanings: `spawning` (registered, launching) → `running` (working) → `idle` (finished responding) / `blocked` (needs you) → `done` or `error` (see Cleanup).

Every agent also carries `status_secs`: seconds held in its *current* status, reset only on an actual status change. Visible via `overseer list`/`overseer agent`. In the TUI, tree rows show it for `blocked`/`idle` only (`blocked 2m`); the detail pane always shows it under `status:`.

### Attention Surfacing

A `blocked` (or, if configured, `idle`) agent reaches you two ways beyond the tree's own `!` badge, both edge-triggered (fire once on the transition, not on a repeated push):

- **Terminal bell.** `\x07` to the TUI's own stdout on any `→blocked` transition — works everywhere, including over ssh. On by default unless `[notify] bell = false`.
- **Desktop notification.** `osascript`/`notify-send`, off by default (`[notify] mode = "off"`). `"blocked"` fires on `→blocked` only; `"blocked+idle"` also fires on `→idle`.

Both are driven by one pure diff (`notify::status_transitions`) comparing each frame's tree against the last — identical for `--mock` and daemon-attached. Config in `[notify]` (see Config below). Out of scope: a supervision loop that auto-re-prompts an idle child — this surfaces, a human (or the workspace reading `overseer list`) decides.

### Workspace

Overseer does **not** manage workspaces. A parent runs in the repo's existing checkout; a child sets up its own git worktree/branch — agents already know how to do this. Overseer never runs `git worktree`, creates branches, or merges. Integrating an agent's branch is the user's call.

### Cleanup

Dropping an agent kills its PTY and deregisters it — Overseer doesn't delete branches or worktrees (it didn't create them). Recursive drop is depth-first, children before parent. Workspaces can't be dropped via IPC, only the TUI (`Request::TuiDrop`).

A PTY exiting on its own (not via `drop`) never removes the row: a background watcher maps the exit code onto `done` (clean exit, code 0) or `error` (non-zero/signal), and the agent stays visible for review before an explicit `drop`.

**Quitting the TUI is a detach, not a kill.** `q`/`Ctrl-C` closes the attach connection immediately, no confirm — the daemon and every agent it tracks are unaffected. A later `overseer` launch reattaches and recovers the full tree and each agent's terminal content.

**`overseer shutdown`** (CLI) or **`Q`** (TUI, with a confirm) is the actual kill switch: recursive-drops every workspace, then the daemon exits. Dropping the last agent does *not* shut the daemon down on its own. **`overseer kill`** is the fallback for when `shutdown` can't even get a response — see "Daemon + Attach Protocol" above for what it does and why a plain `kill -9` on the daemon pid alone wouldn't be enough.

**Daemon death is total — for the graceful paths.** No on-disk state file, and `shutdown`/`Q` recursively drop (and thus `SIGKILL`) every agent PTY explicitly before the daemon exits, so nothing outlives it: a fresh daemon always starts from an empty tree (same contract a `tmux` server has with its panes). This is *not* an automatic consequence of the daemon process itself dying, though — a bare `kill -9` on the daemon pid (or a crash) leaves every agent PTY running as an orphan, since each is `setsid()`'d into its own session and the daemon isn't its process-group leader. `overseer kill` accounts for this by finding and killing those orphans too, rather than assuming a plain signal-kill of the daemon takes them with it.

### TUI Layout

```
┌───────────────────────────┬─────────────────────────────────────────┐
│ WORKSPACES                │                                         │
│ ◌ overseer            idle│   the selected agent's live grid,       │
│   ├ ⠸ auth-module 8%      │   painted directly into this same       │
│   ├ ! tests blocked 2m 91%│   ratatui frame by ui/term_pane —       │
│   └ ✓ docs             62%│   real color, real interaction          │
│ ! refactor-api  blocked 5m│   once focused (Ctrl-l)                 │
├───────────────────────────┤                                         │
│ task:   auth-module       │                                         │
│ repo:   overseer          │                                         │
│ branch: ovsr/a            │                                         │
│ status: running           │                                         │
│ since:  4m                │                                         │
│ ctx:    8%  █░░░░░░░      │                                         │
└───────────────────────────┴─────────────────────────────────────────┘
 OVERSEER   1/6 running · 2 blocked   j/k nav  Ctrl-l/↵ jump in  n/s spawn  d/D drop  / search  q quit  ? help
```

Both columns are ratatui-rendered in one process, one window — `ui::render` does its own ~25/75 horizontal split every frame; no second pane, no multiplexer. `ui::term_pane` paints the selected agent's terminal cell-by-cell into that half from a `GridSnapshot` — the only render currency, in both `--mock` and daemon-attached modes (`App::pane_grid` asks `SessionManager::grid_snapshot` directly in `--mock`, or returns the last streamed snapshot otherwise). `ui/` never touches `alacritty_terminal`.

Each tree row right-aligns `<status> <pct>%` in dim gray (red/bold for `blocked`); the name truncates with `…` (`format_tree_row`/`truncate_with_ellipsis`). Status bar: "`N running`", or "`N running · M blocked`" once any agent needs attention.

Status badges: `●` running · `!` blocked (needs you) · `◌` idle · `✓` done · `✗` error · `…` spawning

**Keybinding house style: nvim.** `j`/`k` within a list, `Ctrl-h`/`Ctrl-l` between panes. New bindings extend this vocabulary, never a parallel one or a prefix-key/chord model. Keys an agent's own TUI relies on (e.g. `Ctrl-j` = Claude Code's insert-newline) pass through to a focused pane untouched — `Ctrl-h` is the *only* key Overseer intercepts while focused (real Backspace still works: terminals send `DEL`, not `^H`).

Every tree-focus action below is remappable via `[keybindings]`. Fixed regardless of config: `Ctrl-h` (stealing it would take a key an agent's own TUI needs) and the scrollback keys (next section). `Enter`/`o`/`Ctrl-C` also stay fixed as extra aliases for `jump_in`/`quit` even if those actions are remapped.

| Key (default) | Action |
|-----|--------|
| `j` / `k` | Navigate agent tree (tree focus only) |
| Left-click the tree | Focus the tree, selecting the row clicked (if any) — works while a pane is focused too, since clicks never forward to agents (no mouse forwarding exists; additive, nothing is mouse-only) |
| Left-click the pane | Jump in, same as `Enter`/`o` (alive check + scroll reset included); a click on an *already-focused* pane is a no-op so it never resets a wheel-scrolled position |
| `<space>` | Fold/unfold the selected agent's children |
| `Ctrl-l` / `Enter` / `o` | Jump in — moves keyboard focus into the selected agent's pane, if it's alive |
| `Ctrl-h` | From inside a focused pane, jump back out to the tree — the only key a pane intercepts; everything else, Ctrl-c included, forwards to the agent |
| `n` | Spawn a workspace in a chosen repo (default cwd). If any adapter is installed, first opens a picker (`j`/`k`, `Enter` to confirm, `Esc` to cancel) — every `overseer install`ed adapter plus "bare terminal" — then the repo-path prompt; skips straight to the prompt (bare shell) if nothing is installed |
| `s` | Spawn child under selected agent (adapter-launched, same as before) |
| `d` | Drop selected agent (confirm prompt) |
| `D` | Recursive drop — agent + all children (confirm prompt) |
| `q` / `Ctrl-C` | Quit immediately, no confirm — detaches, never kills any agent or the daemon (see Cleanup) |
| `Q` | The kill switch: recursive-drop every agent and exit the daemon (confirm prompt) |
| `/` | Fuzzy search the tree by name (see "Search" below) |
| `?` | Open the live keybinding reference — any key closes it |
| `Ctrl-u` / `Ctrl-d` | Scroll the selected agent's pane up/down half a page (tree focus only, fixed — see "Scrollback" below) |
| `Ctrl-y` / `Ctrl-e` | Scroll one line up/down (nvim semantics: `e` = down; fixed) |
| `G` | Jump the selected agent's pane back to the live bottom (fixed) |
| mouse wheel (over pane) | Scroll the selected agent's pane — works in tree focus *and* pane focus, unlike the keys above (see "Scrollback" below) |

### Search

`/` opens a centered input; as you type, the tree shows only agents whose name fuzzy-matches (`fuzzy_match(query, name) -> Option<u32>`: case-insensitive, in-order subsequence, contiguous runs score higher), plus every ancestor of a match (dimmed, for context). `Enter` moves the *real* cursor to the current selection (or the first match) and closes the prompt; `Esc` closes it without moving anything.

### Help

`?` opens a centered popup listing every binding — generated from the live `Keybindings` struct (`ui::help_rows`), never a hardcoded string. Includes the fixed keys too (`Enter`/`o`, `Ctrl-C`, `Ctrl-h`), labeled as fixed. Any key closes it.

### Scrollback

While tree-focused, `Ctrl-u`/`Ctrl-d`/`Ctrl-y`/`Ctrl-e`/`G` scroll the *selected* agent's pane — a read-only preview in that state, so these never collide with a real agent TUI's own use of the same keys (readline's `Ctrl-u` kill-line). These keys stay off-limits once a pane is focused (`Ctrl-h` remains the only key a focused pane intercepts) — but the mouse wheel scrolls the pane in *both* states, focused included. Scroll it over the pane (`MouseEventKind::ScrollUp`/`ScrollDown`, `handle_mouse_event` in `tui.rs`) and it moves the selected agent's history, 3 lines per notch, whether you're previewing from the tree or jumped in. This isn't stealing anything from the agent's own TUI: `EnableMouseCapture` is armed on Overseer's own controlling terminal, and each agent runs in its own PTY (`session::pty`) that only ever receives bytes Overseer explicitly writes to it — no mouse forwarding exists, so a scroll event was never reaching the agent in the first place. Scrolling outside the pane's rect (e.g. over the tree) is ignored.

The scrolled offset resets to the live bottom on cursor move and on jump-in, so you never *start* interacting with a pane mid-scroll — but once focused, scrolling back down (mouse wheel) is what gets you back to the tail, since `G` forwards straight to the agent while focused. The pane border shows the state throughout: `" agent [scrolled ↑N — G to follow] "` while tree-focused, `" agent [FOCUSED, scrolled ↑N — scroll to follow] "` while focused and scrolled, reverting to `" agent "` / `" agent [FOCUSED — Ctrl-h to leave] "` at the live bottom.

Scrolling happens where the real terminal state lives — the daemon (`SessionManager::scroll_display`/`scroll_to_bottom`/`display_offset`). A daemon-attached TUI sends `Request::Scroll { delta }` / `Request::ScrollToBottom` on the attach connection (no `agent_id` — always the connection's watched agent), and gets back a fresh `GridSnapshot` immediately (scrolling doesn't touch the PTY, so it never bumps the generation counter the normal output poll relies on). `--mock` calls `SessionManager` directly, no round trip.

**Mouse wheel not working, especially in macOS Terminal.app.** Some terminal emulators — Terminal.app is the known offender — don't send a real xterm mouse-wheel report even with `EnableMouseCapture` armed; instead they translate wheel motion into synthetic `Up`/`Down` key events. Those are byte-for-byte indistinguishable from a real arrow-key press by the time Overseer sees them, and real arrow keys are exactly what many agents' own TUIs rely on for history/menu navigation — so Overseer can't bind Up/Down to scroll without silently breaking that for every agent, on every terminal, whenever a pane is focused. There is no keyboard key that safely substitutes for the mouse wheel while a pane is focused (`Ctrl-h` stays the *only* key a focused pane intercepts — see `tui::tests::arrow_keys_forward_to_the_agent_while_pane_is_focused_not_scroll` for the regression test pinning this decision). If your wheel doesn't seem to scroll a focused pane: back out to tree focus (`Ctrl-h`) first — `Ctrl-u`/`Ctrl-d`/`Ctrl-y`/`Ctrl-e`/`G` are real Ctrl-key sequences, not wheel-translated ones, so they work regardless of this gap — or switch to a terminal with real xterm mouse reporting (iTerm2, kitty, Alacritty, WezTerm all report genuine wheel events).

### Spawn Data Flow

```
Workspace runs: overseer spawn --name "write-tests" --task "write tests" --adapter claude

IPC server (spawn_blocking):
  → name = name.filter(non-blank).unwrap_or(task) = "write-tests"  // task text is the fallback only
  → AgentRegistry::register(child, name, parent=caller, status=Spawning) // rejects if caller is a child
  → adapter = adapter_for(name); command/extra_args resolved from config.adapters[name]
  → LaunchContext.task = "write tests"
  → SessionManager::launch(agent_id, cwd=repo, adapter.spawn_command(ctx),
                           adapter.env_inject(ctx))
      spawn_command: <command> <extra_args...> "write tests"   // task is the final positional arg
      env_inject:    ...identity vars..., OVERSEER_TASK="write tests"
  → replies: {"agent_id": "..."}

TUI re-renders with the new child visible under the parent, labeled "write-tests"
in the tree — short and recognizable even though the task text (the child's
actual initial prompt) can run to a full paragraph. It starts working
immediately instead of sitting at a bare prompt. The child sets up its own
branch/worktree on startup (`git worktree add ../<repo>-<slug> -b ovsr/<slug>`,
per the overseer-child skill's worked example), and its own SessionStart hook
flips it from Spawning to Running moments later.
```

`overseer start` (launch a workspace) is a *different* path — always no task, and by default no adapter either: it registers `role=root`, `status=idle`, names the node after the repo, and launches a bare shell (`$SHELL`) instead of `adapter.spawn_command(ctx)`. Whatever you run inside that shell (e.g. `claude`) inherits the injected identity env vars from the PTY itself and reports its own status via the same push hooks — Overseer never detects or launches it. The cwd doesn't have to be a git repo — git failure just falls back to the directory's own basename as the name and an empty branch, rather than rejecting the workspace outright; only a nonexistent/non-directory cwd is rejected.

`Request::Start` also accepts an optional `adapter` (the TUI's `n` picker's second field, once step 1 finds at least one `overseer_installed()` adapter). When set, `spawn_root` takes the other branch — `spawn_root_with_adapter` — which launches the *shell* (`resolve_shell()`, no args, `adapter.env_inject(ctx)` as env) exactly like the bare-shell workspace does, then builds the adapter's launch command via `adapter.spawn_command(ctx)` (role `Root`, no parent, `task=""`) and types it into that PTY as a followup write ending in `\n`, so it runs immediately without the harness ever being the PTY's own child process. It registers `status=Idle`, same as a bare-shell workspace — the PTY child is a live shell, byte-identical to a human typing the command into today's bare shell themselves, and the harness's own `SessionStart`-equivalent hook flips status from there, not this launch step. (This shell indirection is deliberate: exec'ing the harness directly as the PTY child meant exiting it killed the whole PTY and froze the pane on `[exited]`; going through a shell first means exiting the harness only drops back to a live shell prompt, and only exiting *that* ends the workspace.) The adapter label is still the chosen adapter, not `shell` — only the launched PTY child changed, not what's shown for it. No `SessionStart`/hook changes were needed for any of this: every adapter's own install hook already branches on `$OVERSEER_ROLE` (`idle` for a workspace, `running` for a child) regardless of who typed the launch command.

---

## Config

`~/.config/overseer/config.toml`. **Implemented:** `[defaults]`, `[adapters.*]`, `[notify]`, `[keybindings]`, `[theme]` — all below. `[defaults]`/`[adapters.*]` load once at daemon/mock startup; `[notify]`/`[keybindings]`/`[theme]` load independently in the TUI process, since they're properties of *your* terminal, not the daemon's. Missing/invalid file falls back to the built-in default; a bad *value* for one field warns on stderr and keeps that field's own default — never a hard error.

```toml
[defaults]
adapter = "claude"

[adapters.claude]
command = "claude"
extra_args = ["--dangerously-skip-permissions"]

[adapters.opencode]
command = "opencode"
extra_args = []

[adapters.pi]
command = "pi"
extra_args = []

[notify]
bell = true      # terminal BEL on a →blocked transition (default on — inert unless your terminal makes it loud)
mode = "off"     # desktop notifications: "off" (default) | "blocked" | "blocked+idle"

[keybindings]     # tree-focus bindings only, all optional — see the keybinding table above for defaults
spawn_root = "n"
spawn_child = "s"
search = "/"
help = "?"
# ...every other tree-focus action is remappable the same way.

[theme]           # status + chrome colors only — named ratatui colors or #rrggbb
running = "green"
blocked = "red"
idle = "dark_gray"
done = "blue"
error = "red"
spawning = "cyan"
border_focused = "yellow"
border = "dark_gray"
```

A child spawn resolves `command`/`extra_args` from `config.adapters[name]`, not the adapter name itself — lets flags like `--dangerously-skip-permissions` reach the process, and lets a user point "claude" at a custom binary. A name with no entry in `config.adapters` is the same `UnknownAdapter` error as one with no `AgentAdapter` impl (e.g. `aider`, a config-shape example only — see "Agent Adapter Trait").

`[notify]`: every channel independently switchable off. `bell` defaults **on**; `mode` defaults **off** (the louder, opt-in channel). `"blocked+idle"` also notifies on `→idle`.

`[keybindings]`: a key is `j`/`D`/`/` (single char, case-sensitive) or `ctrl-<char>` (case-insensitive on the letter). Two actions bound to the same key is a startup warning, not an error — the later declaration wins. `Ctrl-h` and the scrollback keys are **not** in this table — see the house-style note under "TUI Layout." Every binding reflects live in the `?` popup.

`[theme]`: colors only — `Blocked`'s bold weight is fixed. `Theme::default()` is asserted (in a test) to reproduce Overseer's pre-`[theme]` colors exactly, so this section can't silently change anyone's look who never touches it.

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
| Daemon lifecycle | `libc` — `setsid` (detach from the controlling terminal), `flock` (single-daemon lockfile), `getuid` (default socket path) |

**Runtime dependencies:** `git`. Standard on macOS/Linux. (No `tmux` — Overseer owns its own PTYs.)

---

## Distribution

Single statically-linked binary. Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`. Homebrew tap: `nikita-ivanov/tap/overseer`. GitHub Actions handles cross-compile, release assets, and tap formula updates.

---

## Limits

Measured, not assumed — `scripts/stress.sh [N] [lines_per_sec]` spawns 1 workspace + N chatty children (status pushes + one high-output pane, default 400 lines/sec) and watches the streaming pane for the entire load window (a watched pane must be exercised throughout, not just after — an earlier version of this script that skipped that missed a real regression).

Tested at **N=30** (target fleet size) and **N=50** (headroom), release build: daemon RSS 150-250MB (under the 500MB budget, dominated by scrollback buffers not fleet size), daemon CPU under ~17% of one core with two simultaneous watchers, spawn latency ~10-30ms mean, status-push round-trip a few ms to tens of ms (0 pushes lost), write→`Output` round trip in the tens of milliseconds.

**One structural caveat:** every agent's PTY is resized to one shared rect (`SessionManager::resize_all`) — O(agents) work for the resizing connection (doesn't stall others; runs on `spawn_blocking`). Revisit only if a larger-N measurement shows it mattering. Rerun `scripts/stress.sh` after touching the daemon's hot paths.

---

## Specs & Planning Docs

Implementation plans and research notes live in **`.specs/`**, which is **gitignored** — local working documents, not part of the distributed repo. Never commit one to the repo root, and never reference a spec from code or committed docs — once a phase ships, the code/AGENTS.md must stand on their own.

---

## Best Practices

- **IPC is the only shared channel.** Agent ↔ overseer communication always goes through the Unix socket — never shared in-process state from an agent context.
- **The "no grandchildren" rule lives in the IPC server,** in the `spawn` handler. Not the TUI, not adapters. One place, always enforced.
- **`alacritty_terminal` lives only in `session/pty.rs`.** `SessionManager`'s public method set — `launch`, `kill`, `write`, `resize_all`, `is_alive`, `scroll_display`, `scroll_to_bottom`, `display_offset`, `grid_snapshot`, `term_modes`, `generation`, `drain_exits` — is the entire terminal-backend contract; every signature uses only `GridSnapshot`/`TermModes`/std types. Swapping the backend means rewriting that one file, not chasing leaks through `ui/` and `ipc/`.
- **Parse functions are pure.** E.g. `parse_session_line` takes a `&str`, returns a value — no process spawning, no I/O. Trivially testable.
- **`AgentNode` is a data model, not a handle.** No PTY ownership. Session handles live in `SessionManager`, keyed by `AgentId`. Overseer holds no worktree state — that's the agent's.
- **Status is push, not pull.** Agent hooks POST status changes; the daemon POSTs registry/output events to attach clients the same way. Overseer never infers status from PTY output; the TUI never polls for tree state. Each push is its own independent connection with no ordering guarantee against any other (`ipc::server` spawns a task per connection) — `Request::Status`'s `pushed_at` timestamp and `AgentRegistry::set_status`'s staleness guard are what keep a late-arriving-but-earlier-fired push from clobbering a fresher one.
- **`ui/` is a render layer only.** No business logic. State mutations go through `App` / `AgentTree` / `SessionManager`.
- **One code path per request, regardless of backend.** `App::dispatch`/`with_tree`/`write_input`/etc. branch on `Backend::{Mock, Daemon}` in exactly one place (`app.rs`) — `tui.rs`/`ui/` call the same methods either way (bar the one `pane_grid` lookup in `run_app`, which is `ui`-shape glue, not business logic).

## What to Avoid

- **No MCP transport.** Unix socket + hooks is intentional — no token overhead, no plugin registry approval, works locally out of the box.
- **Don't let children spawn children.** A hard server-side constraint, not a UI hint — a child calling `spawn` is rejected, full stop.
- **Don't hardcode adapter binary paths.** Always resolve through adapter config so users can point to a custom binary or wrapper.
- **Don't add agent status polling.** If hooks aren't firing, fix `overseer install`, not a background poller. Same for the TUI — missing tree updates get fixed in the registry's broadcast or the attach connection.
- **Don't reimplement git.** No worktree creation, branching, or merging. Agents own their isolation; Overseer's only git use is read-only display info.
- **Don't write into the user's repo.** All agent config is installed at the user level by `overseer install`. Launch injects env only.
- **Don't skip the confirm prompt for `d`/`D`/`Q`.** Killing a session (or the whole daemon) interrupts in-flight work.
- **Don't make quitting kill agents.** `q`/`Ctrl-C` is a detach, never touches a session or the daemon. `d`/`D` kills one agent, `Q` kills everything plus the daemon; both confirm.
- **Don't add a second way to *gracefully* end the daemon process.** `Request::Shutdown` asks the accept loop to stop and lets `main` return — no `std::process::exit`. A response still in flight when the process exits is a real bug (once caused by `tokio::sync::Notify::notify_waiters` losing a wake under this exact race — use `notify_one`, which stores a permit, for any future stop-signal here). `overseer kill` (`src/kill.rs`) doesn't violate this: it always tries this exact same `Request::Shutdown` path first, and only reaches for `SIGKILL` once that request has been given a real chance and failed to get a response — it's a forceful fallback for an unresponsive daemon, not a second graceful exit path racing this one.
- **Don't add a `Request::Drop`-with-a-flag for workspace drops.** Workspace-allowed drop is `Request::TuiDrop`, a distinct wire request only the TUI's key handling constructs — a caller-supplied bool would let any script opt out of the restriction it exists to enforce.
- **Don't assume `alacritty_terminal` exposes raw PTY bytes.** It doesn't, not without reimplementing its mio/signalfd event loop — that's why the attach protocol streams rendered `GridSnapshot`s instead. Re-verify against the installed version before retrying; a future public tap would be a contained change to `session::pty` + `ipc::protocol`, not a redesign.
