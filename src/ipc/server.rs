use std::{path::PathBuf, sync::Arc};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

use crate::agent::AgentRegistry;
use crate::ipc::{handlers, protocol::{Request, Response}};

pub async fn run(
    reg: Arc<AgentRegistry>,
    socket: PathBuf,
    ready: Option<std::sync::mpsc::SyncSender<()>>,
) -> std::io::Result<()> {
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }

    let listener = UnixListener::bind(&socket)?;
    if let Some(tx) = ready {
        let _ = tx.send(());
    }

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                use std::io::ErrorKind::*;
                match e.kind() {
                    ConnectionAborted | ConnectionReset | Interrupted => continue,
                    _ => return Err(e),
                }
            }
        };
        let reg = reg.clone();
        tokio::spawn(handle_conn(stream, reg));
    }
}

async fn handle_conn(stream: tokio::net::UnixStream, reg: Arc<AgentRegistry>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let resp = match serde_json::from_str::<Request>(line.trim()) {
                    Ok(req) => handlers::dispatch(&reg, req),
                    Err(e) => Response::err(format!("parse error: {e}")),
                };
                // Lock is released inside dispatch before we reach here.
                let mut bytes = serde_json::to_vec(&resp)
                    .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"internal serialization error\"}".to_vec());
                bytes.push(b'\n');
                if write_half.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
