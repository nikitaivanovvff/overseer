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
    use crate::agent::{AgentId, AgentRegistry, AgentRole, AgentStatus};
    use crate::git::GitClient;
    use crate::ipc::protocol::{OkBody, Request, Response};
    use crate::session::TmuxClient;
    use std::path::Path;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Starts a fresh server on a unique socket path and returns the path.
    /// macOS SUN_LEN limit is 104 bytes — keep the path short.
    fn start_server() -> PathBuf {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        let socket = PathBuf::from(format!("/tmp/ovsr-{id}.sock"));
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            tmux: Arc::new(TmuxClient::dry_run()),
            socket: socket.clone(),
            git: Arc::new(GitClient::dry_run()),
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

    fn register_root(socket: &Path, name: &str) -> AgentId {
        let resp = send(socket, Request::Register {
            id: None,
            name: name.to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: Some("claude".to_string()),
            repo: Some("overseer".to_string()),
        });
        assert!(resp.ok, "register root failed: {:?}", resp.error);
        match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    fn register_child(socket: &Path, name: &str, parent_id: &AgentId) -> (AgentId, String) {
        let resp = send(socket, Request::Register {
            id: None,
            name: name.to_string(),
            role: AgentRole::Child,
            parent_id: Some(parent_id.clone()),
            adapter: Some("claude".to_string()),
            repo: Some("overseer".to_string()),
        });
        assert!(resp.ok, "register child failed: {:?}", resp.error);
        match resp.data {
            Some(OkBody::Registered { agent_id, branch }) => (agent_id, branch),
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    // ── smoke test ────────────────────────────────────────────────────────────

    #[test]
    fn integration_register_and_list() {
        let socket = start_server();

        let root_id = register_root(&socket, "integration-agent");

        let list_resp = send(&socket, Request::List);
        assert!(list_resp.ok);
        match list_resp.data {
            Some(OkBody::Agents { agents }) => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, root_id);
                assert_eq!(agents[0].name, "integration-agent");
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

        let root_id = register_root(&socket, "implement-auth");
        let (child_id, child_branch) = register_child(&socket, "auth-module", &root_id);

        assert!(child_branch.starts_with("overseer/"), "child branch should be overseer/<id>");

        let list_resp = send(&socket, Request::List);
        assert!(list_resp.ok);
        let agents = match list_resp.data {
            Some(OkBody::Agents { agents }) => agents,
            other => panic!("expected Agents, got {other:?}"),
        };

        assert_eq!(agents.len(), 2);

        let root_dto = agents.iter().find(|a| a.id == root_id).expect("root missing from list");
        assert_eq!(root_dto.branch, "main");
        assert!(root_dto.parent_id.is_none(), "root should have no parent");

        let child_dto = agents.iter().find(|a| a.id == child_id).expect("child missing from list");
        assert_eq!(child_dto.parent_id.as_ref(), Some(&root_id), "child parent_id should be root");

        let _ = std::fs::remove_file(&socket);
    }

    /// Status updates are reflected immediately in get and list.
    #[test]
    fn lifecycle_status_updates_visible() {
        let socket = start_server();

        let root_id = register_root(&socket, "refactor-api");
        let (child_id, _) = register_child(&socket, "update-tests", &root_id);

        // Both start as Running (default).
        let agents = match send(&socket, Request::List).data {
            Some(OkBody::Agents { agents }) => agents,
            _ => panic!(),
        };
        assert!(agents.iter().all(|a| a.status == AgentStatus::Running));

        // Child moves to Waiting.
        let resp = send(&socket, Request::Status {
            agent_id: child_id.clone(),
            status: AgentStatus::Waiting,
            message: None,
        });
        assert!(resp.ok, "set child status failed");
        assert!(resp.data.is_none(), "status ack should have no data body");

        // Root moves to Done.
        let resp = send(&socket, Request::Status {
            agent_id: root_id.clone(),
            status: AgentStatus::Done,
            message: Some("all PRs merged".to_string()),
        });
        assert!(resp.ok, "set root status failed");

        // Verify via individual gets.
        let child_dto = match send(&socket, Request::Agent { agent_id: child_id.clone() }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(child_dto.status, AgentStatus::Waiting);

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
        assert_eq!(child_in_list.status, AgentStatus::Waiting);

        let _ = std::fs::remove_file(&socket);
    }

    /// Multiple children under one root all appear with the root as their parent.
    #[test]
    fn lifecycle_multiple_children() {
        let socket = start_server();

        let root_id = register_root(&socket, "big-feature");
        let (child_a, _) = register_child(&socket, "task-a", &root_id);
        let (child_b, _) = register_child(&socket, "task-b", &root_id);
        let (child_c, _) = register_child(&socket, "task-c", &root_id);

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

    /// get returns the right agent and carries correct adapter/repo/branch.
    #[test]
    fn lifecycle_get_returns_full_detail() {
        let socket = start_server();

        let resp = send(&socket, Request::Register {
            id: None,
            name: "my-task".to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: Some("aider".to_string()),
            repo: Some("my-repo".to_string()),
        });
        let root_id = match resp.data {
            Some(OkBody::Registered { agent_id, .. }) => agent_id,
            _ => panic!(),
        };

        let dto = match send(&socket, Request::Agent { agent_id: root_id }).data {
            Some(OkBody::Agent { agent }) => agent,
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(dto.name, "my-task");
        assert_eq!(dto.adapter, "aider");
        assert_eq!(dto.repo, "my-repo");
        assert_eq!(dto.branch, "main");
        assert_eq!(dto.status, AgentStatus::Running);

        let _ = std::fs::remove_file(&socket);
    }

    // ── start tests ───────────────────────────────────────────────────────────

    #[test]
    fn integration_start_registers_and_returns_id() {
        let socket = start_server();

        let resp = send(&socket, Request::Start {
            task: "implement auth".to_string(),
            adapter: Some("claude".to_string()),
            cwd: None,
        });
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

        let _ = std::fs::remove_file(&socket);
    }

    // ── error path tests ──────────────────────────────────────────────────────

    #[test]
    fn lifecycle_error_child_with_unknown_parent() {
        let socket = start_server();

        let resp = send(&socket, Request::Register {
            id: None,
            name: "orphan".to_string(),
            role: AgentRole::Child,
            parent_id: Some(AgentId::new()),
            adapter: None,
            repo: None,
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown parent"));

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn lifecycle_error_status_unknown_agent() {
        let socket = start_server();

        let resp = send(&socket, Request::Status {
            agent_id: AgentId::new(),
            status: AgentStatus::Done,
            message: None,
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
