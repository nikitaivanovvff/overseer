use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

use anyhow::Context;

use crate::agent::AgentId;
use crate::ipc::protocol::{AttachEvent, Request, Response, MAX_WRITE_DATA_BYTES};

/// Max size of one response line read from the daemon (SECURITY-AUDIT.md F1,
/// client half). Mirrors `ipc::server::MAX_LINE_BYTES` — a malicious or
/// buggy daemon streaming an unbounded response must not OOM the caller
/// either.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Connects to the Overseer socket. Synchronous — no tokio needed on the client side.
/// Also used as a cheap reachability probe (daemon auto-start, attach setup).
pub fn connect(socket: &Path) -> anyhow::Result<UnixStream> {
    UnixStream::connect(socket).with_context(|| format!("failed to connect to {}", socket.display()))
}

/// Reads one newline-terminated line into `buf`, capped at `MAX_LINE_BYTES`
/// total bytes (F1) — the synchronous, `std::io::BufRead` counterpart of
/// `ipc::server::read_line_capped`.
fn read_line_capped<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let available = reader.fill_buf()?;
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

/// Connects, sends one request, and returns one response.
pub fn send(socket: &Path, req: &Request) -> anyhow::Result<Response> {
    send_on(connect(socket)?, req)
}

/// Same as `send`, but bounds the connection's read/write with `timeout` —
/// used by `overseer kill`'s graceful-first attempt, where an unresponsive
/// (wedged) daemon must not hang the caller indefinitely. `send` itself
/// stays timeout-free: every other caller (ordinary CLI subcommands, the
/// TUI's own attach setup) already assumes a healthy daemon, and a hang
/// there is a real bug worth surfacing as a hang, not something to paper
/// over with a silent timeout.
pub fn send_with_timeout(socket: &Path, req: &Request, timeout: Duration) -> anyhow::Result<Response> {
    let stream = connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    send_on(stream, req)
}

fn send_on(mut stream: UnixStream, req: &Request) -> anyhow::Result<Response> {
    let req_json = serde_json::to_string(req)?;
    stream.write_all(req_json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = Vec::new();
    read_line_capped(&mut reader, &mut line).context("failed to read response from server")?;

    serde_json::from_slice::<Response>(&line).context("failed to parse server response")
}

/// Delay between the prompt text `Write` and the follow-up Enter-keystroke
/// `Write` in `prompt` below. Must stay a **separate**, later `Write` — not
/// `text` and `"\r"` concatenated into one `Write` — because Claude Code's
/// own input widget treats bytes arriving in one fast burst as a paste, and
/// a newline that arrives as part of a paste is inserted literally into the
/// input box rather than submitted. Only a distinct keystroke, arriving
/// noticeably later, reads as a real Enter press. Confirmed empirically;
/// don't collapse this back into a single `Write` without re-verifying
/// against a live Claude Code pane.
const PROMPT_ENTER_DELAY: Duration = Duration::from_millis(600);

/// Submits `text` as a prompt into `agent_id`'s PTY, non-interactively, then
/// disconnects — the one-shot, scriptable counterpart to typing into an
/// agent's pane in the TUI (used by `overseer prompt`). Unlike `send`, this
/// needs a short stateful sequence rather than one request/response: `Write`
/// is only honored on an upgraded `Attach` connection (`ipc::server::
/// handle_attach`), not through the one-shot `dispatch` path, so this opens
/// its own attach connection, discards the mandatory initial
/// `AttachEvent::Snapshot`, writes the text, waits (`PROMPT_ENTER_DELAY`),
/// writes a bare Enter as a second, separate `Write`, and returns — no need
/// to stay attached afterward.
pub fn prompt(socket: &Path, agent_id: &AgentId, text: &str) -> anyhow::Result<()> {
    if text.len() > MAX_WRITE_DATA_BYTES {
        anyhow::bail!("prompt text exceeds max size of {MAX_WRITE_DATA_BYTES} bytes");
    }

    let mut stream = connect(socket)?;
    write_line(&mut stream, &Request::Attach)?;

    {
        let mut reader = BufReader::new(&stream);
        let mut line = Vec::new();
        read_line_capped(&mut reader, &mut line).context("failed to read attach snapshot")?;
        let event: AttachEvent =
            serde_json::from_slice(&line).context("failed to parse attach snapshot")?;
        anyhow::ensure!(
            matches!(event, AttachEvent::Snapshot { .. }),
            "expected an initial Snapshot event, got something else"
        );
    }

    write_line(&mut stream, &Request::Write { agent_id: agent_id.clone(), data: text.to_string() })?;
    std::thread::sleep(PROMPT_ENTER_DELAY);
    write_line(&mut stream, &Request::Write { agent_id: agent_id.clone(), data: "\r".to_string() })?;

    Ok(())
}

fn write_line(stream: &mut UnixStream, req: &Request) -> anyhow::Result<()> {
    let json = serde_json::to_string(req)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush().context("failed to write request to server")
}
