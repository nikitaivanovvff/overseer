#!/usr/bin/env bash
#
# SCALE.md: stress-test the fleet story at N agents (default 30) against a
# real daemon + real spawn/status-push/render paths — not unit-test mocking.
# Same style as test_lifecycle.sh: isolated socket, self-cleaning trap.
#
# Spawns 1 root + N children, each running a stub agent script that emits
# output and pushes running/idle in a loop, approximating chatty hook
# traffic without burning any real tokens. One designated child ("child-1")
# also streams at a high, settable output rate (default ~400 lines/sec,
# matching a real observed agent pane) — PERFORMANCE.md step 1: the original
# version of this script never watched a pane or generated a single
# GridSnapshot during its load window, so it completely missed the code path
# that turned out to matter for a real user's reported typing lag (see
# AGENTS.md "Limits" / .specs/PERFORMANCE.md F5). This version keeps a
# tmux-driven TUI attached and watching child-1 *during* the load window, not
# after it, and records daemon CPU% alongside RSS. Measures:
#   - RSS + CPU% of the daemon process while the fleet is under load
#     and one pane is being watched (a real GridSnapshot stream, not idle)
#   - spawn-to-registered latency per child (the `overseer spawn` round trip)
#   - status-push round-trip latency under load (timed inside each stub)
#   - a best-effort automated tmux proxy for keypress-to-echo latency in a
#     focused, actively-streaming pane, measured *during* the load window
#     (the spec calls the true version of this "manually observed" — this is
#     an automated approximation, not a replacement for actually looking at
#     it, and it is known to under-report/time out at high output rates: real
#     kernel tty echo of injected keystrokes races with the chatty child's own
#     concurrent writes to the *same* pty, which is a property of typing into
#     a firehose pane through a nested terminal, not of the daemon being slow
#     — see the raw write->Output round trip below for the number that
#     actually isolates daemon-side latency from that harness artifact)
#   - a precise write->Output round trip measured over a second, independent
#     raw attach connection (python3, if available): sends a `Write` directly
#     and times how long until an `Output` grid containing the marker text
#     comes back on its *own* `Watch` of the same chatty child the tmux TUI is
#     also watching — deliberately two simultaneous watchers of one agent, to
#     exercise the F3 fix (generation counter, not a consumed dirty flag) live
#
# Usage: scripts/stress.sh [N] [lines_per_sec]
#   N            number of children to spawn (default 30)
#   lines_per_sec  output rate for the one watched/chatty child (default 400)
#
# Requires: cargo, tmux, jq, git. Optional: python3 (for the precise raw
# write->Output round-trip measurement; skipped with a note if absent).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

N="${1:-30}"
STUB_RATE="${2:-400}"

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
#
# The one child spawned with task text "stress-child-1" (always child #1,
# the harness's own naming convention below) additionally runs a background
# high-rate printer at $STUB_LINES_PER_SEC lines/sec — this is the pane the
# harness attaches to and watches for the whole load window, so daemon CPU
# and keypress-echo are measured against real, continuous GridSnapshot
# traffic instead of the ~5 lines/sec baseline chatter every other child
# (and the old version of this script) produced.
cat > "$STUBDIR/claude" <<'EOF'
#!/bin/sh
: "${MARKER_DIR:?}" "${TIMING_DIR:?}"
marker="$MARKER_DIR/${OVERSEER_AGENT_ID:-unknown}"
timing="$TIMING_DIR/${OVERSEER_AGENT_ID:-unknown}"
echo $$ > "$marker"
printf 'STUB-ALIVE-%s\n' "${OVERSEER_AGENT_ID:-unknown}"

if [ "${OVERSEER_TASK:-}" = "stress-child-1" ]; then
    rate="${STUB_LINES_PER_SEC:-400}"
    (
        interval=$(awk -v r="$rate" 'BEGIN { if (r <= 0) r = 400; printf "%.6f", 1.0 / r }')
        n=0
        while true; do
            n=$((n + 1))
            printf 'chatty-line-%s-%s\n' "${OVERSEER_AGENT_ID:-unknown}" "$n"
            sleep "$interval"
        done
    ) &
fi

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
# The stub's env (OVERSEER_BIN/MARKER_DIR/TIMING_DIR/STUB_LINES_PER_SEC) *and*
# SHELL (which `overseer start`'s bare-shell root reads server-side) have to
# be present on *this* invocation -- both the shell-root launch and every
# child's PTY env are built in the daemon process from whatever it itself
# inherited, not from whatever env a later CLI call happens to have when it
# requests a spawn over the socket.
SHELL="$STUBDIR/claude" OVERSEER_BIN="$OVERSEER" MARKER_DIR="$MARKER_DIR" TIMING_DIR="$TIMING_DIR" \
    STUB_LINES_PER_SEC="$STUB_RATE" \
    "$OVERSEER" --socket "$SOCK" daemon >"$WORKDIR/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "daemon never created its socket" >&2; exit 1; }
echo "daemon pid: $DAEMON_PID"

echo
echo "== registering 1 root + $N children (SHELL=claude, child-1 chatty at ${STUB_RATE} lines/sec) =="
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
CHILD1_ID="$(head -n 1 "$CHILD_IDS")" # task "stress-child-1" -- the designated chatty pane

echo "waiting for all children's stub PTYs to come alive..."
ALIVE=0
while read -r cid; do
    wait_for_pty_alive "$cid" && ALIVE=$((ALIVE + 1))
done < "$CHILD_IDS"
echo "$ALIVE / $REGISTERED child PTYs alive"

# A second, independent attach connection: opens its own `Watch` on the same
# chatty child the tmux TUI below is also watching (F3's fix means both
# connections must see every update, not race for one shared dirty flag),
# sends one `Write` with a marker, and times how long until an `Output` grid
# containing that marker comes back. This is the number that actually
# isolates the daemon's own write->render->stream latency from the tmux
# proxy's real but harness-specific limitation (kernel tty echo of an
# injected keystroke racing the chatty child's own concurrent pty writes,
# which can legitimately make a typed marker never become a stable,
# contiguous, on-screen substring at high output rates even when the daemon
# itself responded in single-digit milliseconds).
RAW_PROBE="$WORKDIR/raw_probe.py"
cat > "$RAW_PROBE" <<'PYEOF'
import json, socket, sys, threading, time, uuid

sock_path, agent_id = sys.argv[1], sys.argv[2]

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
s.connect(sock_path)
s.sendall((json.dumps({"cmd": "attach"}) + "\n").encode())

buf = b""
def read_line():
    global buf
    while b"\n" not in buf:
        chunk = s.recv(1 << 20)
        if not chunk:
            return None
        buf += chunk
    line, buf = buf.split(b"\n", 1)
    return line

read_line()  # initial Snapshot
s.sendall((json.dumps({"cmd": "watch", "agent_id": agent_id}) + "\n").encode())
read_line()  # immediate on-Watch grid

marker = "RAWPROBE" + uuid.uuid4().hex[:8]
sent_at = [None]

def sender():
    time.sleep(0.5)
    sent_at[0] = time.time()
    s.sendall((json.dumps({"cmd": "write", "agent_id": agent_id, "data": f"echo {marker}\n"}) + "\n").encode())

threading.Thread(target=sender, daemon=True).start()

deadline = time.time() + 8
found_at = None
while time.time() < deadline:
    line = read_line()
    if line is None:
        break
    try:
        ev = json.loads(line)
    except ValueError:
        continue
    if ev.get("event") == "output":
        cells = ev["grid"]["cells"]
        text = "".join(c["ch"] if c else " " for c in cells)
        if marker in text:
            found_at = time.time()
            break

if sent_at[0] and found_at:
    print(f"RAW_ECHO_MS={(found_at - sent_at[0]) * 1000:.1f}")
else:
    print("RAW_ECHO_MS=timeout")
PYEOF

echo
echo "== attaching a TUI and watching child-1 (the chatty pane) for the whole load window =="
tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true
tmux new-session -d -s "$HARNESS" -x 160 -y 40 -c "$TEST_REPO" \
    -e "PATH=$PATH" -e "MARKER_DIR=$MARKER_DIR" -e "TIMING_DIR=$TIMING_DIR" \
    -- sh -c "exec '$OVERSEER' --socket '$SOCK'"
for _ in $(seq 1 50); do
    tmux capture-pane -t "$HARNESS" -p 2>/dev/null | grep -q "WORKSPACES" && break
    sleep 0.1
done
# Tree order is root first, then children in spawn order — one `j` from the
# initial (root-selected) cursor lands on child-1, the designated chatty
# pane. Avoids fuzzy-search ambiguity between "child-1" and "child-10"+.
tmux send-keys -t "$HARNESS" j
sleep 0.1
tmux send-keys -t "$HARNESS" C-l
sleep 0.3

echo "== sampling daemon CPU%% (~8s) while all $REGISTERED children stream and child-1 is watched =="
CPU_SAMPLES="$WORKDIR/cpu_samples.txt"
: > "$CPU_SAMPLES"
(
    for _ in $(seq 1 20); do
        ps -o %cpu= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ' >> "$CPU_SAMPLES"
        sleep 0.4
    done
) &
CPU_SAMPLER_PID=$!

RAW_PROBE_OUT="$WORKDIR/raw_probe_out.txt"
if command -v python3 >/dev/null; then
    python3 "$RAW_PROBE" "$SOCK" "$CHILD1_ID" > "$RAW_PROBE_OUT" 2>&1 &
    RAW_PROBE_PID=$!
else
    echo "python3 not found -- skipping the precise raw write->Output round-trip measurement"
    RAW_PROBE_PID=""
fi

sleep 1 # let the watch/stream settle before timing a keystroke against it

echo "== keypress-to-echo, measured mid-load with child-1's chatty pane focused =="
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
echo "keypress-to-echo (tmux proxy, includes tmux's own overhead + real kernel-echo/chatty-write racing at high output rates): ${FOUND_MS}ms (threshold: subjectively instant, <50ms is the target but tmux overhead alone often exceeds that -- a 'timeout' here is a known harness limitation at high lines_per_sec, not necessarily a daemon slowdown -- see the raw round trip below)"

if [ -n "$RAW_PROBE_PID" ]; then
    wait "$RAW_PROBE_PID" 2>/dev/null
    RAW_ECHO="$(grep -o 'RAW_ECHO_MS=.*' "$RAW_PROBE_OUT" 2>/dev/null | tail -1)"
    if [ -n "$RAW_ECHO" ]; then
        echo "write->Output round trip (raw second attach connection, precise): ${RAW_ECHO#RAW_ECHO_MS=}ms (threshold: informational -- isolates daemon-side latency from the tmux proxy's own artifacts; two connections watched child-1 simultaneously here, exercising the F3 generation-counter fix)"
    else
        echo "write->Output round trip (raw probe): no result -- see $RAW_PROBE_OUT"
        cat "$RAW_PROBE_OUT" 2>/dev/null
    fi
fi

wait "$CPU_SAMPLER_PID" 2>/dev/null
tmux kill-session -t "$HARNESS" >/dev/null 2>&1 || true

echo
echo "== measurements =="

RSS_KB=$(ps -o rss= -p "$DAEMON_PID" 2>/dev/null | tr -d ' ')
RSS_MB=$((RSS_KB / 1024))
echo "daemon RSS: ${RSS_MB}MB (threshold: <500MB)"

if [ -s "$CPU_SAMPLES" ]; then
    CPU_MEAN=$(awk '{sum+=$1; n++} END {if (n>0) printf "%.1f", sum/n; else print "n/a"}' "$CPU_SAMPLES")
    CPU_MAX=$(sort -n "$CPU_SAMPLES" | tail -1)
    echo "daemon CPU: mean ${CPU_MEAN}%, max ${CPU_MAX}% (sampled every 0.4s during the load+watch window, single core = 100%)"
else
    echo "daemon CPU: no samples recorded"
fi

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
