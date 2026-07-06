pub mod client;
pub mod protocol;
pub mod handlers;
pub(crate) mod server;

pub use handlers::AppCtx;

use std::{path::PathBuf, sync::Arc};

/// Runs the IPC server on a dedicated OS thread's current-thread tokio runtime.
/// Call this from `std::thread::spawn`; it blocks until the server exits.
pub fn serve_blocking(
    ctx: Arc<AppCtx>,
    socket: PathBuf,
    ready: Option<std::sync::mpsc::SyncSender<()>>,
) -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(server::run(ctx, socket, ready))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRegistry, AgentStatus};
    use crate::git::GitClient;
    use crate::ipc::protocol::{OkBody, Request, Response};
    use crate::session::SessionManager;
    use std::path::{Path, PathBuf};

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Starts a fresh server on a unique socket path and returns the path.
    /// macOS SUN_LEN limit is 104 bytes — keep the path short.
    fn start_server() -> PathBuf {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        let socket = PathBuf::from(format!("/tmp/ovsr-{id}.sock"));
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(SessionManager::dry_run()),
            socket: socket.clone(),
            git: Arc::new(GitClient::dry_run()),
            config: Arc::new(crate::config::Config::default()),
            watch_sessions: false,
        });
        let socket_clone = socket.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = serve_blocking(ctx, socket_clone, Some(ready_tx));
        });
        ready_rx.recv().expect("server failed to start");
        socket
    }

    fn send(socket: &Path, req: Request) -> Response {
        client::send(socket, &req).expect("IPC send failed")
    }

    /// Registers a root via the real `Start` path (`GitClient::dry_run` always
    /// names it "test-repo" — there's no `Register` primitive left to pick an
    /// arbitrary root name).
    fn start_root(socket: &Path) -> AgentId {
        let resp = send(socket, Request::Start { cwd: None });
        assert!(resp.ok, "start root failed: {:?}", resp.error);
        match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    /// Spawns a child via the real `Spawn` path — a child's name is its task
    /// text, so this is how tests still get a recognizable, chosen name.
    fn spawn_child(socket: &Path, parent_id: &AgentId, task: &str) -> (AgentId, String) {
        let resp = send(socket, Request::Spawn {
            parent_id: parent_id.clone(),
            task: task.to_string(),
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        assert!(resp.ok, "spawn child failed: {:?}", resp.error);
        match resp.data {
            Some(OkBody::Registered { agent_id, branch }) => (agent_id, branch),
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    // ── smoke test ────────────────────────────────────────────────────────────

    #[test]
    fn integration_start_and_list() {
        let socket = start_server();

        let root_id = start_root(&socket);

        let list_resp = send(&socket, Request::List);
        assert!(list_resp.ok);
        match list_resp.data {
            Some(OkBody::Agents { agents }) => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, root_id);
                assert_eq!(agents[0].name, "test-repo"); // GitClient::dry_run
            }
            other => panic!("expected Agents, got {other:?}"),
        }

        let _ = std::fs::remove_file(&socket);
    }

    // ── lifecycle tests ───────────────────────────────────────────────────────

    /// Root registers, spawns a child, both appear in list with correct hierarchy.
    #[test]
    fn lifecycle_root_and_child_hierarchy() {
        let socket = start_server();

        let root_id = start_root(&socket);
        let (child_id, child_branch) = spawn_child(&socket, &root_id, "auth-module");

        assert!(child_branch.starts_with("overseer/"), "child branch should be overseer/<id>");

        let list_resp = send(&socket, Request::List);
        assert!(list_resp.ok);
        let agents = match list_resp.data {
            Some(OkBody::Agents { agents }) => agents,
            other => panic!("expected Agents, got {other:?}"),
        };

        assert_eq!(agents.len(), 2);

        let root_dto = agents.iter().find(|a| a.id == root_id).expect("root missing from list");
        assert_eq!(root_dto.branch, "test-branch"); // GitClient::dry_run
        assert!(root_dto.parent_id.is_none(), "root should have no parent");

        let child_dto = agents.iter().find(|a| a.id == child_id).expect("child missing from list");
        assert_eq!(child_dto.parent_id.as_ref(), Some(&root_id), "child parent_id should be root");

        let _ = std::fs::remove_file(&socket);
    }

    /// Status updates are reflected immediately in get and list.
    #[test]
    fn lifecycle_status_updates_visible() {
        let socket = start_server();

        let root_id = start_root(&socket);
        let (child_id, _) = spawn_child(&socket, &root_id, "update-tests");

        // Root starts Idle (bare shell, nothing run yet); child starts Spawning
        // (session launching, not yet reporting).
        let agents = match send(&socket, Request::List).data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!(),
        };
        let root = agents.iter().find(|a| a.id == root_id).unwrap();
        assert_eq!(root.status, AgentStatus::Idle);
        let child = agents.iter().find(|a| a.id == child_id).unwrap();
        assert_eq!(child.status, AgentStatus::Spawning);

        // Child moves to Blocked.
        let resp = send(&socket, Request::Status {
            agent_id: child_id.clone(),
            status: AgentStatus::Blocked,
            message: None,
            context_pct: None,
        });
        assert!(resp.ok, "set child status failed");
        assert!(resp.data.is_none(), "status ack should have no data body");

        // Root moves to Done.
        let resp = send(&socket, Request::Status {
            agent_id: root_id.clone(),
            status: AgentStatus::Done,
            message: Some("all PRs merged".to_string()),
            context_pct: None,
        });
        assert!(resp.ok, "set root status failed");

        // Verify via individual gets.
        let child_dto = match send(&socket, Request::Agent { agent_id: child_id.clone() }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(child_dto.status, AgentStatus::Blocked);

        let root_dto = match send(&socket, Request::Agent { agent_id: root_id.clone() }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(root_dto.status, AgentStatus::Done);

        // Verify via list too.
        let agents = match send(&socket, Request::List).data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!(),
        };
        let child_in_list = agents.iter().find(|a| a.id == child_id).unwrap();
        assert_eq!(child_in_list.status, AgentStatus::Blocked);

        let _ = std::fs::remove_file(&socket);
    }

    /// context_pct flows through the socket and persists across a later push
    /// that doesn't carry one.
    #[test]
    fn lifecycle_context_pct_persists_across_updates_without_it() {
        let socket = start_server();
        let root_id = start_root(&socket);

        let resp = send(&socket, Request::Status {
            agent_id: root_id.clone(),
            status: AgentStatus::Running,
            message: None,
            context_pct: Some(37),
        });
        assert!(resp.ok);

        let dto = match send(&socket, Request::Agent { agent_id: root_id.clone() }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.context_pct, Some(37));

        // A later push with no context_pct must not clear the last known value.
        let resp = send(&socket, Request::Status {
            agent_id: root_id.clone(),
            status: AgentStatus::Idle,
            message: None,
            context_pct: None,
        });
        assert!(resp.ok);

        let dto = match send(&socket, Request::Agent { agent_id: root_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.status, AgentStatus::Idle);
        assert_eq!(dto.context_pct, Some(37), "context_pct must survive a push without one");

        let _ = std::fs::remove_file(&socket);
    }

    /// Multiple children under one root all appear with the root as their parent.
    #[test]
    fn lifecycle_multiple_children() {
        let socket = start_server();

        let root_id = start_root(&socket);
        let (child_a, _) = spawn_child(&socket, &root_id, "task-a");
        let (child_b, _) = spawn_child(&socket, &root_id, "task-b");
        let (child_c, _) = spawn_child(&socket, &root_id, "task-c");

        let agents = match send(&socket, Request::List).data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!(),
        };
        assert_eq!(agents.len(), 4);

        for child_id in [&child_a, &child_b, &child_c] {
            let dto = agents.iter().find(|a| &a.id == child_id).unwrap();
            assert_eq!(dto.parent_id.as_ref(), Some(&root_id));
        }

        let _ = std::fs::remove_file(&socket);
    }

    /// get returns the right agent and carries correct name/adapter/repo/branch.
    #[test]
    fn lifecycle_get_returns_full_detail() {
        let socket = start_server();

        let root_id = start_root(&socket);
        let (child_id, child_branch) = spawn_child(&socket, &root_id, "my-task");

        let dto = match send(&socket, Request::Agent { agent_id: child_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.name, "my-task");
        assert_eq!(dto.adapter, "claude");
        assert_eq!(dto.repo, "test-repo"); // inherited from the parent root (GitClient::dry_run)
        assert_eq!(dto.branch, child_branch);
        assert!(dto.branch.starts_with("overseer/"));
        assert_eq!(dto.status, AgentStatus::Spawning);

        let _ = std::fs::remove_file(&socket);
    }

    // ── start tests ───────────────────────────────────────────────────────────

    #[test]
    fn integration_start_registers_and_returns_id() {
        let socket = start_server();

        let resp = send(&socket, Request::Start { cwd: None });
        assert!(resp.ok, "Start failed: {:?}", resp.error);
        let (agent_id, branch) = match resp.data {
            Some(OkBody::Registered { agent_id, branch }) => (agent_id, branch),
            other => panic!("expected Registered, got {other:?}"),
        };
        assert_eq!(branch, "test-branch"); // GitClient::dry_run

        let list_resp = send(&socket, Request::List);
        let agents = match list_resp.data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!(),
        };
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, agent_id);
        assert_eq!(agents[0].name, "test-repo"); // GitClient::dry_run — root's name = repo

        let _ = std::fs::remove_file(&socket);
    }

    // ── error path tests ──────────────────────────────────────────────────────

    #[test]
    fn lifecycle_error_child_with_unknown_parent() {
        let socket = start_server();

        let resp = send(&socket, Request::Spawn {
            parent_id: AgentId::new(),
            task: "orphan".to_string(),
            adapter: Some("claude".to_string()),
            cwd: PathBuf::from("/tmp"),
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown agent"));

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn lifecycle_error_status_unknown_agent() {
        let socket = start_server();

        let resp = send(&socket, Request::Status {
            agent_id: AgentId::new(),
            status: AgentStatus::Done,
            message: None,
            context_pct: None,
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown agent"));

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn lifecycle_error_get_unknown_agent() {
        let socket = start_server();

        let resp = send(&socket, Request::Agent { agent_id: AgentId::new() });
        assert!(!resp.ok);

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn lifecycle_error_malformed_request() {
        let socket = start_server();

        // Send raw garbage over the socket to verify the server returns a parse error.
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(b"not json at all\n").unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();

        let resp: Response = serde_json::from_str(line.trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("parse error"));

        let _ = std::fs::remove_file(&socket);
    }
}
