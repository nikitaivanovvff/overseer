use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use anyhow::Context;

use crate::ipc::protocol::{Request, Response};

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
    let mut stream = connect(socket)?;

    let req_json = serde_json::to_string(req)?;
    stream.write_all(req_json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = Vec::new();
    read_line_capped(&mut reader, &mut line).context("failed to read response from server")?;

    serde_json::from_slice::<Response>(&line).context("failed to parse server response")
}
