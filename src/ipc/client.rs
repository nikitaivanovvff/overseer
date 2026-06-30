use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use anyhow::Context;

use crate::ipc::protocol::{Request, Response};

/// Connects to the Overseer socket, sends one request, and returns one response.
/// Synchronous — no tokio needed on the client side.
pub fn send(socket: &Path, req: &Request) -> anyhow::Result<Response> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("failed to connect to {}", socket.display()))?;

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
