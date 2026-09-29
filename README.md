# Overseer

**An IDE for agents.**

Overseer is a terminal-native TUI for observing and steering a fleet of parallel AI coding agents from a single window, instead of juggling a pile of terminal tabs. Built in Rust: an ordinary alt-screen app with no bundled multiplexer — each agent is a PTY Overseer owns directly, emulated in-process and rendered straight into the same frame — with a small CLI and local Unix socket API for agent coordination.

The agents are already smart. Overseer doesn't reimplement what they do — it doesn't manage git worktrees, branches, or merges; agents handle their own isolation. Overseer gives their work a visible delegation loop: **assign → observe → intervene → receive result → review → archive**. A parent becomes the team lead for the children it spawns.

## What it is

Plainly: Overseer is an **agent orchestrator with an observability layer on top**. A background daemon owns a registry of every agent you're running and the PTY each one lives in; the TUI is just one client of that daemon, so quitting it detaches rather than kills anything — the fleet keeps running, and a later launch reattaches to exactly what was there. One **workspace** per repository is where you talk to your own agent directly; it can delegate real sub-tasks to children through a small `overseer spawn` API, each showing up as its own row in the tree the moment it exists. Depth is capped at three, fan-out is capped per parent, and destructive TUI actions ask for confirmation; explicit CLI drop/shutdown/kill commands act directly.

What that buys you:

- **See the whole fleet at a glance** — status, blocked/idle/done/error, and how long each has been stuck, without opening a terminal tab per agent.
- **Jump into any agent** to approve a permission prompt or nudge it, then jump back out — a real, interactive PTY, not a read-only log tail.
- **Managed delegation stays visible.** Each managed child gets a tree row and its own terminal. Native harness subagents are not yet displayed.
- **Results outlive terminals.** Assignments, completion summaries, artifact references, and reported validation remain retrievable after a child exits or the daemon restarts. A parent can wait, review, accept, and archive through the CLI.

## What it is not

- **Not a sandbox.** Every agent under one daemon fully trusts every other — any agent can write into a sibling's PTY, forge another's status, or drop it. This is a deliberate trade-off, not an oversight (see `AGENTS.md`'s Security section): Overseer's isolation is organizational — a tree you can see and prune — not a security boundary between agents. Don't run mutually-distrusting agents under one daemon.
- **Not a git tool.** Overseer never creates branches or worktrees and never merges anything. Agents own their own isolation; Overseer's only use of git is read-only, for display (repo name, current branch).
- **Not an autonomous supervisor.** There's no loop that automatically re-prompts an idle or blocked agent. A human or a parent agent using `tasks`, `task`, and bounded `wait` requests decides what happens next — Overseer surfaces attention, it doesn't act on it.
- **Not an MCP server.** Agents talk to Overseer over one plain Unix socket with a tiny newline-delimited JSON protocol — the coordination layer works locally without a model API of its own. Role instructions still consume model context, and harnesses retain their own provider and trust requirements.

## Why it exists

*(My own reasoning for building this — expect it to keep shifting as the project grows.)*

I kept using Claude Code, opencode, and similar tools that now all ship some form of built-in multi-agent support — subagents, background tasks, whatever a given harness calls it internally — and every one of them makes that delegation invisible from where I'm sitting. The parent quietly spins up help, folds the results back into its own context, and I never see any of it happen — I can't watch it work, and I can't step in if something goes sideways until I'm handed a summary of a process I never had eyes on. That's backwards from how I actually want to work with a fleet of agents: I want to see every agent that's running, in real time, and be able to walk into any single one of them the moment I need to.

So Overseer isn't trying to make agents smarter — they already are. It exists purely to put a window on top of what they're doing: every agent gets its own visible row the instant it's spawned, an honest status instead of a black box, and a real terminal to jump into instead of a transcript to wait for. Visibility comes first; the orchestration underneath is just what visibility requires.

## Architecture, at a glance

```
overseer daemon (background, one per user, auto-spawned by the TUI)
├── AgentRegistry, TaskStore, SessionManager, Config, git/   ← owned by the daemon, not the TUI
├── IPC socket  $XDG_RUNTIME_DIR/overseer/daemon.sock
├── retained task journal beside the socket
└── attach connections: registry events + rendered terminal snapshots

overseer (TUI) = attach client              overseer <subcommand> = one-shot client
```

A Cargo workspace of two crates: `overseer-core` (library — agent model, sessions, IPC, daemon, config; everything client-agnostic) and `overseer` (the binary — CLI subcommands, daemon entrypoint, and the TUI). `AGENTS.md` is the full spec — architecture, IPC protocol, adapter model, config, and the design rules that keep it that way; this file is just the pitch.

## Getting started

No prebuilt binaries or Homebrew tap yet — build from source. Requires the Rust toolchain.

```sh
cargo install --git https://github.com/nikitaivanovvff/overseer overseer
```

This clones, builds `--release`, and installs the `overseer` binary to `~/.cargo/bin` (already on `PATH` if you have Rust set up). To hack on it locally instead, clone and `cargo build --release`; the binary is under your configured Cargo target directory’s `release/` subdirectory.

Overseer supports **Claude Code**, **opencode**, and an **experimental Codex adapter**. Install support for whichever you use, once, at the user level. If you installed pi support with an older Overseer release, run that release's `overseer uninstall pi` before upgrading to remove its user-level files.

```sh
overseer install claude   # or opencode / codex
```

`install` only ever writes at the **user level** — never into the project repo you happen to run it from, so it never shows up in `git status` for any codebase you use Overseer on:

- **Claude Code**: lifecycle hooks in `~/.claude/settings.json`. SessionStart supplies the current role bootstrap, including supported resume/compaction starts. Upgrading removes the old mandatory root/child skills. Owned hooks are tagged, preserving unrelated hooks.
- **opencode**: an auto-loaded plugin at `~/.config/opencode/plugin/overseer.js` (or `$XDG_CONFIG_HOME/opencode/plugin/overseer.js`). It injects the main session’s role through a context transform and refreshes it during compaction; native nested sessions are filtered. Upgrading removes Overseer’s old unconditional role instruction entries/files. The context interfaces are version-sensitive and were checked against OpenCode 1.17.20/plugin declarations 1.17.13.
- **Codex, experimental**: owned hooks in `$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`). After installing, review/trust them through Codex’s `/hooks` UI and restart. Launch manually inside a workspace with `codex --no-daemon`; spawned Codex children get this flag automatically so the session identity reaches the harness. Launch syntax was checked against Codex 0.159.0, but a live provider-backed lifecycle run remains unverified. Startup, user-submit, stop, and interrupt reporting are experimental; permission, provider-limit, and context-usage telemetry are unsupported. Native subagent events are ignored, and `full_auto_mode` adds no Codex bypass flags.

All integrations use one shared `overseer context` bootstrap for the current live registry identity. A workspace leads its children; a depth-2 contributor can also lead children; a depth-3 leaf works inline. Sessions outside Overseer receive no role instructions. This is a live identity lookup, not a full versioned integration-health handshake.

For each adapter, `overseer install <agent> --uninstall` (or the equivalent `overseer uninstall <agent>`) removes exactly what was installed — nothing else in your settings/config is touched.

Then run `overseer`. It spawns a background daemon on first launch, and `n` immediately opens a bare shell in the directory where you launched Overseer. Use `overseer start --cwd <path>` from another terminal for a different directory. Run your own agent inside it; Overseer picks up its status automatically via the hooks `install` just wired in.

Cross-compiled release CI exists (`.github/workflows/release.yml`) but is currently manual-trigger only while `cargo install --git` is the primary distribution path — no version has been tagged yet.

## Delegate and retain results

From a managed parent session:

```sh
overseer spawn --name tests --task "Add regression coverage for the parser" --adapter claude
# Use the returned child ID:
overseer tasks
overseer wait CHILD_ID --timeout 30
overseer task CHILD_ID
```

The child reports its result from its own working directory:

```sh
overseer complete --summary "Added parser regression coverage" \
  --artifact "commit: <hash>" --validation "cargo test -p parser passed"
```

After reviewing the actual changes, the parent records its decision:

```sh
overseer accept CHILD_ID
overseer archive CHILD_ID
```

`tasks` returns compact summaries for your direct children when run inside a managed session; outside one it lists all unarchived tasks. Use `--parent <id>` to select a parent and `--archived` to include archived records. `task <id>` returns the full assignment/result. `wait` blocks without polling for a result or interruption, for 0–60 seconds (30 by default), then reports an explicit timeout if still assigned. It does not wait for permission attention or wake an idle model automatically.

There is one assignment per child session in this version. A child created without a task through the TUI can receive `overseer assign <id> --task "..."`; this records metadata and **does not type a prompt** into its terminal. Use a new child for another assignment. Legacy `overseer status done` changes lifecycle only; it does not save a result.

Task state is `assigned` → `complete` → `accepted`; an unfinished task becomes `interrupted` when its session exits, is dropped, or the daemon restarts. Archive hides accepted/interrupted records from default listings. Accept/archive never merge code, stop sessions, or delete files. Completion records the caller’s working directory and its reported artifacts/validation; review is still the parent’s responsibility.

The journal lives beside the socket (`daemon.tasks.json` for the default `daemon.sock`). It keeps up to 512 records, with an additional byte budget that can reject assignments earlier. New assignments prune the oldest archived records when space is needed; if space cannot be reclaimed, admission fails rather than discarding unreviewed results. Restart restores history, not terminal processes. The runtime directory may be cleared on reboot, so this is daemon-restart persistence, not a reboot-survival guarantee. The TUI currently shows live sessions; task inbox, review, and history views are still planned.

## Development workflow

Keep local interaction responsive with a few sessions and bounded event bursts. Fleet stress testing is optional for a specific diagnostic need or explicit request. Preserve output/scroll coalescing when changing hot paths. Use the reusable [integration workflow skill](.agents/skills/overseer-integration/SKILL.md) when updating harness support.

## Configuration

Everything below is optional. Overseer runs on built-in defaults if `~/.config/overseer/config.toml` doesn't exist, and a missing or invalid *value* for one field just warns on stderr and keeps that field's own default rather than failing to start. `[defaults]`/`[adapters.*]`/`[danger_zone]` are read once by the daemon (or `--mock`) at startup; `[notify]`/`[keybindings]`/`[theme]` are read independently by the TUI process, since they're properties of *your* terminal, not the daemon's.

```toml
# ~/.config/overseer/config.toml

[defaults]
adapter = "claude"                # harness a new workspace assumes, and what a spawned child inherits when --adapter is omitted
max_children = 8                  # cap on direct children per parent (workspace or child) -- keeps the tree readable and bounds PTY/token cost

[danger_zone]
full_auto_mode = false   # opt-in: supported adapter bypass flags (Claude/OpenCode only) -- see "Danger Zone" below. Off by default; leave it off unless you mean it.

[adapters.claude]
command = "claude"                # binary to launch -- point this at a wrapper or a non-$PATH build if you need to
extra_args = []                   # flags appended before the task text, e.g. ["--dangerously-skip-permissions"] to bypass prompts for every claude child (see Danger Zone)

[adapters.codex]
command = "codex"
extra_args = []                   # experimental; no full_auto_mode bypass

[adapters.opencode]
command = "opencode"
extra_args = []                   # e.g. ["--auto"] to auto-approve anything opencode wouldn't otherwise explicitly deny

[notify]
bell = true      # terminal BEL (\a) on any agent's ->blocked transition -- on by default, harmless if your terminal doesn't ring it
mode = "off"     # desktop notifications: "off" (default), "blocked" (fires on ->blocked), or "blocked+idle" (also fires on ->idle)

[keybindings]      # tree-focus-only bindings; every entry below is optional and independently remappable
spawn_root = "n"   # immediately open a workspace shell at Overseer’s cwd
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
- **For adapters with a supported bypass flag**: set `[danger_zone] full_auto_mode = true`. Overseer appends the equivalent auto-approve flag to every adapter that has one (`--dangerously-skip-permissions` for Claude Code, `--auto` for opencode) without you having to hand-list it per adapter. Codex is excluded; its existing approval and sandbox settings are preserved.

For an adapter using these bypass flags, the consequence is the same: an agent running this way can take any action its harness allows — edit files, run shell commands, whatever the tool permits — without asking, unattended. Both flags are labeled dangerous by their own tools: Claude Code's is literally named `--dangerously-skip-permissions`, and opencode's own docs describe `--auto` as auto-approving permissions that aren't explicitly denied "(dangerous!)". Turn this on only if you mean it.

## Status

Actively developed, pre-release (`0.1.0`), no tagged versions. The retained CLI task loop and shared role bootstrap are shipped. TUI task history/review, a full integration-health handshake with harness invocation epochs, and native subagent visibility remain planned. See `AGENTS.md` for the runtime contract and capability limits.
