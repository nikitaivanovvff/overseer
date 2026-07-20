# Overseer

**An IDE for agents.** A terminal-native TUI for observing and steering a fleet of parallel AI coding agents from a single window — instead of juggling five terminal tabs. Built in Rust. Nvim-aesthetic; a single ordinary alt-screen app with no bundled multiplexer — each agent is a PTY Overseer owns directly, emulated in-process and rendered straight into the same ratatui frame — with a Unix socket IPC layer that gives agents a lightweight API to register, report status, and spawn children — without MCP overhead.

The agents are already smart. Overseer does **not** reimplement what they do — it does not manage git worktrees, branches, or merges; agents handle their own isolation. Overseer is the observability, routing, and approval surface on top: see every agent's state at a glance, jump into any one to approve or intervene, or leave the parent to supervise its own children.

The usual shape is **one workspace per repository**. `n` spawns a workspace immediately, no prompt: a bare shell in wherever Overseer itself is currently running (this process's own cwd) — no adapter is launched on your behalf. (To open a workspace for a *different* repo, use `overseer start --cwd <path>` from a terminal instead; the TUI itself only ever spawns at cwd.) The row appears immediately, named after the repo, and its status flips `idle` → `running` the moment your agent starts reporting via its hooks once you run one yourself (its own `idle` sticks until then). From there you talk to it in natural language and it fans work out into child agents, each in its own PTY (auto-launched via the configured adapter), surfacing as its own row. Drop into any child for approval or a nudge, or let the parent check on them periodically.

The hierarchy is intentionally capped at **depth 3**: a workspace (depth 1) can spawn children (depth 2), and those children can spawn visible leaf agents (depth 3). Depth-3 agents cannot spawn. Every parent also has a configurable direct-child cap (`[defaults] max_children`, default 8), keeping the tree readable and preventing runaway PTY/token costs. A **child's** node name is the short label it was given at spawn (`--name`) — falling back to its task text verbatim if none was given, since the task can be a whole paragraph and a name shouldn't have to be. A **workspace's** node name is the **repo name** — there's no task description, since a workspace is just a bare shell with nothing running yet, same as a human just opening it. Each tree item uses two lines: badge/name/status on the first, then branch plus the verified model name (or full adapter name when no model is known) on a dim secondary line. A bare `shell` workspace contributes no harness metadata until a real harness reports in.

---

## Mental Model

```
You (the user)
  └─ Overseer TUI                                                        ← one window, the whole fleet
       └─ Workspace  (name: overseer, adapter: shell → claude once you run it) ← bare shell in the repo checkout
            ├─ Child Agent A  (task: auth-module, adapter: claude)       ← own PTY, own branch
            │    └─ Lookup Agent (depth 3, visible leaf)                 ← may not spawn further
            └─ Child Agent B  (task: write-tests, adapter: opencode)     ← own PTY, own branch
```

You spawn the workspace — a bare shell you run your own agent inside — and talk to it directly; it fans out children on your behalf. Every agent (workspace or child) is a PTY Overseer launched and a row you can jump into. Branch/worktree isolation is the **agent's** job, not Overseer's.

Agents know their role (`root` (the wire value for a workspace) or `child`) via injected env vars and a **user-level skill** installed once with `overseer install <agent>`. Claude Code hooks POST lifecycle events to the Unix socket to report status — zero agent context tokens, nothing written into your repo.

The registry and every agent's PTY live in a **daemon** process, not the TUI — an `overseer` launch attaches as a client (auto-spawning one if it isn't running). Quitting the TUI detaches; the daemon and every agent keep running, and a later launch reattaches to exactly what was there before (see "Daemon + Attach Protocol").

---

## Glossary

Canonical names for the things this doc (and conversation about Overseer) refers to. When a user or doc says one of these, this is what it means — see the "TUI Layout" diagram below for where each sits on screen.

| Term | Also called | What it is | Code anchor |
|------|-------------|------------|-------------|
| **Agent structure** pane | tree, agent tree, sidebar, workspaces pane | The left-column list of workspaces with their agents nested under them, titled `WORKSPACES`. Selection/navigation (`j`/`k`), folds, and search all act here. | `ui::render_agent_tree`, `RenderLayout::tree_rect`/`tree_rows` |
| **Agent pane** | pane, terminal pane, live pane | The right column: the selected agent's live terminal, painted cell-by-cell from a `GridSnapshot`. Read-only preview while tree-focused; interactive once jumped in. | `ui::term_pane::render_term_pane`, `RenderLayout::pane_rect` |
| **Details** pane | detail panel | The block under the agent structure showing the selected agent's `task`/`name`, `repo`, `branch`, `status`, `since`, and attention. Harness capabilities (lifecycle/permissions/limits/context) are computed but not rendered here — see `AdapterCapabilities` below. | `ui::render_agent_detail` |
| **Footer** | status bar | The bottom line: `OVERSEER` brand, fleet summary (`N running · M blocked`), hotkey hints, and transient error messages. `d`/`D`/`Q` confirmation is a centered modal, not footer text — see `ui::render_confirm_modal`. | `ui::render_status_bar` |
| **Workspace** | root — the wire/env value | A top-level agent, one per repo: the shell/harness you talk to directly. Spawned with `n`/`overseer start`. | `AgentRole::Root` |
| **Child** | — | A depth-2 or depth-3 agent spawned for one task; depth-2 children may spawn visible depth-3 leaves. | `AgentRole::Child` |
| **Harness** | adapter, agent type | The AI CLI an agent runs (claude or opencode) and the `AgentAdapter` that knows how to install/launch it. | `agent::adapters` |
| **Jump in** | focus the pane | Moving keyboard focus into the agent pane (`Ctrl-l`/`Enter`/`o`/click); every key but `Ctrl-h` then forwards to the agent. | `tui::jump_in`, `Focus::Pane` |
| **Daemon** | — | The background process owning the registry and every PTY; the TUI is a detachable client of it. | `crates/overseer-core/src/daemon.rs` |

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

A Cargo workspace of two crates: `overseer-core` (library — everything
client-agnostic: agent model, sessions, IPC, daemon, config) and `overseer`
(the binary — CLI subcommands, daemon entrypoint, and the TUI). The binary's
name, CLI surface, and behavior are exactly what the pre-workspace single
crate shipped; core is an internal path dependency, never published on its
own.

Module-level maps live next to the code they describe — start at
[ARCHITECTURE.md](ARCHITECTURE.md) (process model, workspace layout, the
"what changes where" guide) and drill into
[crates/overseer-core/ARCHITECTURE.md](crates/overseer-core/ARCHITECTURE.md)
or [crates/overseer/ARCHITECTURE.md](crates/overseer/ARCHITECTURE.md).

---

## Key Components

### IPC Server

Unix domain socket at `$XDG_RUNTIME_DIR/overseer/daemon.sock` (falling back to `/tmp/overseer-$UID/daemon.sock`), owned by the daemon — one stable, per-user socket every repo's TUI and every agent's CLI shares. The only channel agents use to talk to Overseer — no MCP, no HTTP, no polling. `overseer` doubles as the client: each subcommand opens the socket, sends one newline-delimited JSON request, prints the reply, exits.

| Command | Args | Description |
|---------|------|-------------|
| `overseer install` | `<agent> --uninstall?` | Install (or remove) the user-level skill(s) + status hooks for an agent type. Run once at setup, not per launch. |
| `overseer uninstall` | `<agent>` | Remove the user-level skill(s) + status hooks for an agent type. Direct top-level equivalent of `overseer install <agent> --uninstall`, both call the same `install::run_install`. |
| `overseer daemon` | — | Runs the daemon itself: binds the socket, serves requests, streams attach events, watches session exits. Hidden from `--help` — not a user workflow, the TUI spawns one automatically. |
| `overseer start` | `--cwd?` | Register a workspace and launch a bare shell for it in its own PTY (default cwd: current directory). No adapter is launched — run your own agent inside it. `--cwd` must exist and be a directory, but doesn't have to be a git repo: outside one, the workspace is named after the directory itself with an empty (`—` in the TUI) branch, rather than faking a repo/branch pair. |
| `overseer status` | `<status> --message? --attention? --clear-attention?` | Push a lifecycle update plus an optional normalized attention delta for the calling agent. Attention kinds are `permission`, `rate-limit`, `quota-limit`, `billing`, and `provider-error`; `--retry-after` carries only a harness-supplied delay. Lifecycle-only pushes preserve permission attention, successful `running` clears provider-limit attention, and terminal status clears all attention. No-op outside Overseer. Each push carries a client-captured `pushed_at`; `AgentRegistry` drops the whole push when it is older than the newest accepted update, including attention. |
| `overseer spawn` | `--task --name? --adapter?` | Request a child. Rejected if the result would exceed depth 3 or the parent's `max_children` cap. `--task` is the child's entire initial prompt; `--name` is a short, distinct tree-row label (falls back to `--task` verbatim if omitted or blank). |
| `overseer drop` | `<id> --recursive?` | Kill the agent's PTY and deregister it. Overseer does not touch the agent's branch/worktree. Workspaces are rejected here — only the TUI's `d`/`D` (a distinct wire request, see below) can drop one. |
| `overseer shutdown` | — | The kill switch: recursive-drops every workspace, then the daemon process exits. Same request the TUI's `Q` sends after its confirm. Unchanged, no timeout — assumes a healthy daemon; if it can't reach one, use `kill` instead. |
| `overseer kill` | — | Last-resort forceful cleanup for a daemon `shutdown` can't reach — wedged/deadlocked and never replying, or already crashed with a stale socket/lockfile left behind. Tries the exact same graceful `Request::Shutdown` first, bounded by a ~5s timeout (with retries against the narrow lock-acquired-before-socket-bound startup race) — only escalates to `SIGKILL`ing the daemon pid (normally read from the `flock`-guarded `daemon.pid` lockfile, see below; if that pid isn't readable while the lock is nonetheless held, falls back to scanning `ps` for the one process whose command line contains `daemon --socket <this socket>` — exactly one match required, or it refuses to guess) if that doesn't answer in time. Also `SIGKILL`s any orphaned agent PTY processes found as the daemon's direct children (best-effort, via `ps` — see "Daemon death is total" below for why the daemon pid alone isn't enough), then removes the stale socket/lockfile so a fresh daemon can bind cleanly. A separate subcommand rather than added behavior on `shutdown`, so existing callers of `shutdown` see no new destructive-by-default timeout/signal-kill. |
| `overseer list` | — | List all agents |
| `overseer agent` | `<id>` | Get agent detail |
| `overseer prompt` | `<id> --text "<text>"` | Submit `--text` into the agent's PTY as a prompt and press Enter, non-interactively, then exit — the scriptable counterpart to typing into a pane in the TUI. Lets a workspace (or a cron job/script) nudge an idle or blocked child without a real interactive terminal (see "Attention Surfacing" below). Internally opens its own `Attach` connection, discards the initial `Snapshot`, and sends two separate `Write`s (text, then a delayed `\r`) — `Write` is only honored on an attach connection, not the one-shot `dispatch` path, so this isn't a thin wrapper over a single request. |

Identity (`OVERSEER_AGENT_ID`, socket path) comes from injected env, so commands don't pass it explicitly.

### Daemon + Attach Protocol

The daemon owns `AgentRegistry` and `SessionManager`; the TUI is its first, richest client. On startup the TUI connects to the socket, or spawns `overseer daemon` detached (`setsid`, stdio to a log file next to the socket) and retries with backoff. A `flock` lockfile (`daemon.pid`) makes a second daemon on the same socket fail fast instead of racing for the bind.

**`overseer kill`: the forceful fallback for when the graceful path can't reach a daemon at all.** `overseer shutdown`/`Q` depend on a healthy daemon actually processing `Request::Shutdown` — wedged (deadlocked, `kill -STOP`'d) or already-crashed daemons don't. `overseer kill` (`crates/overseer-core/src/kill.rs`) covers that gap: it reads `daemon.pid`'s `flock` state, not just its contents, to tell "alive but unresponsive" (something holds the lock) apart from "already dead" (nothing does, only stale files remain, which get removed so a fresh daemon can bind). For the former, it tries the identical `Request::Shutdown` first, bounded by a short timeout — only once that fails does it `SIGKILL` the daemon pid directly. That pid normally comes straight from `daemon.pid`'s contents, written by `DaemonLock::acquire` the instant it wins the lock (and, critically, *only* after it wins — a losing `acquire` opens the file without truncating and never writes to it, so it can't erase a winner's already-recorded pid; a real 2026-07-11 incident traced back to exactly that ordering bug, wedging a live daemon's own `overseer kill` recovery path against its own pid file). If the lockfile's pid still isn't usable for some other reason (hand-edited, or truncated by something outside Overseer entirely) while the lock is nonetheless held, `overseer kill` falls back to scanning `ps` for the one process whose argv contains `daemon --socket <this exact socket>` — a match on the full argument sequence, not a substring or bare-binary-name search, so a daemon serving a different socket never matches. Zero or multiple matches both refuse to guess. Critically, that alone doesn't reclaim everything: each agent's PTY child calls `setsid()` before exec (own session/process-group, decoupled from the daemon's), so it survives the daemon's death as an orphan rather than dying with it — the daemon pid's own `ppid` links to those children still work though, so `overseer kill` walks `ps` for the daemon's direct children and `SIGKILL`s them too. This is why "Daemon death is total" (Cleanup, below) is a guarantee about the *graceful* paths only (`shutdown`/`Q`, which drop every agent explicitly before the daemon exits) — a forceful `kill -9` of the daemon process alone does not actually take its agents with it, contrary to what that line might suggest in isolation.

`Request::Attach` upgrades a connection: one `AttachEvent::Snapshot` (every agent, as of that instant), then a stream:

`Request::TuiSpawnChild` is the TUI's `s` path: it carries only the selected parent and child name. The daemon inherits the parent's configured harness, launches without an initial task, and registers the child idle for the human to prompt manually. CLI `Request::Spawn` remains task-bearing and unchanged.

| Event | When |
|-------|------|
| `AgentRegistered` / `AgentRemoved` | A `start`/`spawn`/`drop` mutates the registry — pushed from `AgentRegistry`'s broadcast channel, not polled |
| `StatusChanged` | Any status push (hook, explicit `overseer status`, exit sweep) |
| `Output` | The **watched** agent's rendered terminal grid — see below |
| `Shutdown` | The daemon is exiting (`overseer shutdown`/`Q`) — treated like the connection dropping |

The same connection accepts `Watch { agent_id }` / `Unwatch` (the TUI watches whichever agent is selected, switching on cursor move; `Watch` sends an immediate grid so switching feels instant), `Write { agent_id, data }` (keystrokes/paste), and `Resize { cols, lines }` (every agent shares one PTY size). `Start`/`Spawn`/`Drop`/`Status`/`List`/`Agent` stay one-shot, additive to the attach connection.

`Output` streams a rendered `GridSnapshot` DTO (cells, colors, cursor, and the `TermModes` bits focused input encoding needs), not raw PTY bytes (see "What to Avoid" for why), whenever `SessionManager`'s per-agent generation counter says the watched agent's screen changed. Those modes cover application-cursor keys, bracketed paste, and terminal mouse reporting/encoding; `ui::term_pane::paint_grid_snapshot` paints the grid directly.

Workspace-drop is `Request::TuiDrop`, distinct from `Request::Drop`, sent only by the TUI's `d`/`D` handling — a safety rail against *accidental* misuse (a script or supervising agent calling the documented CLI taking out a whole workspace tree it doesn't own), not an authorization boundary between agents — see "Security" below for what actually is.

`--mock` never touches any of this: fully in-process, its own throwaway socket, seeded demo data, no real PTYs.

### Security

Every agent under one daemon fully trusts every other agent. `agent_id` is a plain, caller-supplied field on every IPC request — never checked against the identity of the connection sending it, because the protocol has no notion of connection identity (no `SO_PEERCRED` check, no per-agent auth handshake). Any agent holding `OVERSEER_SOCKET` can `Write` into any other agent's PTY (including the workspace's own shell — real cross-agent code execution, not a UI nuisance), forge any agent's `Status`, `Drop` any non-workspace agent, or `Shutdown` the whole daemon. `overseer prompt` is a documented, scriptable path to that same `Write` capability (attach + two writes under the hood) — not a new one; the underlying wire protocol already let any agent write into any other agent's PTY before this command existed. This is a deliberate, accepted trade-off, not an oversight — the isolation Overseer provides is organizational (a tree you can see and `drop`), not a sandbox between siblings. **Do not run mutually-distrusting agents under one daemon.**

Cross-user isolation relies on the socket directory being owner-only (`0700`, validated rather than blindly chmod'd — a pre-existing dir at the predictable `/tmp/overseer-$UID` fallback path is checked for real-directory/ownership/mode before being trusted) and the socket node itself being `0600`.

### Agent Adapter Trait

Two surfaces: **install** (install-time, user-level artifacts) and **launch** (runtime command + env). Both pure — they return data; the `install` / `start` handlers do the I/O.

```rust
pub trait AgentAdapter: Send + Sync {
    fn capabilities(&self) -> AdapterCapabilities;

    // install (install-time): files written at the USER level, once
    fn user_config_dir(&self) -> Option<PathBuf>;      // e.g. ~/.claude
    fn install_files(&self) -> Vec<InstalledFile>;     // skill(s) + status hooks
    fn legacy_paths(&self) -> Vec<PathBuf> { vec![] }  // superseded layout to delete

    // launch (runtime): how to start one agent session
    fn spawn_command(&self, ctx: &LaunchContext) -> Command;
    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String>;
}
```

`AdapterCapabilities` declares `lifecycle`, `permission_requests`, `provider_limits`, and `context_usage` as `CapabilitySupport::Supported`, `Unsupported { reason }`, or `Experimental { note }`. This is a pure contract for what this Overseer version's installed integration can observe, not a dynamic plugin-health check. Unsupported is an expected, machine-readable result, never an adapter error. It's an internal/CLI-facing model — `overseer list`/`agent` JSON surfaces it (see "Agent Awareness" below), but the TUI's Details pane does not render it (removed 2026-07-15, alongside `context_pct`, rather than showing a per-field matrix most rows would render as all-unsupported before a harness is detected).

`InstalledFile` is a `(path, content, merge_strategy)` triple, one of three `MergeStrategy` variants:
- `Overwrite` — Overseer owns the file outright (a skill, a plugin/extension script).
- `JsonMerge` — Claude-specific: merges into `~/.claude/settings.json`'s `hooks` object-of-arrays, tagging Overseer's entries with `_overseer: true` so uninstall removes exactly those. Also recognizes and cleans up pre-tagging-era Overseer hooks, via two independent legacy signatures: entries containing both `OVERSEER_AGENT_ID` and `Follow the overseer` (the old SessionStart printf text), or entries invoking our own binary's `status` subcommand at all (`overseer status `) — the latter catches untagged `PostToolUse`/`Stop` duplicates from before `--from-hook` classification existed, found live racing a correctly-tagged push and intermittently winning (a bare `status done` on every `Stop` could force a perfectly healthy workspace to `done`). Both are treated as ours so upgrading from an install that predates the tag converges instead of leaving orphaned duplicates behind (see `is_overseer_entry` in `crates/overseer-core/src/settings.rs`).
- `JsonArrayMerge { key, entries }` — generic: merges/removes string `entries` into/from a named top-level JSON array (opencode's `instructions`); uninstall removes exactly `entries` back out.

`legacy_paths()` names a previous install layout to delete outright rather than leave to rot. Nothing is ever written into the user's repo, for any adapter.

Adding a fourth adapter is a repeatable recipe: `.claude/skills/adding-harness-support/SKILL.md` walks through it, including a "verify against the installed binary, not the docs" gate. (`aider` appears elsewhere in this doc purely as a config-shape example — no `AgentAdapter` impl, not a real launch target.)

### Agent Awareness

Injected env vars per session (the *only* thing Overseer injects at launch):
- `OVERSEER_SOCKET` — Unix socket path
- `OVERSEER_AGENT_ID` — UUID
- `OVERSEER_ROLE` — `root` (the wire value for a workspace) | `child`
- `OVERSEER_DEPTH` — derived tree depth (`1`, `2`, or `3`)
- `OVERSEER_PARENT_ID` — parent UUID (absent for a workspace)
- `OVERSEER_REPO` — repository name
- `OVERSEER_TASK` — the child's assignment for CLI-spawned children; absent for a workspace or TUI-created child waiting for a manual first prompt.

Role behavior lives in **user-level content installed by `overseer install`** (a skill, a plain instructions file — whatever the harness itself loads), matched to `$OVERSEER_ROLE`:
- Workspaces: may spawn children via `overseer spawn --name "<short-kebab-name>" --task "<full, self-contained task description>" [--adapter claude|opencode]`. Cross-harness spawning is supported — a claude workspace may spawn an opencode child and vice versa.
- Child agents: depth-2 children may delegate real sub-tasks via `overseer spawn`; depth-3 leaves work inline. All children set up their own branch/worktree and report completion explicitly (`overseer status done`) — never inferred. Child skills prohibit invisible built-in subagents because delegation belongs in the observable tree, with only a short, single-shot read-only lookup carve-out.

Two harnesses, two status-wiring mechanisms — each verified against the installed binary, not just its docs:

**Claude Code** — user-level `~/.claude/settings.json` hooks (shared across sessions, no-op outside Overseer, all passing `--from-hook`, which reads the Claude-specific hook-payload JSON from stdin):

| Hook | Pushes | Why |
|------|--------|-----|
| `SessionStart` | `idle` for a workspace, `running` for a child (branches on `$OVERSEER_ROLE`) | A workspace is a bare shell the human ran `claude` inside themselves — freshly started, it's waiting on the human to type a prompt, so `running` here would be misleading before the first prompt is even submitted; `UserPromptSubmit` is what flips it. A child's task is delivered as its initial prompt, so it's already working the instant it launches (registered `Spawning` — see "Spawn Data Flow") — this is what flips it to `running`. Both branches self-identify the adapter and print a pointer at the role-specific skill. |
| `UserPromptSubmit` | `running` | The point real work actually begins — covers both a session's first prompt and the user prompting again after the agent had gone `idle`. |
| `PostToolUse` | `running` | Actively working. |
| `Stop` | `idle` | Finished responding — **not** done. No hook ever pushes `done`; agents report it explicitly with `overseer status done`, while a clean root PTY exit is inferred as `done` (see Cleanup below). |
| `Notification` (`permission_prompt`) | `blocked` + `attention=permission` | Live-probed on Claude Code 2.1.209. Approval reaches `PostToolUse`; denial reaches `Stop`; both clear permission attention. |

Every Claude `--from-hook` push also reads the newest real assistant turn's `message.model` from `transcript_path`, when present, and preserves the last known value otherwise. Synthetic transcript messages are ignored.

**opencode** — a plugin at `~/.config/opencode/plugin/overseer.js`, auto-loaded (opencode scans that directory itself, no `opencode.jsonc` entry needed). Role instructions (`overseer-root.md`/`overseer-child.md`) merge into `opencode.jsonc`'s `instructions` array unconditionally — each file's own "only applies when `$OVERSEER_ROLE=...`" opening line makes loading both, every session, harmless:

| opencode event | Pushes | Why |
|------|--------|-----|
| `session.created` | `idle` for a workspace, `running` for a child (branches on `$OVERSEER_ROLE`) | A workspace is a bare shell waiting on the human to prompt it; a child's task is already its initial prompt, so it's working the instant it launches. Same reasoning as Claude's `SessionStart`. |
| `session.status` (`status.type === "busy"`) | `running` | The actual "agent is actively working" signal — confirmed live; better grounded than proxying through `tool.execute.after`, which only fires around tool calls. |
| `session.idle` | `idle` | Finished responding. |
| `permission.asked` / `permission.v2.asked` (generic event bus) | `blocked` + `attention=permission` | Live-probed on OpenCode 1.17.20 for root and child sessions. The typed top-level `permission.ask` hook exists in the installed declarations but did not fire. Overseer never writes a permission decision. |
| `permission.replied` / `permission.v2.replied` | `running`, clears permission attention | Both approval and denial resolve the attention condition. |
| `session.error` with structured `APIError` | preserves active lifecycle + provider attention | HTTP 429 maps to `rate_limit`, 402 to `billing`, other structured API failures to `provider_error`; `Retry-After` is forwarded when supplied. Experimental until a real provider-limit response is live-probed. |
| typed `chat.message` hook | `running` + model | Reports the hook's authoritative `providerID/modelID`; absent model data leaves the last known model untouched. |

**Branch is self-reported the same way for both harnesses, uniformly, not per-adapter.** Every `overseer status` push — hook-invoked or not — auto-detects the pushing process's own current git branch via `git rev-parse --abbrev-ref HEAD` run in its own cwd (`cli::detect_current_branch`, the agent-process-side mirror of the daemon's read-only `GitClient::current_branch`), unless the caller passes an explicit `--branch`. This lives once in the shared `overseer` CLI binary rather than per-adapter: Claude's hook subprocess and opencode's `execFile`'d plugin subprocess both inherit their harness's own tracked cwd (verified live for Claude — the hook JSON's `cwd` field and the hook subprocess's OS-level cwd are identical), so neither adapter needs its own git-shelling code. Detection failure (not a git repo, no commits yet, `git` missing) preserves the last known branch rather than blanking it out, same "preserve last known value" posture as `model_name` above. This is why a **child's** registered branch starts empty (`—` in the TUI, same convention as a non-git workspace) rather than a synthesized `overseer/<id>` placeholder — its real branch only appears once its own hook/plugin fires from inside the worktree it sets up (per the child skill's convention, `ovsr/<slug>`). A **workspace's** branch, read once from git at `overseer start` time, gets the same live top-up for free the moment a real harness starts reporting inside it.

Status meanings: `spawning` (registered, launching) → `running` (working) → `idle` (finished responding) / `blocked` (needs you) → `done` or `error` (see Cleanup). A child whose PTY exits cleanly is auto-dropped rather than left in `done`; a childless workspace is auto-dropped the same way, while a workspace with live children is not.

Attention is separate from lifecycle: `permission`, `rate_limit`, `quota_limit`, `billing`, or `provider_error`, with optional bounded message/retry time and an observed timestamp. `overseer list`/`agent` include both attention and the selected adapter's capability matrix. Claude and OpenCode context usage are unsupported because their lifecycle integrations do not expose an authoritative active window size; Claude's correct 1M-aware percentage exists only in the user's single-owner `statusLine` command, which Overseer does not replace. Context percentage is not rendered in the TUI.

Every agent also carries `status_secs`: seconds held in its *current* status, reset only on an actual status change. Visible via `overseer list`/`overseer agent`. In the TUI, tree rows show it for `blocked`/`idle` only (`blocked 2m`); the Details pane always shows it on its own `since:` line.

Claude Code sessions also carry `context_pct` (`--from-hook` reads it off the transcript, see the `overseer status` row above) — kept in the wire protocol and `overseer list`/`overseer agent` JSON for scripting, but **no longer rendered in the TUI** (removed from both the tree row and the Details pane, 2026-07-15). The removed display computed a % of a hardcoded 200,000-token window, which is wrong for any account on a larger context variant (e.g. Claude's 1M-token Sonnet) — rather than keep showing a number that can silently be wrong depending on account config, it was pulled from the view entirely. Re-adding it to the UI needs an honestly-sourced per-adapter signal (see `HARNESS-CAPABILITIES.md`'s `context_usage` capability), not a bigger hardcoded guess.

### Attention Surfacing

A `blocked` (or, if configured, `idle`) agent reaches you two ways beyond the tree's own `!` badge, both edge-triggered (fire once on the transition, not on a repeated push):

- **Terminal bell.** `\x07` to the TUI's own stdout on any `→blocked` transition — works everywhere, including over ssh. On by default unless `[notify] bell = false`.
- **Desktop notification.** `osascript`/`notify-send`, off by default (`[notify] mode = "off"`). `"blocked"` fires on `→blocked` only; `"blocked+idle"` also fires on `→idle`.

Both are driven by one pure diff (`notify::status_transitions`) comparing each frame's tree against the last — identical for `--mock` and daemon-attached. Config in `[notify]` (see Config below). Out of scope: a supervision loop that auto-re-prompts an idle child — this surfaces, a human (or the workspace reading `overseer list`) decides.

Normalized attention also renders independently of lifecycle: `$` for rate/quota/billing and `!` for permission, with `$` taking deterministic precedence over a simultaneously blocked lifecycle. Details show reason, bounded message, retry delay, and age. The `?` popup explicitly warns that no badge may mean unsupported/unknown rather than healthy.

### Workspace

Overseer does **not** manage workspaces. A parent runs in the repo's existing checkout; a child sets up its own git worktree/branch — agents already know how to do this. Overseer never runs `git worktree`, creates branches, or merges. Integrating an agent's branch is the user's call.

**Project-local convention (this repo only, not an Overseer feature):** since this repo happens to be the Cargo project Overseer is itself written in, a child working here should point its worktree's build output at a shared location instead of a fresh `target/` per worktree — each independent worktree otherwise recompiles the whole dependency tree from scratch, which adds up in disk and CPU/heat once a few children are building in parallel:

```
mkdir -p .cargo && printf '[build]\ntarget-dir = "../overseer-shared-target"\n' > .cargo/config.toml
```

Cargo locks the target dir per-invocation, so concurrent builds from sibling worktrees queue rather than corrupt anything. This is deliberately *not* baked into Overseer's generic, installed child-skill content — Overseer manages projects in any language, and a Cargo-specific step has no business in instructions every non-Rust project would also receive.

### Cleanup

Dropping an agent kills its PTY and deregisters it — Overseer doesn't delete branches or worktrees (it didn't create them). Recursive drop is depth-first, children before parent. Workspaces can't be dropped via IPC, only the TUI (`Request::TuiDrop`).

When a child PTY exits cleanly (code 0), the background watcher recursively drops that child and its descendants instead of leaving a dead pane in the tree. A child that exits non-zero or by signal stays visible as `error` for inspection. A **childless** workspace's clean exit is auto-dropped the same way — its dead pane can never be re-entered (jumping in is gated on the session still being alive), so there's nothing left to review, same as a leaf child. A workspace that still has live children is never auto-dropped this way, even on a clean exit — dropping it would recursively kill those children with no confirmation — and stays visible as `done` until the user explicitly drops it; a failed root exit (non-zero or by signal) always stays visible as `error` regardless of children, until the user explicitly drops it.

**Quitting the TUI is a detach, not a kill.** `q`/`Ctrl-C` closes the attach connection immediately, no confirm — the daemon and every agent it tracks are unaffected. A later `overseer` launch reattaches and recovers the full tree and each agent's terminal content.

**`overseer shutdown`** (CLI) or **`Q`** (TUI, with a confirm) is the actual kill switch: recursive-drops every workspace, then the daemon exits. Dropping the last agent does *not* shut the daemon down on its own. **`overseer kill`** is the fallback for when `shutdown` can't even get a response — see "Daemon + Attach Protocol" above for what it does and why a plain `kill -9` on the daemon pid alone wouldn't be enough.

**Daemon death is total — for the graceful paths.** No on-disk state file, and `shutdown`/`Q` recursively drop (and thus `SIGKILL`) every agent PTY explicitly before the daemon exits, so nothing outlives it: a fresh daemon always starts from an empty tree (same contract a `tmux` server has with its panes). This is *not* an automatic consequence of the daemon process itself dying, though — a bare `kill -9` on the daemon pid (or a crash) leaves every agent PTY running as an orphan, since each is `setsid()`'d into its own session and the daemon isn't its process-group leader. `overseer kill` accounts for this by finding and killing those orphans too, rather than assuming a plain signal-kill of the daemon takes them with it.

### TUI Layout

```
┌───────────────────────────┬─────────────────────────────────────────┐
│ WORKSPACES                │                                         │
│ ◌ overseer            idle│   the selected agent's live grid,       │
│   main                    │   painted directly into this same       │
│ ├ ⠸ auth-module          │   ratatui frame by ui/term_pane —       │
│   ovsr/auth · claude…     │   real color, real interaction          │
│ ├ ! tests      blocked 2m │   once focused (Ctrl-l)                 │
│   ovsr/tests · opencode   │                                         │
│ └ ✓ docs                 │                                         │
│   ovsr/docs · claude      │                                         │
├───────────────────────────┤                                         │
│ task:   auth-module       │                                         │
│ repo:   overseer          │                                         │
│ branch: ovsr/a            │                                         │
│ status: running           │                                         │
│ since:  4m                │                                         │
└───────────────────────────┴─────────────────────────────────────────┘
 OVERSEER   1/6 running · 2 blocked   j/k nav  Ctrl-l/↵ jump in  n workspace  s child  d drop  D drop+children  / search  q quit  ? help
```

Both columns are ratatui-rendered in one process, one window — `ui::render` does its own ~25/75 horizontal split every frame; no second pane, no multiplexer. `ui::term_pane` paints the selected agent's terminal cell-by-cell into that half from a `GridSnapshot` — the only render currency, in both `--mock` and daemon-attached modes (`App::pane_grid` asks `SessionManager::grid_snapshot` directly in `--mock`, or returns the last streamed snapshot otherwise). `ui/` never touches `alacritty_terminal`.

Each tree item has a uniform two-line height. The first line right-aligns status in dim gray (red/bold for `blocked`) and truncates the name with `…` (`format_tree_row`/`truncate_with_ellipsis`). The dim second line aligns under the name and shows the branch when non-empty, then the verified model identifier when known; otherwise it falls back to the full adapter name. A bare `shell` workspace contributes no harness text, and an item with neither branch nor harness/model still reserves a blank second line. The selected background and mouse hit target cover both lines. Status bar: "`N running`", or "`N running · M blocked`" once any agent needs attention. Context % is no longer surfaced in the UI (tree or Details) — see the note under "Status meanings" below.

Status badges: `$` rate/quota/billing attention (highest precedence) · `!` permission/blocked · `●` running · `◌` idle · `✓` done · `✗` error · `…` spawning. A missing attention badge does not prove health when that adapter declares the capability unsupported.

**Keybinding house style: nvim.** `j`/`k` within a list, `Ctrl-h`/`Ctrl-l` between panes. New bindings extend this vocabulary, never a parallel one or a prefix-key/chord model. Keys an agent's own TUI relies on (e.g. `Ctrl-j` = Claude Code's insert-newline) pass through to a focused pane untouched — `Ctrl-h` is the *only* key Overseer intercepts while focused (real Backspace still works: terminals send `DEL`, not `^H`).

Every tree-focus action below is remappable via `[keybindings]`. Fixed regardless of config: `Ctrl-h` (stealing it would take a key an agent's own TUI needs) and the scrollback keys (next section). `Enter`/`o`/`Ctrl-C` also stay fixed as extra aliases for `jump_in`/`quit` even if those actions are remapped.

| Key (default) | Action |
|-----|--------|
| `j` / `k` | Navigate agent tree (tree focus only) |
| Left-click the tree | Select the clicked row and jump into its pane if the agent is alive; a dead agent falls back to tree focus. The click itself remains an Overseer UI action and is never forwarded to the agent |
| Left-click the pane | Jump in, same as `Enter`/`o` (alive check + scroll reset included); a click on an *already-focused* pane is a no-op so it never resets a wheel-scrolled position |
| `<space>` | Fold/unfold the selected agent's children |
| `Ctrl-l` / `Enter` / `o` | Jump in — moves keyboard focus into the selected agent's pane, if it's alive |
| `Ctrl-h` | From inside a focused pane, jump back out to the tree — the only key a pane intercepts; everything else, Ctrl-c included, forwards to the agent |
| `n` | Spawn a workspace immediately, no prompt: a bare shell in this process's own cwd |
| `s` | Ask for a child name, then create an idle child with the parent's configured harness for manual prompting |
| `d` | Drop selected agent (confirm prompt) |
| `D` | Recursive drop — agent + all children (confirm prompt) |
| `q` / `Ctrl-C` | Quit immediately, no confirm — detaches, never kills any agent or the daemon (see Cleanup) |
| `Q` | The kill switch: recursive-drop every agent and exit the daemon (confirm prompt) |
| `/` | Fuzzy search the tree by name (see "Search" below) |
| `?` | Open the live keybinding reference — any key closes it |
| `Ctrl-u` / `Ctrl-d` | Scroll the selected agent's pane up/down half a page (tree focus only, fixed — see "Scrollback" below) |
| `Ctrl-y` / `Ctrl-e` | Scroll one line up/down (nvim semantics: `e` = down; fixed) |
| `↑` / `↓` | Scroll the selected agent's pane one wheel notch up/down (tree focus only, fixed) — the fallback that makes trackpad scrolling work in wheel-as-arrows terminals; see the "Mouse wheel not working" caveat under "Scrollback" |
| `G` | Jump the selected agent's pane back to the live bottom (fixed) |
| mouse wheel (over pane) | Tree focus scrolls Overseer's history; pane focus forwards to an agent TUI that requested mouse reporting, otherwise falls back to Overseer's history |

### Search

`/` turns the agent structure pane's own title into a live query box (`" / <query>█ "`, yellow border) instead of opening a separate popup — no floating box ever sits on top of the tree it's filtering. As you type, the tree shows only agents whose name fuzzy-matches (`fuzzy_match(query, name) -> Option<u32>`: case-insensitive, in-order subsequence, contiguous runs score higher), plus every ancestor of a match (dimmed, for context), visible the instant each keystroke lands. `Enter` moves the *real* cursor to the current selection (or the first match) and closes the prompt; `Esc` closes it without moving anything.

### Help

`?` opens a centered, context-grouped interaction reference. Configurable rows are generated from the live `Keybindings` struct (`ui::help_rows`), never from default-key strings; the popup also lists fixed pane aliases, scrollback keys, modal controls, and mouse routing. Any key closes it.

### Scrollback

While tree-focused, `Ctrl-u`/`Ctrl-d`/`Ctrl-y`/`Ctrl-e`/`G` — and plain `↑`/`↓`, one wheel notch each — scroll the selected pane as a read-only preview. A real wheel there moves Overseer's terminal history 3 lines per notch. Once a pane is focused, keys stay off-limits (`Ctrl-h` remains the only intercepted key), and wheel routing follows the inner terminal's negotiated mode: if the agent TUI enabled mouse reporting, Overseer translates outer coordinates into pane-local coordinates, encodes the requested SGR/UTF-8/classic xterm wheel report, and writes it to the PTY so Claude Code/OpenCode scroll their own conversation; a shell or program without mouse reporting falls back to Overseer's history. Scrolling outside the pane is ignored.

The outer scrolled offset resets to the live bottom on cursor move and on jump-in, so interaction never starts on a stale shell view. A focused program without mouse reporting can still use the wheel to move that outer offset; an agent TUI with mouse reporting owns its own conversation viewport instead, so Overseer's `display_offset` remains zero.

Outer-history scrolling happens where the terminal state lives: the daemon (`SessionManager::scroll_display`/`scroll_to_bottom`/`display_offset`). Focused inner-TUI wheel reports instead use the existing `Write` input path, exactly like keys and paste.

**The scroll data path is coalesced at both ends** — this is load-bearing, not an optimization to strip. A full `GridSnapshot` is ~1MB of JSON for a realistic pane (see PERFORMANCE.md's floor-guard test), and a trackpad flick delivers dozens of wheel notches nearly at once, so one request-plus-full-reply *per notch* floods the attach connection with tens of MB of serialize/parse work that outlives the gesture by seconds — a real, reported bug that froze the whole TUI (the client's attach writes are synchronous on the UI thread; once the daemon stopped draining requests, one more `send` blocked the event loop for good). The shape that fixed it: **client-side**, `run_app` drains every input event already queued each 16ms frame and folds all scroll intents (wheel notches, tree-focus arrows) into one net delta, flushed as at most one `Request::Scroll` per frame — a flick that nets to zero sends nothing; **daemon-side**, the scroll handlers never build a snapshot inline — they apply the scroll, and only if the offset actually moved (`SessionManager::scroll_display` reports this; a wheel held at the clamp moves nothing) mark the connection's `WatchState::scroll_dirty`, which the existing 16ms output poller consumes alongside its generation check. However many scrolls land within a tick, one grid goes out — the same worst-case send rate as a continuously-busy streaming pane, which is measured and fine (PERFORMANCE.md). Scrolled-up views don't fight incoming output, by the way: `alacritty_terminal` anchors the display offset as new lines enter history, so the view stays put and the offset grows.

**Mouse wheel not working / TUI freezing on scroll?** Two distinct failure classes, and the freeze was ours: before the coalescing above, wheel scrolling in *any* terminal (Alacritty included) could wedge the TUI hard enough that force-killing it left the host terminal with mouse capture still armed — if you hit that, it's fixed at the data path now, and the panic hook restores the terminal on every unclean exit we control. The second class is real: some terminal emulators don't send an xterm mouse-wheel report even with `EnableMouseCapture` armed and instead translate wheel motion into synthetic `Up`/`Down` key events (macOS Terminal.app is the canonical offender). Those are byte-for-byte indistinguishable from real arrow-key presses, and real arrow keys are exactly what agents' own TUIs rely on — so while a pane is *focused*, arrows still forward to the agent untouched (`Ctrl-h` stays the *only* intercepted key; pinned by `tui::tests::arrow_keys_forward_to_the_agent_while_pane_is_focused_not_scroll`). In **tree focus**, though, the pane is a read-only preview and arrows were unbound, so `↑`/`↓` now scroll the preview one wheel notch each — on a wheel-as-arrows terminal your trackpad therefore scrolls the preview with no setup at all; jump out (`Ctrl-h`), scroll, jump back in. **To find out which class your terminal is in, run `cargo run --example mouse-probe`** in it and wheel over the window: it arms the exact capture Overseer arms and prints every event plus a verdict (real wheel reports vs. wheel-as-arrows vs. swallowed). Known-good real-wheel terminals: iTerm2, kitty, Alacritty, WezTerm. **Kaku** (a WezTerm fork) translated wheel-to-arrows in the alternate screen for non-mouse-grabbing apps before its 0.8.0 (PR tw93/Kaku#226, 2026-03-22); 0.8.0+ should deliver real wheel reports to a mouse-reporting app like Overseer — a Kaku 0.9.0 user's "wheel does nothing" report turned out to reproduce in Alacritty too and was the flood/freeze above, not translation. If the probe shows Kaku (or anything else) still translating on your box, the tree-focus arrows cover it; upgrading Kaku (0.14.0+ has several further mouse-path fixes) is also worth a shot.

### Spawn Data Flow

```
Workspace runs: overseer spawn --name "write-tests" --task "write tests" --adapter claude

IPC server (spawn_blocking):
  → name = name.filter(non-blank).unwrap_or(task) = "write-tests"  // task text is the fallback only
  → derive caller depth and direct-child count from the registry
  → reject if child depth would exceed 3 or max_children would be exceeded
  → AgentRegistry::register(child, name, parent=caller, status=Spawning)
  → adapter = adapter_for(name); command/extra_args resolved from config.adapters[name]
  → LaunchContext.task = "write tests"
  → SessionManager::launch(agent_id, cwd=repo, adapter.spawn_command(ctx),
                           adapter.env_inject(ctx))
      spawn_command: <command> <extra_args...> "write tests"   // task is the final positional arg
      env_inject:    ...identity vars..., OVERSEER_TASK="write tests"
  → replies: {"agent_id": "...", "branch": ""}   // empty -- self-reported later, never synthesized

TUI re-renders with the new child visible under the parent, labeled "write-tests"
in the tree — short and recognizable even though the task text (the child's
actual initial prompt) can run to a full paragraph. It starts working
immediately instead of sitting at a bare prompt. The child sets up its own
branch/worktree on startup (`git worktree add ../<repo>-<slug> -b ovsr/<slug>`,
per the overseer-child skill's worked example), and its own SessionStart hook
flips it from Spawning to Running moments later — the same push that, per
"Agent Awareness" above, self-reports the real branch for the tree row to
pick up in place of the initial empty placeholder.
```

`overseer start` (launch a workspace) is a *different* path — always no task and no adapter: it registers `role=root`, `status=idle`, names the node after the repo, and launches a bare shell (`$SHELL`) instead of `adapter.spawn_command(ctx)`. Whatever you run inside that shell (e.g. `claude`) inherits the injected identity env vars from the PTY itself and reports its own status via the same push hooks — Overseer never detects or launches it. The cwd doesn't have to be a git repo — git failure just falls back to the directory's own basename as the name and an empty branch, rather than rejecting the workspace outright; only a nonexistent/non-directory cwd is rejected.

---

## Config

`~/.config/overseer/config.toml`. **Implemented:** `[defaults]`, `[adapters.*]`, `[notify]`, `[keybindings]`, `[theme]` — all below. `[defaults]`/`[adapters.*]` load once at daemon/mock startup; `[notify]`/`[keybindings]`/`[theme]` load independently in the TUI process, since they're properties of *your* terminal, not the daemon's. Missing/invalid file falls back to the built-in default; a bad *value* for one field warns on stderr and keeps that field's own default — never a hard error.

```toml
[defaults]
adapter = "claude"
max_children = 8
im_not_afraid_of_agents = false   # opt-in: auto-approve permissions for every spawned child — see README.md's Danger Zone

[adapters.claude]
command = "claude"
extra_args = []

[adapters.opencode]
command = "opencode"
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

A child spawn resolves `command`/`extra_args` from `config.adapters[name]`, not the adapter name itself — lets a user opt a harness into a bypass flag like `--dangerously-skip-permissions` (see `im_not_afraid_of_agents` above and README's Danger Zone), and lets a user point "claude" at a custom binary. A name with no entry in `config.adapters` is the same `UnknownAdapter` error as one with no `AgentAdapter` impl (e.g. `aider`, a config-shape example only — see "Agent Adapter Trait").

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

Implementation plans and research notes live in **`docs/specs/`**, which is **gitignored** — local working documents, not part of the distributed repo. Never commit one, and never reference a spec from code or committed docs — once a phase ships, the code/AGENTS.md must stand on their own. `docs/specs/TASKS.md` is the working backlog: when picking up work, check it first; when finishing or parking work, update it.

---

## Best Practices

- **IPC is the only shared channel.** Agent ↔ overseer communication always goes through the Unix socket — never shared in-process state from an agent context.
- **Depth 3 and `max_children` admission live only in the IPC server's `spawn` handler.** Depth is derived from the parent chain, never stored on a node. The TUI and adapters do not duplicate enforcement.
- **`alacritty_terminal` lives only in `crates/overseer-core/src/session/pty.rs`** — the one file in the whole workspace; the `overseer` bin crate never imports it at all (both halves guarded by an `alacritty_boundary.rs` test per crate). `SessionManager`'s public method set — `launch`, `kill`, `write`, `resize_all`, `is_alive`, `scroll_display`, `scroll_to_bottom`, `display_offset`, `grid_snapshot`, `term_modes`, `generation`, `drain_exits` — is the entire terminal-backend contract; every signature uses only `GridSnapshot`/`TermModes`/std types. Swapping the backend means rewriting that one file, not chasing leaks through `ui/` and `ipc/`.
- **Parse functions are pure.** E.g. `parse_session_line` takes a `&str`, returns a value — no process spawning, no I/O. Trivially testable.
- **`AgentNode` is a data model, not a handle.** No PTY ownership. Session handles live in `SessionManager`, keyed by `AgentId`. Overseer holds no worktree state — that's the agent's.
- **Status is push, not pull.** Agent hooks POST status changes; the daemon POSTs registry/output events to attach clients the same way. Overseer never infers status from PTY output; the TUI never polls for tree state. Each push is its own independent connection with no ordering guarantee against any other (`ipc::server` spawns a task per connection) — `Request::Status`'s `pushed_at` timestamp and `AgentRegistry::set_status`'s staleness guard are what keep a late-arriving-but-earlier-fired push from clobbering a fresher one.
- **`ui/` is a render layer only.** No business logic. State mutations go through `App` / `AgentTree` / `SessionManager`.
- **One code path per request, regardless of backend.** `App::dispatch`/`with_tree`/`write_input`/etc. branch on `Backend::{Mock, Daemon}` in exactly one place (`app.rs`) — `tui.rs`/`ui/` call the same methods either way (bar the one `pane_grid` lookup in `run_app`, which is `ui`-shape glue, not business logic).

## What to Avoid

- **No MCP transport.** Unix socket + hooks is intentional — no token overhead, no plugin registry approval, works locally out of the box.
- **Don't let delegation become invisible.** Depth-2 children delegate real sub-tasks through `overseer spawn`; their installed skill forbids built-in subagent/Task tools except for a short, single-shot read-only lookup. Depth-3 leaves work inline. The server remains the sole depth/cap enforcement point.
- **Don't hardcode adapter binary paths.** Always resolve through adapter config so users can point to a custom binary or wrapper.
- **Don't add agent status polling.** If hooks aren't firing, fix `overseer install`, not a background poller. Same for the TUI — missing tree updates get fixed in the registry's broadcast or the attach connection.
- **Don't reimplement git.** No worktree creation, branching, or merging. Agents own their isolation; Overseer's only git use is read-only display info.
- **Don't write into the user's repo.** All agent config is installed at the user level by `overseer install`. Launch injects env only.
- **Don't skip the confirm prompt for `d`/`D`/`Q`.** Killing a session (or the whole daemon) interrupts in-flight work.
- **Don't make quitting kill agents.** `q`/`Ctrl-C` is a detach, never touches a session or the daemon. `d`/`D` kills one agent, `Q` kills everything plus the daemon; both confirm.
- **Don't add a second way to *gracefully* end the daemon process.** `Request::Shutdown` asks the accept loop to stop and lets `main` return — no `std::process::exit`. A response still in flight when the process exits is a real bug (once caused by `tokio::sync::Notify::notify_waiters` losing a wake under this exact race — use `notify_one`, which stores a permit, for any future stop-signal here). `overseer kill` (`crates/overseer-core/src/kill.rs`) doesn't violate this: it always tries this exact same `Request::Shutdown` path first, and only reaches for `SIGKILL` once that request has been given a real chance and failed to get a response — it's a forceful fallback for an unresponsive daemon, not a second graceful exit path racing this one.
- **Don't add a `Request::Drop`-with-a-flag for workspace drops.** Workspace-allowed drop is `Request::TuiDrop`, a distinct wire request only the TUI's key handling constructs — a caller-supplied bool would let any script opt out of the restriction it exists to enforce.
- **Don't assume `alacritty_terminal` exposes raw PTY bytes.** It doesn't, not without reimplementing its mio/signalfd event loop — that's why the attach protocol streams rendered `GridSnapshot`s instead. Re-verify against the installed version before retrying; a future public tap would be a contained change to `session::pty` + `ipc::protocol`, not a redesign.
