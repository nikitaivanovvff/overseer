use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use anyhow::Context;

use crate::ipc::protocol::{Request, Response};

/// Connects to the Overseer socket. Synchronous — no tokio needed on the client side.
/// Also used as a cheap reachability probe (daemon auto-start, attach setup).
pub fn connect(socket: &Path) -> anyhow::Result<UnixStream> {
    UnixStream::connect(socket).with_context(|| format!("failed to connect to {}", socket.display()))
}

/// Connects, sends one request, and returns one response.
pub fn send(socket: &Path, req: &Request) -> anyhow::Result<Response> {
    let mut stream = connect(socket)?;

    let req_json = serde_json::to_string(req)?;
    stream.write_all(req_json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("failed to read response from server")?;

    serde_json::from_str::<Response>(line.trim()).context("failed to parse server response")
}
