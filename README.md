# Overseer

**An IDE for agents.**

Overseer is a terminal-native TUI for observing and steering a fleet of parallel AI coding agents from a single window, instead of juggling a pile of terminal tabs. Built in Rust: an ordinary alt-screen app with no bundled multiplexer — each agent is a PTY Overseer owns directly, emulated in-process and rendered straight into the same frame — talking to agents over a lightweight local Unix socket, no MCP overhead.

The agents are already smart. Overseer doesn't reimplement what they do — it doesn't manage git worktrees, branches, or merges; agents handle their own isolation. Overseer is the observability, routing, and approval surface on top of them.

## What it is

Plainly: Overseer is an **agent orchestrator with an observability layer on top**. A background daemon owns a registry of every agent you're running and the PTY each one lives in; the TUI is just one client of that daemon, so quitting it detaches rather than kills anything — the fleet keeps running, and a later launch reattaches to exactly what was there. One **workspace** per repository is where you talk to your own agent directly; it can delegate real sub-tasks to children through a small `overseer spawn` API, each showing up as its own row in the tree the moment it exists. Depth is capped at three, fan-out is capped per parent, and every drop/kill/shutdown path needs an explicit confirm.

What that buys you:

- **See the whole fleet at a glance** — status, blocked/idle/done/error, and how long each has been stuck, without opening a terminal tab per agent.
- **Jump into any agent** to approve a permission prompt or nudge it, then jump back out — a real, interactive PTY, not a read-only log tail.
- **Delegation stays visible.** Spawning a child is an observable event with a tree row and a status you can watch — not a silent in-context subagent call that only the conversation that made it can see.

## What it is not

- **Not a sandbox.** Every agent under one daemon fully trusts every other — any agent can write into a sibling's PTY, forge another's status, or drop it. This is a deliberate trade-off, not an oversight (see `AGENTS.md`'s Security section): Overseer's isolation is organizational — a tree you can see and prune — not a security boundary between agents. Don't run mutually-distrusting agents under one daemon.
- **Not a git tool.** Overseer never creates branches or worktrees and never merges anything. Agents own their own isolation; Overseer's only use of git is read-only, for display (repo name, current branch).
- **Not an autonomous supervisor.** There's no loop that automatically re-prompts an idle or blocked agent. A human, or the workspace agent you're talking to reading `overseer list`, decides what happens next — Overseer surfaces attention, it doesn't act on it.
- **Not an MCP server.** Agents talk to Overseer over one plain Unix socket with a tiny newline-delimited JSON protocol — no plugin registry, no token overhead, works offline out of the box.

## Why it exists

*(My own reasoning for building this — expect it to keep shifting as the project grows.)*

I kept using Claude Code, opencode, and similar tools that now all ship some form of built-in multi-agent support — subagents, background tasks, whatever a given harness calls it internally — and every one of them makes that delegation invisible from where I'm sitting. The parent quietly spins up help, folds the results back into its own context, and I never see any of it happen — I can't watch it work, and I can't step in if something goes sideways until I'm handed a summary of a process I never had eyes on. That's backwards from how I actually want to work with a fleet of agents: I want to see every agent that's running, in real time, and be able to walk into any single one of them the moment I need to.

So Overseer isn't trying to make agents smarter — they already are. It exists purely to put a window on top of what they're doing: every agent gets its own visible row the instant it's spawned, an honest status instead of a black box, and a real terminal to jump into instead of a transcript to wait for. Visibility comes first; the orchestration underneath is just what visibility requires.

## Architecture, at a glance

```
overseer daemon (background, one per user, auto-spawned by the TUI)
├── AgentRegistry, SessionManager, Config, git/   ← owned by the daemon, not the TUI
├── IPC socket  $XDG_RUNTIME_DIR/overseer/daemon.sock
└── attach connections: registry events + rendered terminal snapshots

overseer (TUI) = attach client              overseer <subcommand> = one-shot client
```

A Cargo workspace of two crates: `overseer-core` (library — agent model, sessions, IPC, daemon, config; everything client-agnostic) and `overseer` (the binary — CLI subcommands, daemon entrypoint, and the TUI). `AGENTS.md` is the full spec — architecture, IPC protocol, adapter model, config, and the design rules that keep it that way; this file is just the pitch.

## Getting started

Overseer currently supports two harnesses: **Claude Code** and **opencode**. Install support for whichever you use, once, at the user level. If you installed pi support with an older Overseer release, run that release's `overseer uninstall pi` before upgrading to remove its user-level files.

```sh
cargo build --release
./target/release/overseer install claude   # or opencode
```

Then run `overseer`. It spawns a background daemon on first launch, and `n` opens a workspace picker — pick a repo and it drops you into a bare shell there. Run your own agent inside it; Overseer picks up its status automatically via the hooks `install` just wired in.

No prebuilt binaries or Homebrew tap yet — cross-compiled release CI exists (`.github/workflows/release.yml`) but no version has been tagged. Building from source is the only path today.

## Configuration

Everything below is optional. Overseer runs on built-in defaults if `~/.config/overseer/config.toml` doesn't exist, and a missing or invalid *value* for one field just warns on stderr and keeps that field's own default rather than failing to start. `[defaults]`/`[adapters.*]` are read once by the daemon (or `--mock`) at startup; `[notify]`/`[keybindings]`/`[theme]` are read independently by the TUI process, since they're properties of *your* terminal, not the daemon's.

```toml
# ~/.config/overseer/config.toml

[defaults]
adapter = "claude"                # harness a new workspace assumes, and what a spawned child inherits when --adapter is omitted
max_children = 8                  # cap on direct children per parent (workspace or child) -- keeps the tree readable and bounds PTY/token cost
im_not_afraid_of_agents = false   # opt-in: auto-approve every spawned child's permission prompts -- see "Danger Zone" below. Off by default; leave it off unless you mean it.

[adapters.claude]
command = "claude"                # binary to launch -- point this at a wrapper or a non-$PATH build if you need to
extra_args = []                   # flags appended before the task text, e.g. ["--dangerously-skip-permissions"] to bypass prompts for every claude child (see Danger Zone)

[adapters.opencode]
command = "opencode"
extra_args = []                   # e.g. ["--auto"] to auto-approve anything opencode wouldn't otherwise explicitly deny

[notify]
bell = true      # terminal BEL (\a) on any agent's ->blocked transition -- on by default, harmless if your terminal doesn't ring it
mode = "off"     # desktop notifications: "off" (default), "blocked" (fires on ->blocked), or "blocked+idle" (also fires on ->idle)

[keybindings]      # tree-focus-only bindings; every entry below is optional and independently remappable
spawn_root = "n"   # open the "new workspace" prompt
spawn_child = "s"  # spawn a child under the selected agent
search = "/"       # fuzzy-search the tree by name
help = "?"         # open the live keybinding reference popup
# every other tree-focus action (j/k nav, <space> fold, d/D drop, Q shutdown, ...)
# is remappable the same way -- see AGENTS.md's keybinding table for the full list.
# Ctrl-h (leave a focused pane) and the scrollback keys (Ctrl-u/d/y/e, G) are
# fixed and deliberately not listed here -- remapping them could steal a key
# an agent's own TUI needs.

[theme]                 # status + chrome colors only -- named ratatui colors ("green") or hex ("#rrggbb")
running = "green"
blocked = "red"
idle = "dark_gray"
done = "blue"
error = "red"
spawning = "cyan"
border_focused = "yellow"
border = "dark_gray"
```

See `AGENTS.md`'s Config section for the full rules behind these (e.g. how `[adapters.*]` entries not in your file still keep their built-in defaults, and how key-binding collisions are resolved).

## Danger Zone

By design, Overseer does not bypass permission prompts for spawned children. A child asks for permission exactly like a human running the same harness would — that's the default, and it stays the default.

If you want a child to run unattended without those prompts, that's your call and your risk to take, and there are two ways to make it, both shown in the config example above:

- **Per adapter**: set `[adapters.<name>] extra_args` to whatever flags your harness accepts, e.g. `extra_args = ["--dangerously-skip-permissions"]` for Claude Code.
- **Blanket, for every configured adapter**: set `[defaults] im_not_afraid_of_agents = true`. Overseer appends the equivalent auto-approve flag to every adapter that has one (`--dangerously-skip-permissions` for Claude Code, `--auto` for opencode) without you having to hand-list it per adapter.

Either way, the consequence is the same: an agent running this way can take any action its harness allows — edit files, run shell commands, whatever the tool permits — without asking, unattended. Both flags are labeled dangerous by their own tools: Claude Code's is literally named `--dangerously-skip-permissions`, and opencode's own docs describe `--auto` as auto-approving permissions that aren't explicitly denied "(dangerous!)". Turn this on only if you mean it.

## Status

Actively developed, pre-release (`0.1.0`), no tagged versions. See `AGENTS.md` for what's shipped and what's still open.
