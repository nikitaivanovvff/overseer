use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
};

use crate::agent::{AgentRegistry, AgentStatus};
use crate::ipc::{handlers::{dispatch, AppCtx}, protocol::{Request, Response}};
use crate::session::SessionManager;

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
                        // Blocking I/O (git, PTY launch) must not block the tokio thread.
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

/// Wakes every 5 seconds and drains the set of agents whose PTY child has
/// exited since the last tick — event-driven, not polling: `SessionManager`
/// already knows the instant each child exits (`Event::ChildExit`), this just
/// periodically applies that to the registry. Runs only when
/// `ctx.watch_sessions` is true.
///
/// `AgentTree::remove` deletes a node's whole subtree in one call, so an exited
/// agent with children is never auto-removed here — that would silently orphan
/// any of its children whose own session is still alive. It's marked `Error`
/// instead, leaving the user to `drop --recursive` deliberately.
async fn session_watcher(ctx: Arc<AppCtx>) {
    let interval = Duration::from_secs(5);
    loop {
        tokio::time::sleep(interval).await;

        let sessions = ctx.sessions.clone();
        let registry = ctx.registry.clone();
        tokio::task::spawn_blocking(move || sweep_exited_sessions(&registry, &sessions)).await.ok();
    }
}

/// One watcher tick: reap leaf agents whose PTY child has exited, and flag
/// (rather than remove) exited agents that still have children — see
/// `session_watcher` above for why removal would be unsafe there. Synchronous
/// and side-effect-only against `registry`/`sessions`, so it's directly
/// unit-testable without a tokio runtime.
fn sweep_exited_sessions(registry: &AgentRegistry, sessions: &SessionManager) {
    let exited = sessions.drain_exits();
    if exited.is_empty() {
        return;
    }

    let ids_with_children: HashSet<_> =
        registry.snapshot().iter().filter_map(|a| a.parent_id.clone()).collect();

    for id in exited {
        if ids_with_children.contains(&id) {
            let _ = registry.set_status(
                &id,
                AgentStatus::Error,
                Some("agent process exited".to_string()),
            );
        } else {
            registry.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spawn::{spawn_agent, SpawnRequest};
    use crate::agent::{AgentId, AgentRole};
    use crate::config::Config;
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
    fn sweep_flags_an_exited_parent_instead_of_removing_it_with_its_live_children() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        let child_id = spawn(&registry, &sessions, AgentRole::Child, Some(root_id.clone()));

        // Only the root's PTY exited — the scenario that used to get the child
        // silently orphaned by a wholesale subtree removal.
        sessions.simulate_exit(root_id.clone());
        sweep_exited_sessions(&registry, &sessions);

        let root = registry.get(&root_id).expect("root with a live child must not be removed");
        assert_eq!(root.status, AgentStatus::Error);
        let child = registry.get(&child_id).expect("live child must survive the parent's sweep");
        assert_eq!(child.status, AgentStatus::Running, "live child must be untouched");
    }

    #[test]
    fn sweep_removes_an_exited_leaf_root() {
        let registry = AgentRegistry::new();
        let sessions = SessionManager::dry_run();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);

        sessions.simulate_exit(root_id.clone());
        sweep_exited_sessions(&registry, &sessions);

        assert!(registry.get(&root_id).is_none());
    }
}
