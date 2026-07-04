#!/usr/bin/env bash
#
# End-to-end lifecycle test for `overseer start`/`spawn`/`drop`, driven against a
# real overseer binary — no unit-test mocking involved. Overseer itself is a
# single ordinary process: no tmux server, no bootstrap. The only
# tmux involved here is our own *test harness*, hosting the TUI in a scriptable
# outer pane so we can drive it with `send-keys`/`capture-pane`, same as a human
# would type into a real terminal.
#
# Part A drives the CLI (`overseer spawn`/`drop`) directly — this is what an agent
# calling `overseer spawn` from its own shell actually does.
# Part B drives the TUI itself via `tmux send-keys` + `capture-pane`, since some
# behavior (root agents can only be dropped via the TUI, not the CLI) only exists
# on that path.
# Part C exercises the pane rendering + focus model (jump in/out) and the quit
# guard, against a stub agent binary that reports its own liveness.
#
# Requires: cargo, tmux, jq, git.

set -uo pipefail

# ── setup ─────────────────────────────────────────────────────────────────────

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

for tool in tmux jq git cargo; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

echo "building overseer..."
cargo build --quiet || { echo "build failed" >&2; exit 1; }
OVERSEER="$REPO_ROOT/target/debug/overseer"

WORKDIR="$(mktemp -d)"
STUBDIR="$WORKDIR/bin"
mkdir -p "$STUBDIR"
TEST_REPO="$WORKDIR/repo"
mkdir -p "$TEST_REPO"
MARKER_DIR="$WORKDIR/markers"
mkdir -p "$MARKER_DIR"
SOCK="$WORKDIR/overseer.sock"
HARNESS=overseer-harness-test

# Stub agent/shell binary: no real PTY-owning terminal emulator to fake here (the
# real one is alacritty_terminal, in-process) — this just needs to (a) stay alive
# so its session is inspectable, (b) prove it was actually launched with the
# right identity env, (c) prove it actually dies when killed. `kill()` sends
# SIGKILL directly to this process (session/pty.rs: some real agents, Claude
# Code included, don't die from a hangup alone, and blocking on Child::wait()
# for one that doesn't would hang the UI thread) — SIGKILL can't be trapped, so
# self-cleanup-on-exit isn't possible here. The marker file instead records this
# process's own pid at startup and is never removed; liveness is checked with
# `kill -0` against that pid from the outside, keyed by $OVERSEER_AGENT_ID
# (present for roots and children alike per identity_env). The printed banner
# additionally lets Part C assert on the rendered pane content.
cat > "$STUBDIR/claude" <<'EOF'
#!/bin/sh
: "${MARKER_DIR:?}"
marker="$MARKER_DIR/${OVERSEER_AGENT_ID:-unknown}"
echo $$ > "$marker"
printf 'STUB-ALIVE-%s\n' "${OVERSEER_AGENT_ID:-unknown}"
sleep 3600 &
wait $!
EOF
chmod +x "$STUBDIR/claude"
export PATH="$STUBDIR:$PATH"

git -C "$TEST_REPO" init -q
git -C "$TEST_REPO" commit -q --allow-empty -m init

PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); echo "  ok   - $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL - $1"; }

assert() {
    # assert <description> <actual> <expected>
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (expected '$3', got '$2')"; fi
}

assert_contains() {
    # assert_contains <description> <haystack> <needle>
    if printf '%s' "$2" | grep -qF -- "$3"; then pass "$1"; else fail "$1 (missing '$3')"; fi
}

assert_not_contains() {
    # assert_not_contains <description> <haystack> <needle>
    if printf '%s' "$2" | grep -qF -- "$3"; then fail "$1 (unexpectedly found '$3')"; else pass "$1"; fi
}

cleanup() {
    tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

start_harness() {
    # Explicit -x/-y: with no attached client, a detached new-session defaults to
    # a small fallback size — narrow enough that even a short wrapped status-bar
    # message (e.g. a drop confirm) can split across lines and defeat a
    # single-line `grep -F` assertion. 160x40 comfortably fits the tree pane
    # (~25% width) without wrapping normal-length messages, matching a
    # realistically-sized real terminal.
    tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
    rm -f "$SOCK"
    # tmux special-cases $SHELL on `new-session -e` (verified: it does not
    # apply there, unlike other vars) — set it via a wrapping shell instead so
    # the root's bare-shell PTY (`resolve_shell()`) actually resolves to our
    # stub, not the real system shell.
    tmux new-session -d -s "$HARNESS" -x 160 -y 40 -c "$TEST_REPO" \
        -e "PATH=$PATH" -e "MARKER_DIR=$MARKER_DIR" \
        -- sh -c "SHELL='$STUBDIR/claude' exec '$OVERSEER' --socket '$SOCK'"
    for _ in $(seq 1 50); do
        [ -S "$SOCK" ] && return 0
        sleep 0.1
    done
    echo "overseer never created its socket" >&2
    exit 1
}

# ── CLI helpers ───────────────────────────────────────────────────────────────

ov() { "$OVERSEER" --socket "$SOCK" "$@"; }
ov_as() { # ov_as <agent_id> <cwd> <args...>
    local id="$1" cwd="$2"; shift 2
    (cd "$cwd" && OVERSEER_AGENT_ID="$id" "$OVERSEER" --socket "$SOCK" "$@")
}

list_json() { ov list; }
agent_count() { list_json | jq '.data.agents | length'; }
agent_id_by_name() { list_json | jq -r --arg n "$1" '.data.agents[] | select(.name==$n) | .id'; }

# An agent's PTY is in-process (alacritty_terminal) — no external
# session to query. The stub's marker file is the liveness source of truth.
pty_alive() {
    local marker="$MARKER_DIR/$1" pid
    [ -e "$marker" ] || return 1
    pid="$(cat "$marker" 2>/dev/null)"
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}
wait_for_pty_alive() {
    for _ in $(seq 1 50); do pty_alive "$1" && return 0; sleep 0.1; done
    return 1
}
wait_for_pty_gone() {
    for _ in $(seq 1 50); do pty_alive "$1" || return 0; sleep 0.1; done
    return 1
}

echo
echo "== Part A: CLI-driven lifecycle (spawn/drop as an agent would call them) =="
start_harness

echo "-- start a root agent --"
RESP="$(ov start --cwd "$TEST_REPO")"
ROOT_ID="$(printf '%s' "$RESP" | jq -r '.data.agent_id')"
assert "start returns an agent id" "$([ -n "$ROOT_ID" ] && echo yes || echo no)" "yes"
wait_for_pty_alive "$ROOT_ID"
assert "root pty is alive" "$(pty_alive "$ROOT_ID" && echo yes || echo no)" "yes"
assert "registry shows 1 agent" "$(agent_count)" "1"
assert "root is named after the repo, not a typed task" \
    "$(list_json | jq -r --arg id "$ROOT_ID" '.data.agents[] | select(.id==$id) | .name')" "repo"
assert "root adapter is 'shell' — nothing auto-launched" \
    "$(list_json | jq -r --arg id "$ROOT_ID" '.data.agents[] | select(.id==$id) | .adapter')" "shell"
assert "root starts idle, not running" \
    "$(list_json | jq -r --arg id "$ROOT_ID" '.data.agents[] | select(.id==$id) | .status')" "idle"

echo "-- spawn a child under the root --"
RESP="$(ov_as "$ROOT_ID" "$TEST_REPO" spawn --task "child-cli")"
CHILD_ID="$(printf '%s' "$RESP" | jq -r '.data.agent_id')"
assert "spawn returns an agent id" "$([ -n "$CHILD_ID" ] && echo yes || echo no)" "yes"
assert "child branch is overseer/<id>" \
    "$(printf '%s' "$RESP" | jq -r '.data.branch' | cut -d/ -f1)" "overseer"
wait_for_pty_alive "$CHILD_ID"
assert "child pty is alive" "$(pty_alive "$CHILD_ID" && echo yes || echo no)" "yes"
assert "registry shows 2 agents" "$(agent_count)" "2"
assert "child's parent_id is the root" \
    "$(list_json | jq -r --arg id "$CHILD_ID" '.data.agents[] | select(.id==$id) | .parent_id')" "$ROOT_ID"

echo "-- reject a grandchild (child spawning its own child) --"
if ov_as "$CHILD_ID" "$TEST_REPO" spawn --task "grandchild-cli" >/tmp/spawn_gc.out 2>&1; then
    fail "grandchild spawn should have been rejected"
else
    assert_contains "grandchild spawn rejected with the right message" \
        "$(cat /tmp/spawn_gc.out)" "cannot spawn"
fi
assert "registry still shows 2 agents (grandchild not created)" "$(agent_count)" "2"

echo "-- drop the child (non-root) --"
ov drop "$CHILD_ID" >/dev/null
wait_for_pty_gone "$CHILD_ID"
assert "child pty is gone" "$(pty_alive "$CHILD_ID" && echo yes || echo no)" "no"
assert "registry shows 1 agent" "$(agent_count)" "1"

echo "-- root agents cannot be dropped via the command --"
if ov drop "$ROOT_ID" >/tmp/drop_root.out 2>&1; then
    fail "root drop via command should have been rejected"
else
    assert_contains "root drop rejected with the right message" "$(cat /tmp/drop_root.out)" "TUI"
fi
assert "root pty still alive" "$(pty_alive "$ROOT_ID" && echo yes || echo no)" "yes"

echo "-- dropping an unknown agent is an error --"
if ov drop "00000000-0000-0000-0000-000000000000" >/tmp/drop_unknown.out 2>&1; then
    fail "unknown agent drop should have been rejected"
else
    assert_contains "unknown agent drop rejected" "$(cat /tmp/drop_unknown.out)" "unknown agent"
fi

echo
echo "== Part B: TUI-driven lifecycle (n/s/d/D keybinds via tmux send-keys) =="
start_harness   # fresh, empty tree

tui_key() { tmux send-keys -t "$HARNESS" -- "$1"; sleep 0.3; }
tui_text() { tmux send-keys -t "$HARNESS" -l "$1"; sleep 0.2; }
tui_enter() { tmux send-keys -t "$HARNESS" Enter; sleep 0.3; }
pane() { tmux capture-pane -t "$HARNESS" -p; }

echo "-- 'n': spawn a root from the TUI --"
# 'n' now prompts for a repo path, prefilled with cwd (the harness's own cwd is
# $TEST_REPO) — no typed text needed, just Enter. The row is named after the repo.
tui_key n
tui_enter
assert_contains "tree shows the new root, named after the repo" "$(pane)" "repo"
ROOT_A="$(agent_id_by_name repo)"
wait_for_pty_alive "$ROOT_A"
assert "root pty is alive" "$(pty_alive "$ROOT_A" && echo yes || echo no)" "yes"

echo "-- 's' on a root: spawn a child from the TUI --"
tui_key s
tui_text "tui-child-a"
tui_enter
assert "registry shows root + child" "$(agent_count)" "2"
CHILD_A="$(agent_id_by_name tui-child-a)"
wait_for_pty_alive "$CHILD_A"
assert "child pty is alive" "$(pty_alive "$CHILD_A" && echo yes || echo no)" "yes"

echo "-- 'j' then 's' on a child: spawning under a child is refused server-side --"
# The TUI no longer pre-checks this client-side (AGENTS.md: the "no grandchildren"
# rule lives only in the server's Spawn handler) — it opens the input prompt like
# normal and the rejection comes back after Enter, same as any other spawn failure.
tui_key j
tui_key s
tui_text "would-be-grandchild"
tui_enter
assert_contains "status bar explains why" "$(pane)" "cannot spawn"
assert "no grandchild was created" "$(agent_count)" "2"

echo "-- 'd' + 'y' on the child: drop succeeds --"
tui_key d
assert_contains "confirm prompt shown" "$(pane)" "drop 'tui-child-a'"
tui_key y
wait_for_pty_gone "$CHILD_A"
assert "child removed from registry" "$(agent_count)" "1"
assert "child pty is gone" "$(pty_alive "$CHILD_A" && echo yes || echo no)" "no"

echo "-- 'd' + 'y' on the now-childless root: TUI *can* drop a root --"
tui_key d
tui_key y
wait_for_pty_gone "$ROOT_A"
assert "root removed from registry" "$(agent_count)" "0"
assert "root pty is gone" "$(pty_alive "$ROOT_A" && echo yes || echo no)" "no"

echo "-- non-recursive 'd' on a root WITH children is refused, 'D' works --"
# The previous "repo"-named root was fully dropped above, so re-spawning from the
# same cwd and reusing the name "repo" is unambiguous here.
tui_key n
tui_enter
tui_key s
tui_text "tui-child-b"
tui_enter
ROOT_B="$(agent_id_by_name repo)"
CHILD_B="$(agent_id_by_name tui-child-b)"
wait_for_pty_alive "$CHILD_B"

tui_key d
tui_key y
assert_contains "status bar reports the recursive-required error" "$(pane)" "--recursive"
assert "nothing was removed by the non-recursive attempt" "$(agent_count)" "2"

tui_key D
assert_contains "recursive confirm mentions the child" "$(pane)" "+ 1 children"
tui_key y
wait_for_pty_gone "$ROOT_B"
wait_for_pty_gone "$CHILD_B"
assert "recursive drop removed both agents" "$(agent_count)" "0"
assert "root-b pty is gone" "$(pty_alive "$ROOT_B" && echo yes || echo no)" "no"
assert "child-b pty is gone" "$(pty_alive "$CHILD_B" && echo yes || echo no)" "no"

echo
echo "== Part C: pane rendering, focus model, and the quit guard =="
start_harness   # fresh, empty tree

echo "-- fresh tree: pane shows the placeholder, no agent selected --"
assert_contains "pane shows 'no agent selected'" "$(pane)" "no agent selected"

echo "-- 'q' with nothing running: quits immediately, no confirm --"
tui_key q
sleep 0.3
assert "harness pane exited (no confirm needed)" \
    "$(tmux list-panes -t "$HARNESS" >/dev/null 2>&1 && echo alive || echo gone)" "gone"

start_harness   # fresh again for the rest of Part C

echo "-- 'n': spawn a root — pane renders its live PTY output once selected --"
tui_key n
tui_enter
ROOT_C="$(agent_id_by_name repo)"
wait_for_pty_alive "$ROOT_C"
sleep 0.3 # let the next redraw tick pick up the stub's banner
assert_contains "pane renders the root's own PTY output" "$(pane)" "STUB-ALIVE-$ROOT_C"

echo "-- Ctrl-l: jump in moves focus into the pane --"
tmux send-keys -t "$HARNESS" C-l
sleep 0.3
assert_contains "pane border shows it's focused" "$(pane)" "FOCUSED"

echo "-- Ctrl-h: jump out returns focus to the tree --"
tmux send-keys -t "$HARNESS" C-h
sleep 0.3
assert_not_contains "pane border no longer shows focused" "$(pane)" "FOCUSED"

echo "-- 'q' with a live agent: quit guard asks for confirmation first --"
tui_key q
assert_contains "quit confirm prompt shown" "$(pane)" "running and will be killed"
assert "root pty still alive while the prompt is up" "$(pty_alive "$ROOT_C" && echo yes || echo no)" "yes"

echo "-- 'n' cancels the quit --"
tui_key n
assert_contains "tree still shows the agent after cancelling quit" "$(pane)" "repo"
assert "root pty untouched by a cancelled quit" "$(pty_alive "$ROOT_C" && echo yes || echo no)" "yes"

echo "-- 'q' then 'y': confirmed quit kills the agent and exits --"
tui_key q
tui_key y
wait_for_pty_gone "$ROOT_C"
assert "root pty was killed by the confirmed quit" "$(pty_alive "$ROOT_C" && echo yes || echo no)" "no"

# ── summary ───────────────────────────────────────────────────────────────────

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
