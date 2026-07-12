---
name: verify
description: How to verify Overseer changes end-to-end by driving a throwaway daemon over its real Unix socket — build, launch, speak the attach protocol, observe GridSnapshots. Use when verifying daemon/IPC/TUI changes at runtime instead of via cargo test.
---

# Verifying Overseer at runtime

The daemon's Unix socket **is** the product surface — the TUI is just a
client of it. Most changes (IPC, session, scroll, status, spawn) can be
verified by driving a throwaway daemon with newline-delimited JSON, no
interactive terminal needed.

## Launch an isolated daemon

Never touch the live user daemon (`$OVERSEER_SOCKET`, default
`/tmp/overseer-$UID/daemon.sock`). Give the throwaway its own socket:

```bash
cargo build                       # binary at target/debug/overseer
TMP=$(mktemp -d)
target/debug/overseer daemon --socket "$TMP/daemon.sock" &   # backgrounded
# wait for the socket file to appear (~100ms)
```

Shut it down at the end by sending `{"cmd": "shutdown"}` on any
connection — don't leave orphan daemons (each agent PTY is `setsid()`'d
and survives a bare kill of the daemon).

## Wire protocol crib sheet

Requests are internally tagged `{"cmd": "<snake_case variant>", ...}`
(see `crates/overseer-core/src/ipc/protocol.rs`); events come back as
`{"event": "<snake_case>", ...}`. One JSON object per line, both ways.

- One-shot (own connection, one reply):
  `{"cmd":"start","cwd":"<dir>"}` → `{"ok":true,"data":{"agent_id":...}}`,
  `{"cmd":"list"}`, `{"cmd":"shutdown"}`
- Attach connection: send `{"cmd":"attach"}` first → one
  `{"event":"snapshot",...}` then a stream. Then:
  `{"cmd":"watch","agent_id":...}` (immediate `output` event),
  `{"cmd":"write","agent_id":...,"data":"seq 1 300\n"}`,
  `{"cmd":"resize","cols":100,"lines":30}`,
  `{"cmd":"scroll","delta":N}` / `{"cmd":"scroll_to_bottom"}`.

`Output` events carry a `grid`: `cells` is a **flat row-major
`Vec<Option<CellDto>>`** (`cols * lines` entries, `null` = blank; chunk
by `cols` to get lines), plus `display_offset` (0 = live bottom) —
assert on `display_offset` directly for scroll behavior.

## Gotchas learned the hard way

- **The shell eats the first write.** A `write` sent ~1-2s after `start`
  can lose its leading byte(s) while zsh initializes (`seq ...` arrives
  as `eq ...`). Send a bare `"\n"` first, wait ~1s, then the real
  command — and confirm from the grid text that it actually ran.
- **No history = every scroll is a clamped no-op** (0 output events,
  offset stays 0). Generate >1 screen of output before testing scroll.
- **Scroll replies are coalesced by design** (AGENTS.md "Scrollback"):
  N rapid scrolls within one 16ms poller tick → at most 1 Output event;
  clamped scrolls → none. Don't "fix" a burst returning one event.
- Read events with a short socket timeout in a loop for a wall-clock
  window; a working session emits `output` within ~16ms of a change.

A worked example script from the wheel-scroll-fix verification pattern:
attach → watch → seed history → burst 200 scrolls → expect 1 Output
event with the summed offset → hammer both clamps → expect 0 → final
`write` proves the connection never wedged.

## TUI-side changes

For pure `ui/`/`tui.rs` rendering logic there is no socket surface —
`cargo run -- --mock` gives seeded demo data with zero daemon/PTY risk,
but observing it needs a real terminal; say so rather than faking it.
The `examples/mouse-probe.rs` binary diagnoses terminal mouse behavior.
