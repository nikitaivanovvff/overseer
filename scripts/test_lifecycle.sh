#!/usr/bin/env bash
#
# End-to-end lifecycle test for `overseer start`/`spawn`/`drop`, driven against a
# real tmux server and a real overseer binary — no unit-test mocking involved.
#
# Runs in an isolated tmux server (own TMUX_TMPDIR) and a stub "claude" binary on
# PATH, so it never touches your real tmux sessions or launches a real agent.
#
# Part A drives the CLI (`overseer spawn`/`drop`) directly — this is what an agent
# calling `overseer spawn` from its own shell actually does.
# Part B drives the TUI itself via `tmux send-keys` + `capture-pane`, since some
# behavior (root agents can only be dropped via the TUI, not the CLI) only exists
# on that path.
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
export TMUX_TMPDIR="$WORKDIR/tmux"       # isolated tmux server — never touches your real sessions
mkdir -p "$TMUX_TMPDIR"
STUBDIR="$WORKDIR/bin"
mkdir -p "$STUBDIR"
TEST_REPO="$WORKDIR/repo"
mkdir -p "$TEST_REPO"
SOCK="$WORKDIR/overseer.sock"
HARNESS=overseer-harness-test

# Stub "claude" so tmux panes stay alive without launching a real agent.
cat > "$STUBDIR/claude" <<'EOF'
#!/bin/sh
exec sleep 3600
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

cleanup() {
    tmux kill-server >/dev/null 2>&1 || true
    tmux -L overseer kill-server >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

start_harness() {
    # The overseer binary (non-mock) now hosts its TUI in a private "-L overseer"
    # tmux session and reconnects to it if one is already running (PHASE5.md §3.2).
    # Kill any such leftover session first so each start_harness() call gets a
    # truly fresh registry, not whatever a previous Part left behind.
    tmux -L overseer kill-server >/dev/null 2>&1 || true
    # A force-killed server never runs the normal shutdown path that removes the
    # socket file, so a stale one could satisfy the readiness check below early.
    rm -f "$SOCK"
    # Explicit -x/-y: with no attached client, a detached new-session defaults to
    # a small fallback size — narrow enough that even a short wrapped status-bar
    # message (e.g. a drop confirm) can split across lines and defeat a
    # single-line `grep -F` assertion. 160x40 comfortably fits the tree pane
    # (~25% width) without wrapping normal-length messages, matching a
    # realistically-sized real terminal.
    tmux new-session -d -s "$HARNESS" -x 160 -y 40 -c "$TEST_REPO" \
        -e "PATH=$PATH" -e "TMUX_TMPDIR=$TMUX_TMPDIR" \
        -- "$OVERSEER" --socket "$SOCK"
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
# Agent sessions launched by TmuxClient live on the private "overseer" tmux server
# (PHASE5.md §2), not the default one — check there.
session_exists() { tmux -L overseer has-session -t "overseer-${1:0:8}" 2>/dev/null; }

echo
echo "== Part A: CLI-driven lifecycle (spawn/drop as an agent would call them) =="
start_harness

echo "-- start a root agent --"
RESP="$(ov start --cwd "$TEST_REPO")"
ROOT_ID="$(printf '%s' "$RESP" | jq -r '.data.agent_id')"
assert "start returns an agent id" "$([ -n "$ROOT_ID" ] && echo yes || echo no)" "yes"
assert "root tmux session exists" "$(session_exists "$ROOT_ID" && echo yes || echo no)" "yes"
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
assert "child tmux session exists" "$(session_exists "$CHILD_ID" && echo yes || echo no)" "yes"
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
assert "child tmux session is gone" "$(session_exists "$CHILD_ID" && echo yes || echo no)" "no"
assert "registry shows 1 agent" "$(agent_count)" "1"

echo "-- root agents cannot be dropped via the command --"
if ov drop "$ROOT_ID" >/tmp/drop_root.out 2>&1; then
    fail "root drop via command should have been rejected"
else
    assert_contains "root drop rejected with the right message" "$(cat /tmp/drop_root.out)" "TUI"
fi
assert "root tmux session still exists" "$(session_exists "$ROOT_ID" && echo yes || echo no)" "yes"

echo "-- dropping an unknown agent is an error --"
if ov drop "00000000-0000-0000-0000-000000000000" >/tmp/drop_unknown.out 2>&1; then
    fail "unknown agent drop should have been rejected"
else
    assert_contains "unknown agent drop rejected" "$(cat /tmp/drop_unknown.out)" "unknown agent"
fi

echo
echo "== Part B: TUI-driven lifecycle (n/s/d/D keybinds via tmux send-keys) =="
tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
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
assert "root tmux session exists" "$(session_exists "$ROOT_A" && echo yes || echo no)" "yes"

echo "-- 's' on a root: spawn a child from the TUI --"
tui_key s
tui_text "tui-child-a"
tui_enter
assert "registry shows root + child" "$(agent_count)" "2"
CHILD_A="$(agent_id_by_name tui-child-a)"
assert "child tmux session exists" "$(session_exists "$CHILD_A" && echo yes || echo no)" "yes"

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
assert "child removed from registry" "$(agent_count)" "1"
assert "child tmux session is gone" "$(session_exists "$CHILD_A" && echo yes || echo no)" "no"

echo "-- 'd' + 'y' on the now-childless root: TUI *can* drop a root --"
tui_key d
tui_key y
assert "root removed from registry" "$(agent_count)" "0"
assert "root tmux session is gone" "$(session_exists "$ROOT_A" && echo yes || echo no)" "no"

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

tui_key d
tui_key y
assert_contains "status bar reports the recursive-required error" "$(pane)" "--recursive"
assert "nothing was removed by the non-recursive attempt" "$(agent_count)" "2"

tui_key D
assert_contains "recursive confirm mentions the child" "$(pane)" "+ 1 children"
tui_key y
assert "recursive drop removed both agents" "$(agent_count)" "0"
assert "root-b tmux session is gone" "$(session_exists "$ROOT_B" && echo yes || echo no)" "no"
assert "child-b tmux session is gone" "$(session_exists "$CHILD_B" && echo yes || echo no)" "no"

echo
echo "== Part C: live split-pane view =="
tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
start_harness   # fresh, empty tree

# The overseer TUI's own window on the private server now has two real tmux
# panes: pane 0 is ratatui (tree), pane 1 is a nested client permanently
# attached to whichever agent session is selected (or the placeholder).
# Assert via that nested client's session, not the outer client's — jump-in
# is now a focus change (select-pane), not a client switch.
live_pane_tty() {
    tmux -L overseer list-panes -t overseer -F '#{pane_index} #{pane_tty}' 2>/dev/null | awk '$1==1{print $2}'
}
nested_client_session() {
    local tty
    tty="$(live_pane_tty)"
    [ -n "$tty" ] || return
    tmux -L overseer list-clients -F '#{client_tty} #{client_session}' 2>/dev/null | awk -v t="$tty" '$1==t{print $2}'
}
active_pane_index() {
    tmux -L overseer list-panes -t overseer -F '#{pane_index} #{pane_active}' 2>/dev/null | awk '$2==1{print $1}'
}

echo "-- fresh tree: live pane shows the placeholder --"
assert "nested client starts on the placeholder session" "$(nested_client_session)" "overseer-placeholder"

echo "-- 'n': spawn a root — live pane retargets automatically, no keypress needed --"
tui_key n
tui_enter
ROOT_C="$(agent_id_by_name repo)"
AGENT_SESSION="overseer-${ROOT_C:0:8}"
assert "live pane retargeted to the new root's session" "$(nested_client_session)" "$AGENT_SESSION"

echo "-- 'Enter': jump in moves tmux focus into the live pane --"
tui_enter
assert "active pane is the live pane (index 1)" "$(active_pane_index)" "1"

echo "-- <prefix> o: default pane-cycle binding returns focus to the tree pane --"
tmux send-keys -t "$HARNESS" C-b
sleep 0.2
tmux send-keys -t "$HARNESS" o
sleep 0.3
assert "active pane is back to the tree (index 0)" "$(active_pane_index)" "0"
assert_contains "TUI repainted and still shows the root" "$(pane)" "repo"

echo "-- 'd' + 'y': dropping the displayed agent retargets to the placeholder before the kill --"
tui_key d
tui_key y
assert "live pane retargeted back to the placeholder" "$(nested_client_session)" "overseer-placeholder"
assert "root tmux session is gone" "$(session_exists "$ROOT_C" && echo yes || echo no)" "no"

# ── summary ───────────────────────────────────────────────────────────────────

echo
echo "== $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
