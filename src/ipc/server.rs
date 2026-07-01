use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

use crate::agent::spawn::tmux_session_name;
use crate::agent::{AgentRegistry, AgentStatus};
use crate::ipc::{handlers::{dispatch, AppCtx}, protocol::{Request, Response}};
use crate::session::TmuxClient;

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
///
/// `AgentTree::remove` deletes a node's whole subtree in one call, so a dead agent
/// with children is never auto-removed here — that would silently orphan any of its
/// children whose own session is still alive. It's marked `Error` instead, leaving
/// the user to `drop --recursive` deliberately.
async fn session_watcher(ctx: Arc<AppCtx>) {
    let interval = Duration::from_secs(5);
    loop {
        tokio::time::sleep(interval).await;

        let tmux = ctx.tmux.clone();
        let registry = ctx.registry.clone();
        tokio::task::spawn_blocking(move || sweep_dead_sessions(&registry, &tmux)).await.ok();
    }
}

/// One watcher tick: reap leaf agents whose tmux session is gone, and flag (rather
/// than remove) dead agents that still have children — see `session_watcher` above
/// for why removal would be unsafe there. Synchronous and side-effect-only against
/// `registry`/`tmux`, so it's directly unit-testable without a tokio runtime.
fn sweep_dead_sessions(registry: &AgentRegistry, tmux: &TmuxClient) {
    let snapshot = registry.snapshot();
    let ids_with_children: HashSet<_> =
        snapshot.iter().filter_map(|a| a.parent_id.clone()).collect();

    for agent in snapshot {
        if tmux.session_exists(&tmux_session_name(&agent.id)) {
            continue;
        }
        if ids_with_children.contains(&agent.id) {
            let _ = registry.set_status(
                &agent.id,
                AgentStatus::Error,
                Some("tmux session exited".to_string()),
            );
        } else {
            registry.remove(&agent.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spawn::{spawn_agent, SpawnRequest};
    use crate::agent::{AgentId, AgentRole};
    use std::path::PathBuf;

    fn spawn(
        registry: &AgentRegistry,
        tmux: &TmuxClient,
        role: AgentRole,
        parent_id: Option<AgentId>,
    ) -> AgentId {
        spawn_agent(
            registry,
            tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            SpawnRequest {
                role,
                parent_id,
                task: "task".to_string(),
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
    fn sweep_flags_a_dead_parent_instead_of_removing_it_with_its_live_children() {
        let registry = AgentRegistry::new();
        let setup_tmux = TmuxClient::dry_run();
        let root_id = spawn(&registry, &setup_tmux, AgentRole::Root, None);
        let child_id = spawn(&registry, &setup_tmux, AgentRole::Child, Some(root_id.clone()));

        // Root's session is dead, but the child's is still alive — the scenario that
        // used to get the child silently orphaned by a wholesale subtree removal.
        let live: HashSet<_> = [tmux_session_name(&child_id)].into_iter().collect();
        let sweeping_tmux = TmuxClient::dry_run_with_live_sessions(live);

        sweep_dead_sessions(&registry, &sweeping_tmux);

        let root = registry.get(&root_id).expect("root with a live child must not be removed");
        assert_eq!(root.status, AgentStatus::Error);
        let child = registry.get(&child_id).expect("live child must survive the parent's sweep");
        assert_eq!(child.status, AgentStatus::Running, "live child must be untouched");
    }

    #[test]
    fn sweep_removes_a_dead_leaf_root() {
        let registry = AgentRegistry::new();
        let tmux = TmuxClient::dry_run();
        let root_id = spawn(&registry, &tmux, AgentRole::Root, None);

        sweep_dead_sessions(&registry, &tmux);

        assert!(registry.get(&root_id).is_none());
    }
}
