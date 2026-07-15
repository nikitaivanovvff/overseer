use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::{
        unix::{OwnedReadHalf, OwnedWriteHalf},
        UnixListener,
    },
    sync::Mutex as AsyncMutex,
};

use crate::agent::{AgentRegistry, AgentStatus};
use crate::ipc::{
    handlers::{dispatch, AppCtx},
    protocol::{AttachEvent, Request, Response, MAX_WRITE_DATA_BYTES},
};
use crate::session::SessionManager;

/// Max size of one newline-delimited protocol line (SECURITY-AUDIT.md F1).
/// `AsyncBufReadExt::read_line` grows its buffer without bound until a
/// newline arrives, so any client holding `OVERSEER_SOCKET` could otherwise
/// stream gigabytes with no newline and OOM the single daemon process that
/// backs every agent. Sized comfortably above the largest legitimate line —
/// a `Spawn.task` near `MAX_SPAWN_TASK_BYTES` (128 KiB) plus its JSON
/// envelope — while still capping the worst case tightly.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Max concurrent connections the daemon will service at once
/// (SECURITY-AUDIT.md F9). `pub(super)` so `ipc::mod`'s integration tests can
/// reference the real value instead of duplicating the number.
///
/// Shrunk under `cfg(test)`: exercising the real ceiling means opening more
/// simultaneous *unaccepted* connections than macOS's default `somaxconn`
/// (128) comfortably allows, which makes a 256-connection version of the
/// test flaky under parallel `cargo test` load (`ECONNREFUSED` from the
/// kernel's listen backlog, not a bug in the daemon) — the gating logic
/// itself doesn't care what the number is.
#[cfg(not(test))]
pub(super) const MAX_CONCURRENT_CONNECTIONS: usize = 256;
#[cfg(test)]
pub(super) const MAX_CONCURRENT_CONNECTIONS: usize = 8;

/// A bound Unix listener remains usable by already-connected clients after its
/// filesystem node is unlinked, but no new hook or CLI process can connect.
/// Check often enough that lifecycle pushes recover promptly without adding a
/// filesystem stat to the hot accept path.
const SOCKET_REBIND_INTERVAL: Duration = Duration::from_millis(250);

/// How long `handle_conn` will wait for a line before giving up on a
/// connection that opened but never sent anything (SECURITY-AUDIT.md F9) —
/// a slow-loris-style hold that ties up a connection slot (and, before F9,
/// had no ceiling on how many could pile up) for no purpose. Only guards the
/// one-shot request loop: once a connection upgrades via `Request::Attach`,
/// sitting idle between `Watch`/`Scroll` requests while just receiving
/// streamed output is the normal, expected steady state, so `handle_attach`
/// deliberately does not reuse this timeout.
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Reads one newline-terminated line into `buf` (replacing its contents),
/// capped at `MAX_LINE_BYTES` total bytes read (F1). Mirrors
/// `AsyncBufReadExt::read_line`'s `Ok(0)` == clean EOF convention. Returns an
/// error (without waiting for a newline that may never come) once the cap is
/// exceeded — callers treat that the same as any other I/O error: drop the
/// connection.
///
/// Operates on raw bytes rather than `String`/`read_line` so a malicious
/// client can't force UTF-8 (re-)validation over an unbounded, still-growing
/// buffer either; the one UTF-8 check happens once, in `serde_json`, over an
/// already-capped slice.
async fn read_line_capped<R>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(total);
        }
        let newline_at = available.iter().position(|&b| b == b'\n');
        let chunk_len = newline_at.map(|i| i + 1).unwrap_or(available.len());
        if total + chunk_len > MAX_LINE_BYTES {
            reader.consume(chunk_len);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds max size",
            ));
        }
        buf.extend_from_slice(&available[..chunk_len]);
        reader.consume(chunk_len);
        total += chunk_len;
        if newline_at.is_some() {
            return Ok(total);
        }
    }
}

/// Trims ASCII whitespace (matches the `\n`/`\r\n` line endings this protocol
/// actually produces) from both ends of a byte slice — the byte-oriented
/// counterpart of `str::trim` now that lines are read as `Vec<u8>` (F1).
fn trim_ascii_whitespace(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(start);
    &b[start..end]
}

/// How often the attach connection's output task checks the watched agent's
/// content generation — matches the TUI's own render tick (`tui.rs`'s 16ms
/// poll), so streamed output feels as responsive as local rendering used to.
/// A real user's reported typing lag traced back to this stacking additively
/// with the TUI's own poll: both were 80-100ms, so a keystroke's round trip
/// (input → PTY write → agent echo → dirty → next redraw) could compound to
/// 200-300ms worst case even though neither interval looked large on its own.
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// One attach connection's watch state: which agent (if any) it's currently
/// streaming, and the last content generation (`SessionManager::generation`)
/// it sent for that agent. Shared between the request-read loop (which
/// updates it on `Watch`/`Unwatch`) and the output poller (which reads it
/// each tick and updates `last_sent_gen` after a successful send) — moving
/// this state per-connection, instead of a single flag consumed off the
/// session itself, is what fixes F3: two connections watching the same agent
/// each track their own progress instead of racing to steal one shared flag.
///
/// `scroll_dirty` marks that a `Scroll`/`ScrollToBottom` moved the watched
/// agent's display offset since the last send — scrolling never touches the
/// PTY, so it never bumps the generation counter; this flag is how the
/// output poller learns a resend is needed. It's set by the request loop and
/// *consumed* by the poller, which is safe (unlike the pre-F3 shared dirty
/// bool) because both live on the same connection: scrolls only ever arrive
/// from this connection's own client, so there's no second consumer to
/// starve. Deferring the resend to the poll tick (≤16ms) instead of replying
/// inline is what coalesces a trackpad flick's dozens of notches into at
/// most one full grid per tick — each inline reply used to cost a ~1MB
/// serialize+write *per notch*, head-of-line blocking every keystroke
/// (`Write`) queued behind it on this same request loop, and a fast flick
/// could keep the connection saturated for seconds after the gesture ended
/// (a real, reported freeze).
#[derive(Clone)]
struct WatchState {
    agent_id: Option<crate::agent::AgentId>,
    last_sent_gen: u64,
    scroll_dirty: bool,
}

pub async fn run(
    ctx: Arc<AppCtx>,
    socket: PathBuf,
    ready: Option<std::sync::mpsc::SyncSender<()>>,
) -> std::io::Result<()> {
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }

    let mut listener = bind_listener(&socket)?;
    if let Some(tx) = ready {
        let _ = tx.send(());
    }

    if ctx.watch_sessions {
        tokio::spawn(session_watcher(ctx.clone()));
    }

    // Bounds in-flight connections (SECURITY-AUDIT.md F9): without this, any
    // client can open unlimited concurrent connections, each spawning tasks
    // (attach connections spawn two long-lived forwarders plus a 16ms
    // poller) with nothing to stop the count growing until the daemon
    // degrades. 256 is far above any real usage (one TUI attach + a handful
    // of one-shot hook calls at a time) but still a hard ceiling.
    let conn_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    let mut socket_guard = tokio::time::interval(SOCKET_REBIND_INTERVAL);
    socket_guard.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            conn = listener.accept() => {
                let (stream, _) = match conn {
                    Ok(conn) => conn,
                    Err(e) => {
                        use std::io::ErrorKind::*;
                        match e.kind() {
                            ConnectionAborted | ConnectionReset | Interrupted => continue,
                            _ => return Err(e),
                        }
                    }
                };
                // At the ceiling: drop the connection immediately rather than
                // spawn a task that would just queue up behind the others.
                let Ok(permit) = conn_semaphore.clone().try_acquire_owned() else { continue };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    handle_conn(stream, ctx).await;
                    drop(permit);
                });
            }
            _ = socket_guard.tick() => {
                if !socket.exists() {
                    match bind_listener(&socket) {
                        Ok(new_listener) => {
                            listener = new_listener;
                            eprintln!("overseer: restored unlinked daemon socket at {}", socket.display());
                        }
                        Err(e) => {
                            // Keep the old listener and retry. Existing attach
                            // clients are still healthy even while the path is
                            // absent, so a transient bind failure must not tear
                            // down the daemon and every PTY it owns.
                            eprintln!("overseer: failed to restore daemon socket at {}: {e}", socket.display());
                        }
                    }
                }
            }
            // `overseer shutdown`'s handler notifies this only after its own
            // response has already been written back to its caller — see
            // `handle_conn`. Stopping the accept loop and letting `run`
            // return is the entire "exit the daemon": `main` returning ends
            // the process with no `std::process::exit` needed.
            _ = ctx.shutdown_notify.notified() => {
                return Ok(());
            }
        }
    }
}

fn bind_listener(socket: &std::path::Path) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(socket)?;
    // The bound socket node otherwise inherits whatever the process umask
    // allows. Keep it owner-only even when this is a recovery rebind.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn handle_conn(stream: tokio::net::UnixStream, ctx: Arc<AppCtx>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();

    loop {
        line.clear();
        let read_result = match tokio::time::timeout(IDLE_READ_TIMEOUT, read_line_capped(&mut reader, &mut line)).await
        {
            Ok(result) => result,
            // Idle too long with no complete line -- same disposition as any
            // other read failure below: drop the connection.
            Err(_) => break,
        };
        match read_result {
            Ok(0) => break,
            Ok(_) => {
                match serde_json::from_slice::<Request>(trim_ascii_whitespace(&line)) {
                    // `Attach` upgrades this connection for the rest of its
                    // life — hand off to the dedicated event-stream loop and
                    // never return to one-shot request/response handling.
                    Ok(Request::Attach) => {
                        handle_attach(reader, write_half, ctx).await;
                        return;
                    }
                    Ok(req) => {
                        let is_shutdown = matches!(req, Request::Shutdown);
                        // Blocking I/O (git, PTY launch) must not block the tokio thread.
                        let ctx_clone = ctx.clone();
                        let resp = tokio::task::spawn_blocking(move || dispatch(&ctx_clone, req))
                            .await
                            .unwrap_or_else(|_| Response::err("handler panicked"));
                        let ok = resp.ok;
                        if !write_response(&mut write_half, &resp).await {
                            break;
                        }
                        if is_shutdown && ok {
                            // The response bytes are already handed to the
                            // kernel's socket buffer at this point — safe to
                            // ask the accept loop to stop even though this
                            // task (and the runtime under it) may tear down
                            // before the caller has actually read them.
                            //
                            // `notify_one`, not `notify_waiters`: the accept
                            // loop's `select!` re-creates its `.notified()`
                            // future every iteration, so there's a real
                            // window where nothing is polling it yet when
                            // this fires. `notify_waiters` only wakes
                            // *currently* registered waiters and drops the
                            // notification otherwise; `notify_one` stores a
                            // permit so the next `.notified()` call (even one
                            // created after this line runs) completes
                            // immediately. Confirmed by reproducing the lost
                            // wake with `notify_waiters` under test.
                            ctx.shutdown_notify.notify_one();
                            return;
                        }
                    }
                    Err(e) => {
                        let resp = Response::err(format!("parse error: {e}"));
                        if !write_response(&mut write_half, &resp).await {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
}

async fn write_response(write_half: &mut OwnedWriteHalf, resp: &Response) -> bool {
    let mut bytes = serde_json::to_vec(resp)
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"internal serialization error\"}".to_vec());
    bytes.push(b'\n');
    write_half.write_all(&bytes).await.is_ok()
}

/// Writes one `AttachEvent` as a newline-delimited JSON line, serializing
/// concurrent writers (registry-event forwarder, output poller, and the
/// request loop's immediate on-`Watch` snapshot all share one connection).
/// Returns `false` on any I/O or serialization failure — callers treat that
/// as "the client is gone".
///
/// Only for small events (`Snapshot`/registry events) — an `Output` event
/// carrying a full `GridSnapshot` must go through `build_output_event_bytes`
/// instead, which does its own (expensive) serialization off this single-
/// threaded runtime's one thread. Serializing a large `Output` inline here
/// would reintroduce exactly the stall this split exists to avoid.
async fn send_event(write_half: &AsyncMutex<OwnedWriteHalf>, event: &AttachEvent) -> bool {
    let Ok(mut bytes) = serde_json::to_vec(event) else { return false };
    bytes.push(b'\n');
    write_half.lock().await.write_all(&bytes).await.is_ok()
}

/// Builds and serializes `agent_id`'s current grid snapshot as a ready-to-
/// write `AttachEvent::Output` line, entirely inside `spawn_blocking`.
/// `None` means no live session for `agent_id` (nothing to send) — distinct
/// from a later write failure, which the caller still detects from the
/// actual socket write's own result.
///
/// Both steps here are CPU-bound and, for a full-screen terminal, together
/// cost tens of milliseconds (measured: ~1MB of JSON, ~60ms to serialize a
/// realistic 200x50 grid in a debug build — a release build is faster but
/// still far from free). The daemon's IPC server runs a single-threaded
/// (`new_current_thread`) tokio runtime, so doing this inline on the async
/// task would stall *every* other connection/agent on the daemon for that
/// whole duration — a real, reported "everything feels slow, not just one
/// pane" bug, not a theoretical one. `spawn_blocking` moves the CPU-bound
/// work onto tokio's separate blocking-thread pool, leaving the single
/// async-runtime thread free to keep servicing every other connection.
async fn build_output_event_bytes(sessions: Arc<SessionManager>, agent_id: crate::agent::AgentId) -> Option<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let grid = sessions.grid_snapshot(&agent_id)?;
        let event = AttachEvent::Output { agent_id, grid };
        let mut bytes = serde_json::to_vec(&event).ok()?;
        bytes.push(b'\n');
        Some(bytes)
    })
    .await
    .unwrap_or(None)
}

/// Writes pre-built `Output` event bytes (from `build_output_event_bytes`) to
/// the connection — the actual socket write is cheap and stays on the async
/// task; only the CPU-bound build step was moved off it.
async fn write_output_bytes(write_half: &AsyncMutex<OwnedWriteHalf>, bytes: Vec<u8>) -> bool {
    write_half.lock().await.write_all(&bytes).await.is_ok()
}

/// Runs one attach connection end-to-end: sends the initial snapshot, spawns
/// the registry-event and terminal-output forwarders, then reads
/// `Watch`/`Unwatch`/`Write`/`Resize` requests until the client disconnects.
/// `Request::Attach` itself never reaches `dispatch` — this is the only
/// handler for an upgraded connection (AGENTS.md "IPC is the only shared
/// channel", extended here: attach is the *streaming* half of it).
async fn handle_attach(
    mut reader: BufReader<OwnedReadHalf>,
    write_half: OwnedWriteHalf,
    ctx: Arc<AppCtx>,
) {
    let write_half = Arc::new(AsyncMutex::new(write_half));

    let snapshot = AttachEvent::Snapshot { agents: ctx.registry.snapshot() };
    if !send_event(&write_half, &snapshot).await {
        return;
    }

    let mut registry_rx = ctx.registry.subscribe();
    let watch: Arc<std::sync::Mutex<WatchState>> =
        Arc::new(std::sync::Mutex::new(WatchState { agent_id: None, last_sent_gen: 0, scroll_dirty: false }));

    // Forwards registry mutations (register/status/remove) as they happen —
    // no polling, per AGENTS.md's "status is push, not pull", now extended
    // to the TUI itself.
    let registry_task = tokio::spawn({
        let write_half = write_half.clone();
        let ctx = ctx.clone();
        async move {
            loop {
                match registry_rx.recv().await {
                    Ok(event) => {
                        if !send_event(&write_half, &event.into()).await {
                            break;
                        }
                    }
                    // A slow client missed some events — there's nothing to
                    // replay them from (the registry only broadcasts), but
                    // silently moving on used to leave the client's local
                    // tree permanently stale for whichever agent's specific
                    // update got dropped in the gap (a real, reported bug:
                    // "agent is not running" shown for a live agent — the
                    // client's own is_alive() reads its last-known status,
                    // per `App::is_alive`'s doc comment, and a missed
                    // StatusChanged with nothing after it to correct the
                    // record leaves that wrong forever). A fresh `Snapshot`
                    // is a full resync — the same mechanism `Watch` already
                    // uses to avoid staleness on switching agents.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = AttachEvent::Snapshot { agents: ctx.registry.snapshot() };
                        if !send_event(&write_half, &snapshot).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });

    // Streams the watched agent's rendered terminal grid, throttled to a
    // content-generation poll (see `session::pty::EventProxy` — the terminal
    // emulator crate has no raw-byte tap without reimplementing its event
    // loop, so this polls rendered content instead of streaming raw PTY
    // bytes). Compares against this connection's own `last_sent_gen` rather
    // than consuming a shared flag (F3), so a second connection watching the
    // same agent doesn't starve this one.
    let output_task = tokio::spawn({
        let write_half = write_half.clone();
        let watch = watch.clone();
        let sessions = ctx.sessions.clone();
        async move {
            let mut interval = tokio::time::interval(OUTPUT_POLL_INTERVAL);
            loop {
                interval.tick().await;
                // `scroll_dirty` is consumed *here*, before the build, not
                // cleared after the send — a scroll landing mid-build just
                // re-marks it and costs one extra resend next tick, whereas
                // clearing afterwards could erase a scroll the in-flight
                // snapshot was built too early to include (same
                // read-before-build reasoning as the `Watch` handler's
                // generation read below).
                let (agent_id, last_sent_gen, scroll_dirty) = {
                    let mut w = watch.lock().unwrap_or_else(|e| e.into_inner());
                    let dirty = std::mem::take(&mut w.scroll_dirty);
                    (w.agent_id.clone(), w.last_sent_gen, dirty)
                };
                let Some(agent_id) = agent_id else { continue };
                let Some(gen) = sessions.generation(&agent_id) else { continue };
                if gen == last_sent_gen && !scroll_dirty {
                    continue;
                }
                let Some(bytes) = build_output_event_bytes(sessions.clone(), agent_id.clone()).await else { continue };
                if !write_output_bytes(&write_half, bytes).await {
                    break;
                }
                // Only record progress if still watching the same agent —
                // a concurrent `Watch` switch already reset `last_sent_gen`
                // for the new agent, and this send was for the old one.
                let mut w = watch.lock().unwrap_or_else(|e| e.into_inner());
                if w.agent_id.as_ref() == Some(&agent_id) {
                    w.last_sent_gen = gen;
                }
            }
        }
    });

    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line_capped(&mut reader, &mut line).await {
            Ok(0) => break,
            Ok(_) => match serde_json::from_slice::<Request>(trim_ascii_whitespace(&line)) {
                Ok(Request::Watch { agent_id }) => {
                    // Read the generation *before* building the snapshot: if
                    // a Wakeup lands mid-build, we'd rather resend once more
                    // on the next tick than record a generation newer than
                    // what we actually sent (which would let a real update
                    // slip past undetected).
                    let gen = ctx.sessions.generation(&agent_id).unwrap_or(0);
                    // Immediate snapshot so switching the watched agent is
                    // instant, not gated on the next poll tick.
                    let sent = if let Some(bytes) = build_output_event_bytes(ctx.sessions.clone(), agent_id.clone()).await {
                        if !write_output_bytes(&write_half, bytes).await {
                            break;
                        }
                        true
                    } else {
                        false
                    };
                    *watch.lock().unwrap_or_else(|e| e.into_inner()) = WatchState {
                        agent_id: Some(agent_id),
                        // No live session to snapshot from (`sent == false`)
                        // — leave last_sent_gen at 0 so the poller doesn't
                        // skip a real first send once the session appears.
                        last_sent_gen: if sent { gen } else { 0 },
                        scroll_dirty: false,
                    };
                }
                Ok(Request::Unwatch) => {
                    *watch.lock().unwrap_or_else(|e| e.into_inner()) =
                        WatchState { agent_id: None, last_sent_gen: 0, scroll_dirty: false };
                }
                Ok(Request::Write { agent_id, data }) => {
                    // Oversized writes are silently dropped rather than
                    // acted on (SECURITY-AUDIT.md F2) -- there's no
                    // `Response` channel on an attach connection to report
                    // an error over, same as the garbage-request case below.
                    if data.len() <= MAX_WRITE_DATA_BYTES {
                        ctx.sessions.write(&agent_id, data.into_bytes());
                    }
                }
                Ok(Request::Resize { cols, lines }) => {
                    // `resize_all` locks and resizes every live session's
                    // `Term` serially (SCALE.md risk #2) -- CPU-bound work
                    // proportional to agent count x grid size, same class of
                    // bug as the grid-snapshot stall this file's `spawn_blocking`
                    // split already exists to prevent. A terminal-window
                    // resize is common enough that leaving this inline would
                    // stall every other connection on the daemon's single-
                    // threaded runtime each time the user resizes.
                    let sessions = ctx.sessions.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        sessions.resize_all(cols as usize, lines as usize);
                    })
                    .await;
                }
                Ok(Request::Scroll { delta }) => {
                    // Scrolling doesn't touch the PTY, so it never bumps the
                    // generation the output poll relies on — mark this
                    // connection's own scroll_dirty flag and let the poller
                    // push the fresh grid on its next tick (≤16ms away)
                    // instead of building + writing a full ~1MB snapshot
                    // inline per request. The inline reply was a real,
                    // reported freeze: a trackpad flick delivers dozens of
                    // notches, each of which stalled this request loop for
                    // the whole serialize+write, queueing keystrokes
                    // (`Write`) behind megabytes of identical grids and
                    // blocking the TUI's own (synchronous) request writes
                    // once the socket buffers filled. Deferring to the
                    // poller coalesces however many scrolls land within a
                    // tick into one send. Skipped entirely when the offset
                    // didn't move (already clamped at the top/bottom).
                    let current = watch.lock().unwrap_or_else(|e| e.into_inner()).agent_id.clone();
                    if let Some(agent_id) = current {
                        if ctx.sessions.scroll_display(&agent_id, delta) {
                            let mut w = watch.lock().unwrap_or_else(|e| e.into_inner());
                            // Only mark if still watching the same agent — a
                            // concurrent `Watch` switch means this scroll
                            // applied to the *old* agent's term, and the new
                            // agent's immediate snapshot is already correct.
                            if w.agent_id.as_ref() == Some(&agent_id) {
                                w.scroll_dirty = true;
                            }
                        }
                    }
                }
                Ok(Request::ScrollToBottom) => {
                    // Same deferred-to-the-poller shape as `Scroll` above.
                    let current = watch.lock().unwrap_or_else(|e| e.into_inner()).agent_id.clone();
                    if let Some(agent_id) = current {
                        if ctx.sessions.scroll_to_bottom(&agent_id) {
                            let mut w = watch.lock().unwrap_or_else(|e| e.into_inner());
                            if w.agent_id.as_ref() == Some(&agent_id) {
                                w.scroll_dirty = true;
                            }
                        }
                    }
                }
                Ok(_) | Err(_) => {
                    // One-shot requests (or garbage) arriving on an attach
                    // connection: there's no `Response` channel to answer on
                    // here, so silently ignore rather than desync the stream.
                }
            },
            Err(_) => break,
        }
    }

    registry_task.abort();
    output_task.abort();
}

/// Wakes every 5 seconds and drains the set of agents whose PTY child has
/// exited since the last tick — event-driven, not polling: `SessionManager`
/// already knows the instant each child exits (`Event::ChildExit`), this just
/// periodically applies that to the registry. Runs only when
/// `ctx.watch_sessions` is true.
///
/// Never removes anything — an exited agent's row stays visible (as `done` or
/// `error`) so the user can review it before an explicit `drop`. That also
/// sidesteps any orphaning concern for an exited parent with live children:
/// nothing is deleted, so nothing can be silently taken out from under them.
async fn session_watcher(ctx: Arc<AppCtx>) {
    // Was 5s — a real user reported a pane looking "frozen" after typing
    // `exit`: the underlying PTY had already died (`SessionManager` knows
    // immediately, via `Event::ChildExit`), but the tree/pane title had no
    // way to reflect that until this sweep next ran and flipped the status
    // to `done`/`error` (`ui::term_pane`'s new "[exited]" marker keys off
    // that same status). Tightened to close the gap between "the process
    // actually died" and "the UI says so" — cheap even at this cadence
    // (`sweep_exited_sessions` is O(live sessions), and does nothing at all
    // when `drain_exits()` is empty, the common case).
    let interval = Duration::from_millis(500);
    loop {
        tokio::time::sleep(interval).await;

        let sessions = ctx.sessions.clone();
        let registry = ctx.registry.clone();
        tokio::task::spawn_blocking(move || sweep_exited_sessions(&registry, &sessions)).await.ok();
    }
}

/// One watcher tick: map each exited PTY's exit status onto `done` (clean exit,
/// code 0 — including a root shell where the user typed `exit`) or `error`
/// (non-zero/signal). Synchronous and side-effect-only against
/// `registry`/`sessions`, so it's directly unit-testable without a tokio runtime.
///
/// Skips *the status update* for an agent that already reports `done`:
/// that's an explicit push from the agent itself declaring the task
/// complete, a stronger signal than this exit-code inference — its wrapping
/// process exiting non-zero afterward (e.g. during its own teardown) must
/// not silently downgrade it to `error`.
///
/// This check is independent of (and not subsumed by)
/// `AgentRegistry::set_status`'s `pushed_at` staleness guard: the exit is
/// detected strictly *after* the explicit `done` push happened (the wrapping
/// process can only exit once its final hook has already run), so a
/// `pushed_at` of "now" here is always chronologically newer, not stale. The
/// guard below is about *authority* (an explicit self-report outranks an
/// inferred one), not about ordering — a different axis than the staleness
/// guard, so both stay.
///
/// Liveness (`AgentRegistry::mark_session_exited`) is recorded unconditionally
/// for every id `drain_exits()` yields, *before* that done-skip check —
/// this sweep is the one and only place with ground truth that the PTY
/// process has actually exited, and an agent that already self-reported
/// `done` while still alive (task complete, session still running — see
/// `AgentNode::session_alive`) needs that fact recorded exactly when it
/// stops being true, not silently dropped because its status update is
/// skipped.
fn sweep_exited_sessions(registry: &AgentRegistry, sessions: &SessionManager) {
    for (id, success) in sessions.drain_exits() {
        registry.mark_session_exited(&id);
        if registry.get(&id).is_some_and(|a| a.status == AgentStatus::Done) {
            continue;
        }
        let (status, message) = if success {
            (AgentStatus::Done, None)
        } else {
            (AgentStatus::Error, Some("agent process exited".to_string()))
        };
        if let Some(agent) = registry.get(&id) {
            eprintln!(
                "overseer: agent {} '{}' ({:?}) swept to {status:?} (clean_exit={success})",
                id.short(),
                agent.name,
                agent.role,
            );
        }
        let _ = registry.set_status(&id, status, message, None, None, std::time::SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spawn::{spawn_agent, SpawnRequest};
    use crate::agent::{AgentId, AgentRole};
    use crate::config::Config;

    // ── read_line_capped / trim_ascii_whitespace (F1) ─────────────────────────

    #[tokio::test]
    async fn read_line_capped_reads_a_normal_line() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"hello\n").await.unwrap();
        drop(client);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();
        let n = read_line_capped(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 6);
        assert_eq!(buf, b"hello\n");
    }

    #[tokio::test]
    async fn read_line_capped_returns_zero_on_clean_eof() {
        let (client, server) = tokio::io::duplex(64);
        drop(client);
        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();
        let n = read_line_capped(&mut reader, &mut buf).await.unwrap();
        assert_eq!(n, 0);
        assert!(buf.is_empty());
    }

    /// The core F1 regression test: a client that streams bytes with no
    /// newline past `MAX_LINE_BYTES` must be rejected, not grown forever.
    #[tokio::test]
    async fn read_line_capped_errors_on_an_unterminated_line_past_the_cap() {
        let (mut client, server) = tokio::io::duplex(8192);
        let mut reader = BufReader::new(server);
        let huge = vec![b'a'; MAX_LINE_BYTES + 1];
        let write_task = tokio::spawn(async move {
            let _ = client.write_all(&huge).await;
        });
        let mut buf = Vec::new();
        let result = read_line_capped(&mut reader, &mut buf).await;
        assert!(result.is_err(), "an unterminated line past the cap must error, not keep growing");
        write_task.abort();
    }

    // ── F9: idle-read timeout ──────────────────────────────────────────────────

    fn make_app_ctx() -> Arc<AppCtx> {
        Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(crate::session::SessionManager::dry_run()),
            socket: PathBuf::from("/tmp/test.sock"),
            git: Arc::new(crate::git::GitClient::dry_run()),
            config: Arc::new(Config::default()),
            watch_sessions: false,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn run_restores_an_unlinked_socket_without_losing_daemon_state() {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        let socket = PathBuf::from(format!("/tmp/ovsr-rebind-{id}.sock"));
        let ctx = make_app_ctx();
        let server_ctx = ctx.clone();
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { run(server_ctx, server_socket, None).await });

        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        std::fs::remove_file(&socket).unwrap();
        assert!(!socket.exists());

        tokio::time::advance(SOCKET_REBIND_INTERVAL * 2).await;
        tokio::task::yield_now().await;
        assert!(
            socket.exists(),
            "the live daemon must recreate its missing socket node"
        );

        let request_socket = socket.clone();
        let response = tokio::task::spawn_blocking(move || {
            crate::ipc::client::send(&request_socket, &Request::List)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(
            response.ok,
            "new one-shot clients must reach the rebound listener"
        );

        ctx.shutdown_notify.notify_one();
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_file(&socket);
    }

    /// A connection that opens but never sends a complete line must be
    /// dropped once `IDLE_READ_TIMEOUT` elapses -- the slow-loris scenario
    /// F9 exists to bound. Uses a paused clock (`start_paused`) so the test
    /// doesn't actually wait out the real 30s timeout.
    #[tokio::test(start_paused = true)]
    async fn handle_conn_drops_an_idle_connection_after_the_timeout() {
        let (mut client, server_side) = tokio::net::UnixStream::pair().unwrap();
        let ctx = make_app_ctx();

        let task = tokio::spawn(handle_conn(server_side, ctx));
        // Let the spawned task run far enough to register its read timer
        // before the clock jumps -- otherwise `advance` can race ahead of
        // the timer even existing yet.
        tokio::task::yield_now().await;

        tokio::time::advance(IDLE_READ_TIMEOUT + Duration::from_secs(1)).await;

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("handle_conn should return once the idle timeout elapses")
            .unwrap();

        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 1];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "server should have closed its end once the idle timeout fired");
    }

    #[test]
    fn trim_ascii_whitespace_strips_leading_and_trailing() {
        assert_eq!(trim_ascii_whitespace(b"  hi \n"), b"hi");
        assert_eq!(trim_ascii_whitespace(b"hi"), b"hi");
        assert_eq!(trim_ascii_whitespace(b"   "), b"");
        assert_eq!(trim_ascii_whitespace(b""), b"");
    }
    use std::path::PathBuf;

    fn spawn(
        registry: &AgentRegistry,
        sessions: &SessionManager,
        role: AgentRole,
        parent_id: Option<AgentId>,
    ) -> AgentId {
        spawn_agent(
            registry,
            sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &Config::default(),
            SpawnRequest {
                role,
                parent_id,
                task: "task".to_string(),
                name: None,
                adapter_name: "claude".to_string(),
                cwd: PathBuf::from("/tmp"),
                repo: "overseer".to_string(),
                branch: None,
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn sweep_marks_clean_exit_done() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);

        sessions.simulate_exit(root_id.clone(), true);
        sweep_exited_sessions(&registry, &sessions);

        let root = registry.get(&root_id).expect("exited agent must stay visible, not be removed");
        assert_eq!(root.status, AgentStatus::Done);
    }

    #[test]
    fn sweep_marks_nonzero_exit_error() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);

        sessions.simulate_exit(root_id.clone(), false);
        sweep_exited_sessions(&registry, &sessions);

        let root = registry.get(&root_id).unwrap();
        assert_eq!(root.status, AgentStatus::Error);
    }

    #[test]
    fn sweep_does_not_downgrade_an_already_done_agent_to_error() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        registry.set_status(&root_id, AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap();

        // The wrapping process exits non-zero after the agent already
        // explicitly reported done — that must not clobber it.
        sessions.simulate_exit(root_id.clone(), false);
        sweep_exited_sessions(&registry, &sessions);

        let root = registry.get(&root_id).unwrap();
        assert_eq!(root.status, AgentStatus::Done, "explicit done must survive a later non-zero exit");
    }

    /// Real bug, reproduced here: an agent that self-reports `done` while its
    /// PTY is still alive (e.g. the user keeps prompting it afterward) must
    /// not be treated as dead. `session_alive` stays `true` until this sweep
    /// actually observes the process exit — `drain_exits()` never yields an
    /// id for a session that hasn't exited, so a `done` push alone must never
    /// flip it.
    #[test]
    fn done_while_still_alive_keeps_session_alive_true_until_a_real_exit() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        registry.set_status(&root_id, AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap();

        // The session never exited (no `simulate_exit`) -- a sweep tick must
        // be a no-op for it, since `drain_exits()` yields nothing.
        sweep_exited_sessions(&registry, &sessions);
        let root = registry.get(&root_id).unwrap();
        assert_eq!(root.status, AgentStatus::Done);
        assert!(root.session_alive, "a done-but-still-running agent must not be reported as dead");

        // Now the process actually exits -- this must flip session_alive
        // even though the done-skip continues past the status-update branch.
        sessions.simulate_exit(root_id.clone(), true);
        sweep_exited_sessions(&registry, &sessions);
        let root = registry.get(&root_id).unwrap();
        assert_eq!(root.status, AgentStatus::Done, "status is untouched by the already-done skip");
        assert!(!root.session_alive, "a real exit must be recorded even when the status update is skipped");
    }

    #[test]
    fn freshly_registered_agent_starts_session_alive() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        assert!(registry.get(&root_id).unwrap().session_alive);
    }

    #[test]
    fn sweep_exit_of_parent_does_not_touch_live_childs_status() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        let child_id = spawn(&registry, &sessions, AgentRole::Child, Some(root_id.clone()));

        // Only the root's PTY exited — the child's own session is untouched.
        sessions.simulate_exit(root_id.clone(), false);
        sweep_exited_sessions(&registry, &sessions);

        let root = registry.get(&root_id).expect("root with a live child must not be removed");
        assert_eq!(root.status, AgentStatus::Error);
        let child = registry.get(&child_id).expect("live child must survive the parent's sweep");
        assert_eq!(child.status, AgentStatus::Spawning, "live child's own status must be untouched");
    }
}
