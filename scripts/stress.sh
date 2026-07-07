#!/usr/bin/env bash
#
# SCALE.md: stress-test the fleet story at N agents (default 30) against a
# real daemon + real spawn/status-push/render paths — not unit-test mocking.
# Same style as test_lifecycle.sh: isolated socket, self-cleaning trap.
#
# Spawns 1 root + N children, each running a stub agent script that emits
# output and pushes running/idle in a loop, approximating chatty hook
# traffic without burning any real tokens. Measures:
#   - RSS of the daemon process
#   - spawn-to-registered latency per child (the `overseer spawn` round trip)
#   - status-push round-trip latency under load (timed inside each stub)
#   - a best-effort automated proxy for keypress-to-echo latency in a
#     focused pane (the spec calls this "manually observed" — this is an
#     automated approximation, not a replacement for actually looking at it)
#
# Usage: scripts/stress.sh [N]
#
# Requires: cargo, tmux, jq, git.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

N="${1:-30}"

for tool in tmux jq git cargo; do
    command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

echo "building overseer (release, for a realistic RSS/latency measurement)..."
cargo build --release --quiet || { echo "build failed" >&2; exit 1; }
OVERSEER="$REPO_ROOT/target/release/overseer"

# A dedicated, owned subdirectory — the daemon chmods its socket's parent dir
# to 0o700 on startup, which fails outright against a shared dir like /tmp
# itself (discovered running this very script).
WORKDIR="$(mktemp -d)"
STUBDIR="$WORKDIR/bin"
mkdir -p "$STUBDIR"
TEST_REPO="$WORKDIR/repo"
mkdir -p "$TEST_REPO"
MARKER_DIR="$WORKDIR/markers"
mkdir -p "$MARKER_DIR"
TIMING_DIR="$WORKDIR/timing"
mkdir -p "$TIMING_DIR"
SOCK="$WORKDIR/overseer.sock"
HARNESS=overseer-stress-test
DAEMON_PID=""

# Stub agent: prints an identity banner (so a focused pane has something to
# echo against), then loops emitting output and pushing running/idle,
# timing each `overseer status` call to its own per-agent timing file. Rate
# is deliberately fast (chattier than any real hook traffic) to load the
# registry mutex and the socket accept path harder than a real fleet would.
cat > "$STUBDIR/claude" <<'EOF'
#!/bin/sh
: "${MARKER_DIR:?}" "${TIMING_DIR:?}"
marker="$MARKER_DIR/${OVERSEER_AGENT_ID:-unknown}"
timing="$TIMING_DIR/${OVERSEER_AGENT_ID:-unknown}"
echo $$ > "$marker"
printf 'STUB-ALIVE-%s\n' "${OVERSEER_AGENT_ID:-unknown}"
i=0
while true; do
    for status in running idle; do
        start=$(date +%s%N)
        if "$OVERSEER_BIN" status "$status" >/dev/null 2>&1; then
            end=$(date +%s%N)
            echo "$((end - start))" >> "$timing"
        else
            echo "FAILED" >> "$timing"
        fi
        i=$((i + 1))
        printf 'tick-%s-%s\n' "$i" "$status"
        sleep 0.2
    done
done
EOF
chmod +x "$STUBDIR/claude"
export PATH="$STUBDIR:$PATH"

git -C "$TEST_REPO" init -q
git -C "$TEST_REPO" commit -q --allow-empty -m init

cleanup() {
    tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
    [ -n "$DAEMON_PID" ] && kill -9 "$DAEMON_PID" >/dev/null 2>&1 || true
    pkill -f "overseer --socket $SOCK daemon" >/dev/null 2>&1 || true
    pkill -f "$STUBDIR/claude" >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

ov() { "$OVERSEER" --socket "$SOCK" "$@"; }
ov_as() { # ov_as <agent_id> <cwd> <args...> -- only affects the *requesting*
    # CLI call's own env, not the spawned child's PTY env (that's built
    # server-side, in the daemon process, from the daemon's own environment
    # plus injected identity vars -- see the daemon startup line below for
    # where OVERSEER_BIN/MARKER_DIR/TIMING_DIR actually need to live).
    local id="$1" cwd="$2"; shift 2
    (cd "$cwd" && OVERSEER_AGENT_ID="$id" OVERSEER_SOCKET="$SOCK" "$OVERSEER" --socket "$SOCK" "$@")
}
list_json() { ov list; }
agent_count() { list_json | jq '.data.agents | length'; }

pty_alive() {
    local marker="$MARKER_DIR/$1" pid
    [ -e "$marker" ] || return 1
    pid="$(cat "$marker" 2>/dev/null)"
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}
wait_for_pty_alive() {
    for _ in $(seq 1 100); do pty_alive "$1" && return 0; sleep 0.1; done
    return 1
}

echo
echo "== starting a headless daemon on an isolated socket =="
rm -f "$SOCK"
# The stub's env (OVERSEER_BIN/MARKER_DIR/TIMING_DIR) *and* SHELL (which
# `overseer start`'s bare-shell root reads server-side) have to be present on
# *this* invocation -- both the shell-root launch and every child's PTY env
# are built in the daemon process from whatever it itself inherited, not
# from whatever env a later CLI call happens to have when it requests a
# spawn over the socket.
SHELL="$STUBDIR/claude" OVERSEER_BIN="$OVERSEER" MARKER_DIR="$MARKER_DIR" TIMING_DIR="$TIMING_DIR" \
    "$OVERSEER" --socket "$SOCK" daemon >"$WORKDIR/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "daemon never created its socket" >&2; exit 1; }
echo "daemon pid: $DAEMON_PID"

echo
echo "== registering 1 root + $N children (SHELL=claude) =="
RESP="$(ov start --cwd "$TEST_REPO")"
ROOT_ID="$(printf '%s' "$RESP" | jq -r '.data.agent_id')"
[ -n "$ROOT_ID" ] && [ "$ROOT_ID" != "null" ] || { echo "root registration failed: $RESP" >&2; exit 1; }
wait_for_pty_alive "$ROOT_ID"

SPAWN_LATENCIES="$WORKDIR/spawn_latencies.txt"
: > "$SPAWN_LATENCIES"
CHILD_IDS="$WORKDIR/child_ids.txt"
: > "$CHILD_IDS"

for i in $(seq 1 "$N"); do
    start=$(date +%s%N)
    RESP="$(ov_as "$ROOT_ID" "$TEST_REPO" spawn --name "child-$i" --task "stress-child-$i" --adapter claude 2>&1)"
    end=$(date +%s%N)
    CHILD_ID="$(printf '%s' "$RESP" | jq -r '.data.agent_id' 2>/dev/null)"
    if [ -z "$CHILD_ID" ] || [ "$CHILD_ID" = "null" ]; then
        echo "spawn $i failed: $RESP" >&2
        continue
    fi
    echo "$CHILD_ID" >> "$CHILD_IDS"
    echo "$(( (end - start) / 1000000 ))" >> "$SPAWN_LATENCIES" # ms
done

REGISTERED=$(wc -l < "$CHILD_IDS" | tr -d ' ')
echo "spawned $REGISTERED / $N children"

echo "waiting for all children's stub PTYs to come alive..."
ALIVE=0
while read -r cid; do
    wait_for_pty_alive "$cid" && ALIVE=$((ALIVE + 1))
done < "$CHILD_IDS"
echo "$ALIVE / $REGISTERED child PTYs alive"

echo
echo "== letting the fleet chatter for 8s (status pushes + output) =="
sleep 8

echo
echo "== measurements =="

RSS_KB=$(ps -o rss= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ')
RSS_MB=$((RSS_KB / 1024))
echo "daemon RSS: ${RSS_MB}MB (threshold: <500MB)"

if [ -s "$SPAWN_LATENCIES" ]; then
    SPAWN_MAX=$(sort -n "$SPAWN_LATENCIES" | tail -1)
    SPAWN_MEAN=$(awk '{sum+=$1; n++} END {if (n>0) printf "%.1f", sum/n; else print "n/a"}' "$SPAWN_LATENCIES")
    echo "spawn latency: mean ${SPAWN_MEAN}ms, max ${SPAWN_MAX}ms (over $REGISTERED spawns)"
else
    echo "spawn latency: no successful spawns recorded"
fi

TOTAL_PUSHES=0
FAILED_PUSHES=0
PUSH_NS_ALL="$WORKDIR/all_push_ns.txt"
: > "$PUSH_NS_ALL"
for f in "$TIMING_DIR"/*; do
    [ -f "$f" ] || continue
    TOTAL_PUSHES=$((TOTAL_PUSHES + $(wc -l < "$f")))
    FAILED_PUSHES=$((FAILED_PUSHES + $(grep -c FAILED "$f" || true)))
    grep -v FAILED "$f" >> "$PUSH_NS_ALL" || true
done
if [ -s "$PUSH_NS_ALL" ]; then
    PUSH_MEAN_MS=$(awk '{sum+=$1; n++} END {if (n>0) printf "%.2f", sum/n/1000000; else print "n/a"}' "$PUSH_NS_ALL")
    PUSH_MAX_MS=$(sort -n "$PUSH_NS_ALL" | tail -1 | awk '{printf "%.2f", $1/1000000}')
    echo "status-push latency: mean ${PUSH_MEAN_MS}ms, max ${PUSH_MAX_MS}ms (over $TOTAL_PUSHES pushes)"
fi
echo "status pushes: $TOTAL_PUSHES sent, $FAILED_PUSHES failed (threshold: 0 failed)"

echo
echo "== best-effort input-latency proxy (attach a TUI, jump into a chatty child, time an echoed keystroke) =="
tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
tmux new-session -d -s "$HARNESS" -x 160 -y 40 -c "$TEST_REPO" \
    -e "PATH=$PATH" -e "MARKER_DIR=$MARKER_DIR" -e "TIMING_DIR=$TIMING_DIR" \
    -- sh -c "exec '$OVERSEER' --socket '$SOCK'"
for _ in $(seq 1 50); do
    tmux capture-pane -t "$HARNESS" -p 2>/dev/null | grep -q "AGENTS" && break
    sleep 0.1
done
tmux send-keys -t "$HARNESS" C-l
sleep 0.3
MARKER_TOKEN="latency-probe-$$"
SEND_NS=$(date +%s%N)
tmux send-keys -t "$HARNESS" -l "echo $MARKER_TOKEN"
tmux send-keys -t "$HARNESS" Enter
FOUND_MS="timeout"
for _ in $(seq 1 50); do
    if tmux capture-pane -t "$HARNESS" -p 2>/dev/null | grep -qF "$MARKER_TOKEN"; then
        END_NS=$(date +%s%N)
        FOUND_MS=$(( (END_NS - SEND_NS) / 1000000 ))
        break
    fi
    sleep 0.02
done
echo "keypress-to-echo (automated proxy, includes tmux's own overhead): ${FOUND_MS}ms (threshold: subjectively instant, <50ms is the target but tmux overhead alone often exceeds that -- treat this as informational, confirm by eye if borderline)"
tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true

echo
echo "== verdict =="
PASS=1
[ "$RSS_MB" -lt 500 ] || { echo "FAIL: RSS ${RSS_MB}MB >= 500MB"; PASS=0; }
[ "$FAILED_PUSHES" -eq 0 ] || { echo "FAIL: $FAILED_PUSHES status pushes failed"; PASS=0; }
[ "$ALIVE" -eq "$REGISTERED" ] || { echo "FAIL: only $ALIVE/$REGISTERED child PTYs came alive"; PASS=0; }
[ "$REGISTERED" -eq "$N" ] || { echo "FAIL: only $REGISTERED/$N children registered"; PASS=0; }

if [ "$PASS" -eq 1 ]; then
    echo "PASS: N=$N fleet within all measured thresholds"
else
    echo "one or more thresholds failed -- see AGENTS.md Task 3 / SCALE.md for what to fix"
fi
[ "$PASS" -eq 1 ]
