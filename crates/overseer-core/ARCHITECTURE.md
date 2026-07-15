# overseer-core

The client-agnostic core of Overseer: everything the daemon, the CLI, the TUI,
and any future frontend share. A library crate — internal path dependency,
never published on its own. Workspace-level picture and the "what changes
where" guide: [../../ARCHITECTURE.md](../../ARCHITECTURE.md).

## Module map

```
src/
├── session/          PTY + terminal-emulator management, keyed by AgentId
│   ├── pty           SessionManager: owns one PTY + terminal emulator per agent — the only file
│   │                 in the workspace that imports alacritty_terminal. Renders GridSnapshot DTOs and
│   │                 tracks a per-agent content-generation counter (bumped on new PTY output)
│   └── keys          Crossterm key/paste/mouse -> PTY escape-byte encoders, parameterized by the neutral
│                     TermModes struct (input path for a focused pane); the one crossterm import in
│                     core — kept here rather than splitting the encoder from its input type
├── agent/            Agent model and lifecycle
│   ├── node/status/  AgentNode, AgentStatus, AgentRole, AgentTree (id.rs: AgentId)
│   │   tree/id
│   ├── registry      AgentRegistry: in-memory tree of registered agents + a broadcast channel
│   │                 of RegistryEvent (Registered/Removed/StatusChanged/Shutdown) for attach clients;
│   │                 set_status's pushed_at staleness guard lives here
│   ├── hook          Pure Claude Code hook-payload parsing: blocked-vs-idle-nag classification
│   ├── adapters/     Pluggable per-agent-type behaviour
│   │   ├── mod       AgentAdapter trait (capabilities, install_files, spawn_command, env_inject) +
│   │   │             identity_env(); the *.md files are the installed skills/instructions
│   │   ├── claude    Claude Code adapter (user-level skills + hooks, launch cmd)
│   │   └── opencode  opencode adapter (auto-loaded plugin.js + instructions array)
│   ├── spawn         Orchestrates session launch + env injection + register — the one shared
│   │                 path under Start and Spawn (spawn_root_shell vs spawn_child_agent)
│   └── drop          Kills an agent's PTY (and, recursively, its subtree) + deregisters it
├── ipc/              Unix socket layer (tokio, newline-delimited JSON)
│   ├── server        Binds the socket; one-shot request/response *and* the attach event-stream
│   │                 loop (Watch/Unwatch/Write/Resize inward, AttachEvent outward); session-exit watcher
│   ├── handlers      dispatch: status, list, agent, start, spawn, drop, tui_drop, shutdown —
│   │                 depth-3/max-children admission is enforced here, in exactly one place
│   ├── protocol      Request / Response / AgentDto / AttachEvent / GridSnapshot / ColorDto
│   │                 wire types (serde); snapshots include input-relevant terminal modes
│   └── client        One-shot sync client used by CLI subcommands and daemon reachability probes
├── daemon            Daemon process bootstrap: socket path resolution, flock lockfile,
│                     detached auto-spawn (setsid) with retry/backoff for a client to attach to
├── kill              `overseer kill`: forceful fallback for an unreachable daemon — graceful
│                     Shutdown attempt first, then SIGKILL by lockfile pid (ps-scan fallback),
│                     orphaned-PTY cleanup, stale socket/lockfile removal
├── install           `overseer install/uninstall <agent>`: writes adapters' user-level files
├── settings          Pure JSON merge/remove for Claude's settings.json hooks (incl. legacy
│                     untagged-entry recognition — see is_overseer_entry)
├── git               Read-only git info via CLI (repo name, current branch) — no worktrees
├── notify            Pure status-transition diff (notify::status_transitions) driving the
│                     TUI's bell/desktop notifications — the diff logic is core, the emission
│                     is the frontend's
├── config/           TOML config (~/.config/overseer/config.toml): Config{defaults, adapters,
│                     notify, keybindings, theme}. Missing/invalid file falls back to a built-in
│                     default; per-field a bad value falls back to that field's own default
│                     (stderr warning, never a hard error). Parsing lives here even for the
│                     TUI-only sections (keybindings/theme) so a second frontend never needs a
│                     second loader; Theme stores the wire-neutral ColorDto, not a ratatui color
└── test_env          Test-only (see below): process-global env-var lock + escape-sequence-to-
                      GridSnapshot render helpers
```

`tests/alacritty_boundary.rs` is the guard test pinning the pty.rs-only rule
below.

## Invariants owned by this crate

- **`alacritty_terminal` lives only in `session/pty.rs`.** `SessionManager`'s
  public method set (`launch`, `kill`, `write`, `resize_all`, `is_alive`,
  `scroll_display`, `scroll_to_bottom`, `display_offset`, `grid_snapshot`,
  `term_modes`, `generation`, `drain_exits`) is the entire terminal-backend
  contract; every signature uses only `GridSnapshot`/`TermModes`/std types.
- **No UI toolkit.** ratatui never appears here; crossterm only in
  `session/keys.rs`.
- **Status is push, not pull**, and every push carries `pushed_at` — the
  registry drops stale pushes (each hook fire is its own connection with no
  ordering guarantee).
- **Parse functions are pure** (`hook.rs`, `settings.rs`, `notify.rs`) — no
  I/O, no process spawning.
- **The depth-3 and child-cap rules live in the `spawn` handler** — not in any
  frontend, not in adapters.

## The `test-util` feature

Other workspace crates reach core's test helpers by depending on
`overseer-core` with `features = ["test-util"]` in `[dev-dependencies]`.
Behind `#[cfg(any(test, feature = "test-util"))]` live: `test_env` (env-var
mutation lock), `SessionManager`'s dry-run constructors + `is_dry_run`, the
`snapshot_from_bytes*` render fixtures, and the `RegisterArgs` re-export.
Add new cross-crate test helpers under the same gate — never widen them to
unconditional `pub` and never duplicate them in a consumer crate.
