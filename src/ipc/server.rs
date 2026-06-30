use std::{path::PathBuf, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

use crate::agent::spawn::tmux_session_name;
use crate::ipc::{handlers::{dispatch, AppCtx}, protocol::{Request, Response}};

pub async fn run(
    ctx: Arc<AppCtx>,
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

    if ctx.watch_sessions {
        tokio::spawn(session_watcher(ctx.clone()));
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
        let ctx = ctx.clone();
        tokio::spawn(handle_conn(stream, ctx));
    }
}

async fn handle_conn(stream: tokio::net::UnixStream, ctx: Arc<AppCtx>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let resp = match serde_json::from_str::<Request>(line.trim()) {
                    Ok(req) => {
                        // Blocking I/O (git, tmux) must not block the tokio thread.
                        let ctx = ctx.clone();
                        tokio::task::spawn_blocking(move || dispatch(&ctx, req))
                            .await
                            .unwrap_or_else(|_| Response::err("handler panicked"))
                    }
                    Err(e) => Response::err(format!("parse error: {e}")),
                };
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

/// Polls every 5 seconds and removes any agent whose tmux session no longer exists.
/// Covers both clean exits (Stop hook may or may not have fired) and crashed sessions.
/// Runs only when `ctx.watch_sessions` is true.
async fn session_watcher(ctx: Arc<AppCtx>) {
    let interval = Duration::from_secs(5);
    loop {
        tokio::time::sleep(interval).await;

        let ids: Vec<_> = ctx.registry.snapshot().into_iter().map(|a| a.id).collect();

        let tmux = ctx.tmux.clone();
        let registry = ctx.registry.clone();

        tokio::task::spawn_blocking(move || {
            for id in ids {
                if !tmux.session_exists(&tmux_session_name(&id)) {
                    registry.remove(&id);
                }
            }
        })
        .await
        .ok();
    }
}
