# Research prompt: what should Overseer's terminal backend be?

Paste this whole file as your prompt. Take your time — this is a foundational
architecture decision, not a quick lookup.

## Context

Overseer is a Rust TUI ("IDE for agents") that orchestrates a hierarchy of AI
coding agents (Claude Code, etc.): you talk to **one root agent per repo**,
and it spawns **child agents** to do delegated work — a flat hierarchy (no
grandchildren). A ratatui-rendered sidebar shows every agent's status at a
glance (running/waiting/idle/done/error), and you can jump into any agent's
**real, live, interactive terminal** to type, approve permission prompts, or
just watch — critically, agents run full-screen TUIs themselves (Claude
Code's own interface), so whatever renders that pane has to render arbitrary
ANSI/VT output correctly, not just plain text.

It's currently built as a thin layer on top of **real tmux**: a private
`tmux -L overseer` server hosts everything, the TUI's own window is split
into two real tmux panes (sidebar + a permanent live pane showing the
selected agent's session via `switch-client`), and jumping into an agent
means moving tmux's own pane focus. This works, but has real problems:

- **tmux's prefix-key model (`Ctrl-b` then a key) is genuinely unfriendly**
  to anyone who doesn't already use tmux daily. The maintainer (a non-tmux-
  power-user) specifically dislikes it. "Learn tmux to use this tool" is a
  bad onboarding story for a product aimed at a general audience of agent
  users, not terminal power users.
- We hit **real, hard-to-predict bugs** building this: tmux's own status bar
  bleeding through in unexpected places (had to explicitly disable it
  server-wide), a redraw-corruption bug from a size-mismatch between when
  ratatui initializes vs. when the pane actually gets resized, and a subtle
  "nested tmux session inside a pane already on the same server refuses to
  attach without unsetting `$TMUX` first" gotcha that only a live spike
  caught. tmux's behavior is powerful but has a lot of implicit, load-bearing
  state that's easy to get subtly wrong.
- If the user is *already* inside their own tmux session when they launch
  Overseer, they get nested tmux (their own status bar layered with ours),
  which is confusing and outside our control to fully clean up.

Separately, we looked at **herdr** (`github.com/ogulcancelik/herdr`, cloned
locally for reference) for UI inspiration — it's marketed as "tmux rebuilt
for agents" but is actually a **from-scratch terminal multiplexer**: it uses
`portable-pty` to own PTYs directly, FFI bindings to **libghostty** (Ghostty
the terminal emulator's Zig core) as its own VT100/ANSI parser and screen
state engine, and ratatui only for its own chrome. Its terminal-emulation
code alone (`src/pane/terminal.rs`) is 4,400+ lines. That's a lot of surface
area and ongoing maintenance burden for a small project to take on.

## The question

**Should Overseer keep building on tmux, switch to a different backend
(Zellij is the specific alternative to evaluate — a Rust-native terminal
multiplexer marketed as more modern/discoverable than tmux), or build a
lighter-weight custom PTY + terminal-emulation layer (herdr's approach, but
scoped down)?**

Research and compare, concretely:

1. **tmux (status quo).** What's the realistic ceiling on hiding its prefix-
   key model and quirks behind Overseer's own UX? Is there a way to run
   Overseer's own keybindings as the *only* thing the user ever needs to
   know, with tmux fully invisible as an implementation detail? Or is some
   irreducible tmux-shaped leakage (status bars, prefix keys, session
   nesting) unavoidable once you're really building on top of it?

2. **Zellij.** Does it expose a way to be driven/embedded programmatically
   (a library crate, a scriptable CLI/IPC/plugin API comparable to what we
   use tmux's CLI for — creating sessions, splitting panes, sending keys,
   capturing pane content, detecting when a pane's process exits), or is it
   only usable as a standalone interactive multiplexer the way tmux is? Does
   its default keybinding model actually solve the "unfriendly to newcomers"
   problem, or does it have its own prefix-key/modal learning curve once you
   look past the marketing? What's its session persistence / detach-reattach
   story? Is it meaningfully more reliable/predictable to script against than
   tmux, or does it have its own set of gotchas? License and Rust-ecosystem
   fit (can we depend on it as a crate, or would we be shelling out to a
   separate `zellij` binary the same way we shell out to `tmux` today)?

3. **Custom PTY + terminal-emulation layer**, herdr-style but scoped down.
   What's the actual minimum viable version of this — not herdr's full
   4,400-line hand-rolled emulator, but something like `portable-pty` (spawn
   and own the PTY) + an existing, already-written Rust terminal-emulation
   crate to parse VT/ANSI output into a renderable grid (research options —
   e.g. `vt100`, `alacritty_terminal`, `wezterm-term` — and compare
   maturity, API ergonomics, how much of "faithfully render a full-screen
   TUI like Claude Code's own interface" each actually gets you for free vs.
   how much you'd still have to build). What do we gain (full control over
   UX, no prefix keys, no other multiplexer's quirks, one less external
   runtime dependency) versus what we give up (we own persistence/detach-
   reattach ourselves now, more code to maintain, more surface area for the
   exact kind of rendering bugs we just spent a session debugging in tmux —
   except now they'd be *our* bugs in *our* VT parser instead of tmux's).

4. **Anything else worth considering** — other Rust-ecosystem multiplexers
   or embeddable-terminal crates, or a hybrid (e.g. tmux for persistence,
   something else for rendering)?

## Evaluation criteria, in rough priority order

1. **Friendliness for someone who's never used tmux/Zellij/etc.** — this is
   the whole reason we're reconsidering. No mandatory prefix-key muscle
   memory should be required for Overseer's actual, everyday keybindings.
2. **Faithful rendering of arbitrary full-screen TUI agent output** — an
   agent's own interface (Claude Code, etc.) has to render correctly when
   you jump into or preview it. This is non-negotiable.
3. **Persistence** — agents keep running when you close your terminal/laptop;
   you can reattach later. (A real herdr/tmux strength worth preserving.)
4. **Implementation and ongoing maintenance cost** for a small project (one
   maintainer, not a funded team) — be honest about what "build our own
   terminal emulator" actually costs over time, including edge cases
   (resize handling, mouse support if we ever want it, unicode/wide-char
   handling, etc.) versus depending on a mature, already-solved multiplexer.
5. **Reliability/predictability** — how much implicit, easy-to-get-wrong
   state does each option have, based on what you can find (docs, issues,
   source) — not just marketing claims.
6. **Cross-platform** — macOS + Linux at minimum (see `AGENTS.md`'s
   distribution targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
   `x86_64-unknown-linux-musl`).

## What NOT to relitigate

The **product** direction is settled and out of scope for this research:
flat root+children hierarchy, root is the sole communication point, root
fans work out to children, a sidebar shows fleet status at a glance, jump
into any agent's real terminal to intervene. Keep it **simpler than herdr**
— no workspaces/tabs/nested-pane-splitting, just this one flat model. The
only open question is *what renders the terminal panes and manages
persistence underneath* — tmux, Zellij, or something custom.

## Deliverable

A concrete recommendation, not just a survey — pick one (or a specific
hybrid), and justify it against the criteria above with the actual tradeoffs
made explicit, including what we'd be giving up. If the answer is "stay on
tmux but change how we use it," say specifically what changes. If it's
"switch to X," describe the minimum viable integration path and flag the
riskiest unknowns worth spiking before committing (the way this project's
`PHASE5.md` treated the switch-client/jump-in mechanic as a throwaway-spike
unknown before building on it — same discipline here).
