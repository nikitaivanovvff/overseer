# Overseer

**An IDE for agents.** A terminal-native TUI for observing and steering a fleet of parallel AI coding agents from a single window — instead of juggling five terminal tabs. Built in Rust. Nvim-aesthetic; a single ordinary alt-screen app with no bundled multiplexer — each agent is a PTY Overseer owns directly, emulated in-process via `alacritty_terminal` and rendered straight into the same ratatui frame — with a Unix socket IPC layer that gives agents a lightweight API to register, report status, and spawn children — without MCP overhead.

The agents are already smart. Overseer does **not** reimplement what they do — it does not manage git worktrees, branches, or merges; agents handle their own isolation. Overseer is the observability, routing, and approval surface on top: see every agent's state at a glance, jump into any one to approve or intervene, or leave the parent to supervise its own children.

The usual shape is **one root per repository**. `n` spawns a root as a bare shell in a repo you choose (default: cwd) — Overseer doesn't launch an agent for you. You `cd`/run `claude` (or whatever) yourself, in your own time, exactly as you would without Overseer; the row appears in the tree immediately, named after the repo, and its status flips from `idle` to `running` the moment your agent starts reporting via its hooks. From there you talk to it in natural language — "implement X", "research Y", "write unit tests for Z" — and it fans the work out into child agents, each running in its own PTY (auto-launched via the configured adapter) and surfacing as its own row in the TUI. You can drop into any child for approval or a nudge, or ignore them and let the parent check on them periodically.

The hierarchy is intentionally **flat**: a parent (root) can spawn children, but children cannot spawn further agents. This keeps the tree readable, the user in control, and token costs predictable. A **child's** node name is the short label it was given at spawn (`--name`) — falling back to its task text verbatim if none was given, since the task can be a whole paragraph and a name shouldn't have to be. A **root's** node name is the **repo name** — there's no task description at the point a bare shell is spawned, since no agent runs there until you start one yourself. The adapter (claude, aider, etc.) is shown in the detail panel; a not-yet-running root shows adapter `shell`.

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

Agents know their role (`root` or `child`) via injected env vars and a **user-level skill** installed once with `overseer install <agent>` (`overseer teach` still works as a hidden alias). Claude Code hooks POST lifecycle events to the Unix socket to report status — zero agent context tokens consumed, nothing written into your repo.

The registry and every agent's PTY live in a **daemon** process, not the TUI — a `overseer` launch attaches to it as a client (auto-spawning one if it isn't already running). Quitting the TUI detaches; the daemon and every agent it's tracking keep running, and a later `overseer` launch reattaches to exactly what was there before. See "Daemon + Attach Protocol" below.

---

## Architecture

```
overseer daemon (background, one per user, auto-spawned by the TUI)
├── AgentRegistry, SessionManager, Config, git/   ← unchanged internals, all owned here now
├── IPC socket  $XDG_RUNTIME_DIR/overseer/daemon.sock
│               (fallback /tmp/overseer-$UID/daemon.sock), mode 0700 dir, flock-guarded
├── one-shot requests: status/list/agent/start/spawn/drop/shutdown  ← existing protocol, unchanged
└── attach connections: long-lived streams of registry events + rendered terminal snapshots

overseer (TUI) = attach client                    overseer <subcommand> = one-shot client
overseer --mock = fully in-process demo data, never touches a daemon at all
```

```
overseer (binary)
├── ui/               Ratatui-based terminal UI
│   ├── mod           Tree|pane split (~25/75): agent tree, detail, status bar, spawn modal
│   └── term_pane     Paints the selected agent's terminal into the pane half — a live
│                     alacritty_terminal grid in --mock, a daemon-streamed GridSnapshot otherwise
├── session/          PTY + terminal-emulator management (daemon-side only, post-split)
│   ├── pty           SessionManager: owns one alacritty_terminal Term + PTY per agent, keyed by
│   │                 AgentId; also renders GridSnapshot DTOs and tracks each Term's dirty flag
│   └── keys          Crossterm KeyEvent -> PTY escape-byte encoder (input path for a focused pane)
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

Unix domain socket at `$XDG_RUNTIME_DIR/overseer/daemon.sock` (falling back to `/tmp/overseer-$UID/daemon.sock` when `$XDG_RUNTIME_DIR` is unset), owned by the daemon process — one stable, per-user socket that every repo's TUI and every agent's CLI calls share, unlike the old per-invocation socket. The only channel agents use to talk to Overseer — no MCP, no HTTP, no polling. The `overseer` binary doubles as the client: each subcommand opens the socket, sends one newline-delimited JSON request, prints the reply, and exits. Agents invoke these commands, not raw HTTP endpoints — it's a terminal app, the API is its CLI.

| Command | Args | Description |
|---------|------|-------------|
| `overseer install` | `<agent> --uninstall?` | Install (or remove) the user-level skill(s) + status hooks for an agent type. Run once at setup, not per launch. `teach` is a hidden alias. |
| `overseer daemon` | — | Runs the daemon itself: binds the socket, serves requests, streams attach events, watches session exits. Hidden from `--help` — not a user workflow, the TUI spawns one automatically. |
| `overseer start` | `--cwd?` | Register a root and launch a bare shell for it in its own PTY (default cwd: current directory). No adapter is launched — run your own agent inside it. |
| `overseer status` | `<status> --message? --from-hook?` | Push a status update for the calling agent. No-op (silent exit 0) when not running under Overseer. `--from-hook` reads the Claude Code hook payload from stdin to classify a `blocked` push (idle nag vs. real permission request) and attach context % — Claude-specific; opencode's plugin and pi's extension push plain `overseer status <s>`, no `--from-hook`, since their own events are already precise. |
| `overseer spawn` | `--task --name? --adapter?` | Request a child. Rejected if the caller is already a child. `--task` is the child's entire initial prompt; `--name` is a short, distinct tree-row label (falls back to `--task` verbatim if omitted or blank). |
| `overseer drop` | `<id> --recursive?` | Kill the agent's PTY and deregister it. Overseer does not touch the agent's branch/worktree. Root agents are rejected here — only the TUI's `d`/`D` (a distinct wire request, see below) can drop one. |
| `overseer shutdown` | — | The kill switch: recursive-drops every root, then the daemon process exits. Same request the TUI's `Q` sends after its confirm. |
| `overseer list` | — | List all agents |
| `overseer agent` | `<id>` | Get agent detail |

Identity (`OVERSEER_AGENT_ID`, socket path) comes from injected env, so commands don't pass it explicitly.

### Daemon + Attach Protocol

The daemon is what actually owns `AgentRegistry` and `SessionManager` — the TUI is just its first, richest client. On startup the TUI tries to connect to the socket; if that fails, it spawns `overseer daemon` detached from its own controlling terminal (`setsid`, stdio to a log file next to the socket) and retries with backoff before attaching. A `flock`-based lockfile (`daemon.pid`, next to the socket) makes a second daemon targeting the same socket fail fast instead of racing the first for the bind.

Attaching upgrades a connection with `Request::Attach`: the daemon replies with one `AttachEvent::Snapshot` (every agent, as of that instant), then streams events until the connection closes:

| Event | When |
|-------|------|
| `AgentRegistered` / `AgentRemoved` | A `start`/`spawn`/`drop` mutates the registry — pushed from `AgentRegistry`'s own broadcast channel, not polled |
| `StatusChanged` | Any status push (hook, explicit `overseer status`, exit sweep) |
| `Output` | The **watched** agent's rendered terminal grid — see below |
| `Shutdown` | The daemon is exiting (`overseer shutdown`/`Q`) — the client treats this like the connection dropping |

The same connection accepts `Watch { agent_id }` / `Unwatch` (start/stop streaming one agent's terminal — the TUI watches whichever agent is currently selected, switching on cursor move and sending an immediate grid on `Watch` so switching feels instant), `Write { agent_id, data }` (forward keystrokes/paste), and `Resize { cols, lines }` (every agent shares one PTY size). `Start`/`Spawn`/`Drop`/`Status`/`List`/`Agent` still go over ordinary one-shot connections, exactly as before the daemon split — the attach connection is additive, not a replacement for those.

**Rendering deviation from the original design:** the natural design ships raw PTY bytes so the client can feed its own `Term` — but `alacritty_terminal` 0.26 doesn't expose incoming PTY bytes without reimplementing its mio/signalfd event loop, so the daemon instead keeps owning the real `Term` (`session::pty`, unchanged internals) and the attach connection streams a rendered `GridSnapshot` DTO (cells + colors + cursor + the two `TermMode` bits key encoding needs) whenever `SessionManager`'s dirty flag (set on `Event::Wakeup`) says the watched agent's screen changed. `ui::term_pane::paint_grid_snapshot` paints it directly — no client-side `Term` needed. Same visual result as the raw-byte design, without touching the already-tested PTY plumbing.

Root-drop's IPC restriction survives the client/server split as `Request::TuiDrop` — a request distinct from `Request::Drop`, sent only by the TUI's own `d`/`D` key handling (never by `cli.rs`'s `overseer drop`, never by an agent). It's a safety rail, not a security boundary (this is a local, single-user socket) — the point is that a script or a supervising agent calling the documented CLI can't accidentally take out a whole root tree.

`--mock` never touches any of this: it's the pre-daemon-split architecture verbatim, in-process, with its own throwaway socket, purely for demoing the UI against seeded tree data.

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

`InstalledFile` is a `(path, content, merge_strategy)` triple written under the agent's user config dir, one of three `MergeStrategy` variants:
- `Overwrite` — Overseer owns the file outright (a skill, a plugin/extension script).
- `JsonMerge` — Claude-specific: merges into `~/.claude/settings.json`'s `hooks` object-of-arrays shape, tagging Overseer's own entries with `_overseer: true` so uninstall removes exactly those and nothing the user added.
- `JsonArrayMerge { key, entries }` — generic: merges/removes specific string `entries` into/from a named top-level JSON array field (opencode's `instructions`). Array elements here are bare strings with no room for an `_overseer` sentinel, so uninstall removes exactly `entries` back out, byte-for-byte restoring what wasn't Overseer's.

`legacy_paths()` names any previous install layout (e.g. Claude's old single `skills/overseer/`) that install/uninstall should delete outright rather than leave to rot alongside the current one. Nothing is ever written into the user's repo, for any adapter.

Adding a fourth adapter is a repeatable recipe, not a one-off: `.claude/skills/adding-harness-support/SKILL.md` (committed to this repo) walks through it, including the "verify against the installed binary, not the docs" gate that mattered a lot in practice — see the per-adapter notes below for what it actually caught.

### Agent Awareness

Injected env vars per session (the *only* thing Overseer injects at launch):
- `OVERSEER_SOCKET` — Unix socket path
- `OVERSEER_AGENT_ID` — UUID
- `OVERSEER_ROLE` — `root` | `child`
- `OVERSEER_PARENT_ID` — parent UUID (absent for root)
- `OVERSEER_REPO` — repository name
- `OVERSEER_TASK` — the child's assignment, verbatim (children only; absent for root). Also delivered as the child's initial prompt — the env var just lets it re-read the assignment mid-session.

Role behavior lives in **user-level content installed by `overseer install`** (a skill, a plain instructions file — whatever the harness itself loads), matched to `$OVERSEER_ROLE` — not in a per-launch file:
- Root agents: may spawn children via `overseer spawn --name "<short-kebab-name>" --task "<full, self-contained task description>" [--adapter claude|opencode|pi]`. Cross-harness spawning is a supported feature, not an accident — a claude root may spawn an opencode or pi child and vice versa; children don't have to run their own harness.
- Child agents: spawning is not permitted; the agent sets up its own branch/worktree for isolation, does the task, and reports completion explicitly (`overseer status done`) — never inferred.

Three harnesses, three different status-wiring mechanisms — each verified against the actually-installed binary (`.claude/skills/adding-harness-support/SKILL.md`'s Task 0 gate), not just its docs:

**Claude Code** — user-level `~/.claude/settings.json` hooks (shared across all sessions, no-op outside Overseer, all passing `--from-hook`, which reads the Claude-specific hook-payload JSON from stdin):

| Hook | Pushes | Why |
|------|--------|-----|
| `SessionStart` | `running` | Closes the gap between "user runs claude" and the first tool call; also prints a pointer at the role-specific skill. |
| `UserPromptSubmit` | `running` | Covers the user prompting inside a pane after the agent had gone `idle`. |
| `PostToolUse` | `running` | Actively working. |
| `Stop` | `idle` | Finished responding — **not** done. No hook ever pushes `done`; the only paths there are an explicit `overseer status done` from the agent, or a clean PTY exit (see Cleanup below). |
| `Notification` | `blocked`, downgraded to `idle` for the ~60s idle nag | Fires for both a real permission prompt and the nag; `--from-hook` classifies which via the payload's message text. |

**opencode** — a plugin at `~/.config/opencode/plugin/overseer.js`, auto-loaded with zero registration needed (confirmed live: opencode scans that directory itself; no entry in `opencode.jsonc`'s `plugin` array required). Role instructions (`overseer-root.md`/`overseer-child.md`) are merged into `opencode.jsonc`'s `instructions` array unconditionally — each file's own "only applies when `$OVERSEER_ROLE=...`" opening line is what makes loading both, every session, harmless:

| opencode event | Pushes | Why |
|------|--------|-----|
| `session.created` | `running` | A session just started. |
| `session.status` (`status.type === "busy"`) | `running` | The actual "agent is actively working" signal — confirmed live; better grounded than proxying through `tool.execute.after`, which only fires around tool calls. |
| `session.idle` | `idle` | Finished responding. |
| `permission.ask` *(a separate hook, not the generic event bus)* | `blocked` | The moment a permission prompt appears. Never sets the hook's own `output.status` — Overseer only observes, the human still decides. |
| `permission.replied` | `running` | The prompt resolved either way; work resumes. |
| `session.error` | *(nothing)* | The exit watcher owns `error`, not a lifecycle push. |

**pi** — an extension loaded via `pi --extension <absolute-path>` at spawn time (confirmed live: bypasses pi's own npm-style package manager/`settings.json` entirely, so `overseer install`/`--uninstall` is just "write/delete one file"). Role instructions are selected **per role** at spawn time via `--append-system-prompt <path>` (pi reads the file's contents when given an existing path), so only the correct doc is ever loaded — no shared array, no self-filtering preamble needed:

| pi event | Pushes | Why |
|------|--------|-----|
| `session_start` | `running` | Mirrors Claude's `SessionStart` — closes the startup gap. |
| `agent_start` | `running` | A turn begins. |
| `agent_end` | `idle` | A turn ends. |
| `session_shutdown` | *(nothing)* | The exit watcher owns `error`. |

**pi never pushes `blocked`** — confirmed against its installed `ExtensionEvent` type union: no permission-request event exists at all (permission gates are themselves opt-in extensions in pi, not something the base install has). Documented as a plain caveat in `pi_root.md`, not faked with a different event.

Status meanings: `spawning` (registered, session launching) → `running` (working) → `idle` (finished responding, awaiting more input) / `blocked` (needs you — permission pending) → `done` or `error` (see Cleanup for how these two are reached).

Every agent also carries `status_secs`: how long, in whole seconds, it's held its *current* status — reset only when the status actually changes (a repeated `running` push from hook chatter doesn't reset it). Visible via `overseer list`/`overseer agent`, which is what makes "check on a long-idle child" an actionable instruction for the root skill, not just a UI nicety. In the TUI, tree rows show it for `blocked`/`idle` only (`blocked 2m`) — a running agent doesn't need a clock — and the detail pane always shows it under `status:`.

### Attention Surfacing

A `blocked` (or, if configured, `idle`) agent can reach you two ways beyond the tree's own `!` badge, both edge-triggered — they fire once on the transition *into* that status, never on a repeated push:

- **Terminal bell.** The TUI writes `\x07` to its own stdout the moment any agent transitions into `blocked`. What that turns into (a badge, a sound, a dock bounce) is entirely your terminal's call — this works everywhere, including over ssh, with zero dependencies. On (default) unless `[notify] bell = false`.
- **Desktop notification.** `osascript`/`notify-send`, fired the same way, off by default (`[notify] mode = "off"`). `"blocked"` fires on `→blocked` only; `"blocked+idle"` also fires on `→idle`, for long tasks where "it finished responding" is itself worth a ping.

Both channels are driven by one pure diff (`notify::status_transitions`) comparing each frame's tree against the previous frame's recorded statuses — not a hook into either backend's event plumbing, so it works identically for `--mock` and a daemon-attached session without either one needing to know notifications exist. Config lives in `[notify]` (see Config below).

Explicitly out of scope (unchanged since Phase 7): a supervision loop that auto-re-prompts an idle child. This surfaces; a human (or the root agent reading `overseer list`) decides what to do about it.

### Workspace

Overseer does **not** manage workspaces. A parent runs in the repo's existing checkout; a child sets up its own git worktree/branch — agents already know how to do this. Overseer's only job is to launch the PTY in the repo and inject identity env. It never runs `git worktree`, never creates branches, and never merges. Integrating an agent's branch is the user's call, same as it would be without Overseer.

### Cleanup

Dropping an agent kills its PTY and deregisters it — that's all. Overseer does not delete branches or worktrees (it didn't create them). Recursive drop is depth-first, children before parent, so no session is orphaned. Root agents cannot be dropped via IPC — only via the TUI (`Request::TuiDrop`, see "Daemon + Attach Protocol").

A PTY exiting on its own (not via `drop`) never removes the row: a background watcher maps the exit code onto `done` (clean exit, code 0 — including a root shell where the user typed `exit`) or `error` (non-zero/signal), and the agent stays visible for you to review before an explicit `drop`.

**Quitting the TUI is a detach, not a kill.** `q`/`Ctrl-C` closes the attach connection and exits immediately, no confirm — the daemon (and every agent it's tracking) is completely unaffected, since it's a separate process the TUI never owned in the first place. A later `overseer` launch reattaches to that same daemon and recovers the full tree *and* each agent's terminal content (the daemon never stopped rendering into its `Term`s while no one was watching). This is what "quitting never kills agents" (long-standing house rule) actually resolves to post-daemon-split: previously it meant "the PTYs survive as orphaned, untracked processes"; now it means "the daemon keeps tracking them and you can get back to exactly where you left off."

**`overseer shutdown`** (CLI) or **`Q`** (TUI, with a confirm — "kill N agents and the daemon?") is the actual kill switch: recursive-drops every root, then the daemon process exits. Dropping the last remaining agent does *not* shut the daemon down on its own — an idle daemon is cheap, and predictable beats clever here.

**Daemon-death caveat:** "persistence" here means the daemon process staying alive, not serialized state — if the daemon itself is killed (crash, `kill -9`, machine restart), every PTY it owned dies with it, the same contract a `tmux` server has with its panes. There is no on-disk state file and no plan to add one; a fresh daemon after that always starts from an empty tree.

### TUI Layout

```
┌───────────────────────────┬─────────────────────────────────────────┐
│ AGENTS                    │                                         │
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
 OVERSEER   1/6 running · 2 blocked   j/k nav  Ctrl-l/↵ jump in  / search  q quit  ? help
```

Both columns are ratatui-rendered in one process, one window — `ui::render` does its own ~25/75 horizontal split every frame; there is no second pane, no multiplexer, nothing external compositing the right side. `ui::term_pane` paints the selected agent's terminal cell-by-cell into that half of the buffer via a `PaneSource`: in `--mock` it locks the local `alacritty_terminal::Term` directly (`SessionManager::with_term`); everywhere else it paints the last `GridSnapshot` the daemon streamed for the watched agent (see "Daemon + Attach Protocol").

Each tree row right-aligns `<status> <pct>%` in dim gray (red/bold for `blocked`, matching its badge); the name truncates with `…` to whatever width remains, computed by the pure `format_tree_row`/`truncate_with_ellipsis` helpers. The status bar shows "`N running`" normally, or "`N running · M blocked`" once any agent needs attention.

Status badges: `●` running · `!` blocked (needs you — permission pending) · `◌` idle (finished responding / a not-yet-started root) · `✓` done (explicit push, or a clean PTY exit) · `✗` error (unexpected process exit) · `…` spawning

**Keybinding house style: nvim.** Navigation follows nvim conventions — `j`/`k` within a list, `Ctrl-h`/`Ctrl-l` to move between panes, like nvim window navigation. New bindings should extend this vocabulary, not invent a parallel one — and must never require a prefix-key/chord model. One hard constraint: keys that agents' own TUIs rely on (e.g. `Ctrl-j` = Claude Code's insert-newline) must pass through to a focused agent pane untouched — `Ctrl-h` is the *only* key Overseer intercepts while a pane is focused (real Backspace still works: terminals send `DEL`, not `^H`).

Every tree-focus action below is remappable via `[keybindings]` (PHASE5B.md) — the table shows the defaults. Two things stay fixed regardless of config: `Ctrl-h` (pane-focus interception — see the paragraph above; making this remappable would invite a user to steal a key their agent's own TUI needs) and the scrollback keys (`Ctrl-u`/`Ctrl-d`/`Ctrl-y`/`Ctrl-e`/`G`, next section) — neither was ever in scope for `[keybindings]`. `Enter`/`o` and `Ctrl-C` also stay fixed *as extra aliases* for `jump_in`/`quit` even if those actions are remapped to something else.

| Key (default) | Action |
|-----|--------|
| `j` / `k` | Navigate agent tree (tree focus only) |
| `<space>` | Fold/unfold the selected agent's children |
| `Ctrl-l` / `Enter` / `o` | Jump in — moves keyboard focus into the selected agent's pane, if it's alive |
| `Ctrl-h` | From inside a focused pane, jump back out to the tree — the only key a pane intercepts; everything else, Ctrl-c included, forwards to the agent |
| `n` | Spawn a root: a bare shell in a chosen repo (default cwd) — no agent launched, run your own |
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

### Search

`/` opens a centered input (same modal chrome as spawn); as you type, the tree shows only agents whose name fuzzy-matches the query, plus every ancestor of a match (dimmed, kept for context — e.g. a matching child keeps its parent row visible so you can still see whose child it is). Matching is a pure `fuzzy_match(query, name) -> Option<u32>`: case-insensitive, in-order subsequence, with a score that rewards contiguous runs over scattered ones (so typing `au` ranks `auth-module` above a node that only matches `a` and `u` far apart). `Enter` moves the *real* cursor to the current selection if it still matches, else the first match in tree order, and closes the prompt; `Esc` closes it without moving anything. Nothing about the real cursor changes while you're still typing — only what the tree list renders.

### Help

`?` (itself remappable) opens a centered popup listing every binding — generated from the live `Keybindings` struct (`ui::help_rows`), never a hardcoded string, so a remap or a newly-added action can't silently drift out of sync with what's shown. Includes the fixed keys too (`Enter`/`o`, `Ctrl-C`, `Ctrl-h`), labeled as fixed. Any key closes it.

### Scrollback

While tree-focused, `Ctrl-u`/`Ctrl-d`/`Ctrl-y`/`Ctrl-e`/`G` scroll the *selected* agent's pane — a read-only preview in that state, so these never collide with a real agent TUI's own use of the same keys (readline's `Ctrl-u` kill-line, Claude Code's own scrolling). They are deliberately unavailable once a pane is focused: `Ctrl-h` remains the *only* key a focused pane intercepts, so `Ctrl-u`/`Ctrl-d` reach the agent untouched there (verified against a real shell: typing text then `Ctrl-u` while focused clears it via the shell's own readline, not Overseer).

The scrolled offset resets to the live bottom on cursor move (`j`/`k`, and any drop-driven cursor shift — handled in one place, by comparing the selection against the previous frame's) and on jump-in (`Ctrl-l`/`Enter`/`o`), so you never end up interacting with a pane mid-scroll. The pane border shows the state: `" agent [scrolled ↑N — G to follow] "` while scrolled, reverting to `" agent "` at the bottom.

Scrolling happens where the real `Term` lives — the daemon (`SessionManager::scroll_display`/`scroll_to_bottom`/`display_offset`, thin wrappers over `alacritty_terminal`'s own `Scroll::Delta`/`Scroll::Bottom`). A daemon-attached TUI sends `Request::Scroll { delta }` / `Request::ScrollToBottom` on the attach connection — no `agent_id`, since both only ever apply to whichever agent that connection is currently watching — and the daemon replies immediately with a fresh `GridSnapshot` (scrolling doesn't touch the PTY, so it never sets the dirty flag the normal output poll relies on). `--mock` mode calls `SessionManager` directly, no round trip needed.

### Spawn Data Flow

```
Root agent runs: overseer spawn --name "write-tests" --task "write tests" --adapter claude

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
branch/worktree on startup, per the overseer-child skill, and its own
SessionStart hook flips it from Spawning to Running moments later.
```

`overseer start` (launch a root) is a *different* path — no adapter, no task: it registers `role=root`, `status=idle`, names the node after the repo, and launches a bare shell (`$SHELL`) instead of `adapter.spawn_command(ctx)`. Whatever you run inside that shell (e.g. `claude`) inherits the injected identity env vars from the PTY itself and reports its own status via the same push hooks — Overseer never detects or launches it.

---

## Config

`~/.config/overseer/config.toml`. **Implemented:** `[defaults]`, `[adapters.*]`, `[notify]`, `[keybindings]`, and `[theme]` — all below. `[defaults]`/`[adapters.*]` load once at daemon/mock startup (adapter resolution); `[notify]`/`[keybindings]`/`[theme]` load independently in the TUI process itself (PHASE5B.md), since they're all properties of *your* terminal/desktop, not the daemon's. A missing or invalid file falls back to the built-in default; a bad *value* for one field (an unknown action name, an unparseable key, an unrecognized color) warns on stderr and keeps that field's own default — never a hard error, and never blocks startup.

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

[adapters.aider]
command = "aider"
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

A child spawn resolves its `command`/`extra_args` from `config.adapters[name]`, not from the adapter name itself — this is what lets `--dangerously-skip-permissions`-style flags actually reach the launched process, and lets a user point "claude" at a custom binary or wrapper. An adapter name with no entry in `config.adapters` is the same `UnknownAdapter` error as a name with no `AgentAdapter` impl at all.

`[notify]` (see "Attention Surfacing" above): every channel is independently switchable off. `bell` defaults **on** (a terminal bell is inert unless the user's own terminal turns it into something loud); `mode` defaults **off** (desktop notifications are the louder, opt-in channel). `"blocked+idle"` also notifies on `→idle`, for long tasks where "it finished responding" is worth a ping on its own.

`[keybindings]` (PHASE5B.md): a key is `j`/`D`/`/` (single char, case-sensitive) or `ctrl-<char>` (case-insensitive on the letter — Ctrl+A and Ctrl+a are the same physical keystroke). Two actions bound to the same key is a startup warning, not an error — the action declared later wins, deterministically (see `config::keybindings::Action::ALL`'s order). `Ctrl-h` (pane interception) and the scrollback keys are **not** in this table at all — see the house-style note above for why. Every binding is reflected live in the `?` popup.

`[theme]` (PHASE5B.md): colors only, nothing else — `Blocked`'s bold weight, for instance, is fixed. `Theme::default()` is asserted (in a test) to reproduce the exact colors Overseer shipped with before `[theme]` existed, so this section existing can't silently change anyone's look who never touches it.

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

**Runtime dependencies:** `git`. Standard on macOS/Linux. (No `tmux` — Overseer owns its own PTYs now.)

---

## Distribution

Single statically-linked binary. Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`. Homebrew tap: `nikita-ivanov/tap/overseer`. GitHub Actions handles cross-compile, release assets, and tap formula updates.

---

## Specs & Planning Docs

Implementation plans (`PHASE*.md`) and research notes live in **`.specs/`**, which is **gitignored** — they are local working documents that drive development, not part of the distributed repo. Every new spec/phase plan goes into `.specs/`; never commit one to the repo root, and never reference a spec from code or committed docs — once a phase ships, its spec is disposable and the code/AGENTS.md must stand on their own.

---

## Best Practices

- **IPC is the only shared channel.** Agent ↔ overseer communication always goes through the Unix socket. Never write to shared in-process state from an agent context.
- **The "no grandchildren" rule lives in the IPC server,** in the `spawn` handler. Not in the TUI, not in adapters. One place, always enforced.
- **`SessionManager` is the only terminal-backend boundary.** No `alacritty_terminal` imports outside `session/` and the pane renderer (`ui/term_pane.rs`) — this now includes the daemon's `Term` instances, never touched from `ipc/server.rs`'s attach handling except through `SessionManager`'s own methods (`grid_snapshot`, `take_dirty`, `write`, `resize_all`).
- **Parse functions are pure.** Functions like `parse_session_line` take a `&str` and return a value — no process spawning, no I/O. This makes them trivially testable.
- **`AgentNode` is a data model, not a handle.** It does not own a PTY. Session handles live in `SessionManager`, keyed by `AgentId`. Overseer holds no worktree state at all — that's the agent's.
- **Status is push, not pull.** Agent hooks POST status changes to the socket; the daemon POSTs registry/output events to attach clients the same way. Overseer never infers status from PTY output, and the TUI never polls the daemon for tree state.
- **`ui/` is a render layer only.** No business logic. All state mutations go through `App` / `AgentTree` / `SessionManager` methods.
- **One code path per request, regardless of backend.** `App::dispatch`/`with_tree`/`write_input`/etc. branch on `Backend::{Mock, Daemon}` in exactly one place (`app.rs`) — `tui.rs` and `ui/` call the same methods either way and never match on the backend themselves (bar the one `PaneSource` translation in `run_app`, which is `ui`-shape glue, not business logic).

## What to Avoid

- **No MCP transport.** The choice of Unix socket + hooks is intentional — no token overhead, no plugin registry approval, works locally out of the box.
- **Don't let children spawn children.** It's a hard server-side constraint, not a UI hint. A child calling `spawn` is rejected, full stop. The tree is exactly two levels: roots and their children.
- **Don't hardcode adapter binary paths.** Always resolve through the adapter config so users can point to a custom binary or wrapper.
- **Don't add agent status polling.** If hooks aren't firing, the fix is in `overseer install` (the installed hooks), not in adding a background poller. Same rule for the TUI itself — if it's missing tree updates, the fix is in the registry's broadcast or the attach connection, not a poll loop.
- **Don't reimplement git.** No worktree creation, no branching, no merging, no `git worktree` anywhere. Agents own their isolation. Overseer's only git use is read-only display info (repo name, current branch).
- **Don't write into the user's repo.** All agent config (skill, hooks) is installed at the user level by `overseer install`. Launch injects env only.
- **Don't skip the confirm prompt for `d`/`D`/`Q`.** Killing a running agent's session (or the whole daemon) interrupts in-flight work — confirm first.
- **Don't make quitting kill agents.** `q`/`Ctrl-C` must exit the TUI without touching any session or the daemon — it's a detach. `d`/`D` kills one agent, `Q` kills everything plus the daemon; both are deliberate, both confirm.
- **Don't add a second way to end the daemon process.** `Request::Shutdown`'s handler asks `ipc::server::run`'s accept loop to stop and lets `main` return — no `std::process::exit`. A response that's still in flight when the process exits is a real bug (confirmed once via `tokio::sync::Notify::notify_waiters` losing a wake under exactly this race — use `notify_one`, which stores a permit, for any future "tell the other task to stop" signal here).
- **Don't add a `Request::Drop`-with-a-flag for root drops.** Root-allowed drop is `Request::TuiDrop`, a distinct wire request only the TUI's key handling constructs — a caller-supplied bool on the existing `Drop` request would let any script opt out of the restriction it exists to enforce.
- **Don't assume `alacritty_terminal` exposes raw PTY bytes.** It doesn't, not without reimplementing its mio/signalfd event loop — that's why the attach protocol streams rendered `GridSnapshot`s instead of bytes for a client-side `Term`. Re-verify against the installed version before trying the raw-byte approach again; if a future version adds a public tap, that's a contained change to `session::pty` + `ipc::protocol`, not a redesign.
