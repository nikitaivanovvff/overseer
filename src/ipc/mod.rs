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
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
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

    /// Spawns a child via the real `Spawn` path with no explicit `name` — a
    /// child's name falls back to its task text, so this is how tests still
    /// get a recognizable, chosen name.
    fn spawn_child(socket: &Path, parent_id: &AgentId, task: &str) -> (AgentId, String) {
        let resp = send(socket, Request::Spawn {
            parent_id: parent_id.clone(),
            task: task.to_string(),
            name: None,
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
            name: None,
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

    // ── attach protocol ───────────────────────────────────────────────────────

    use crate::ipc::protocol::AttachEvent;
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::os::unix::net::UnixStream;

    /// Opens a fresh connection and upgrades it via `Request::Attach`.
    fn attach(socket: &Path) -> (UnixStream, StdBufReader<UnixStream>) {
        let mut stream = UnixStream::connect(socket).expect("connect for attach");
        let req = serde_json::to_string(&Request::Attach).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let reader = StdBufReader::new(stream.try_clone().unwrap());
        (stream, reader)
    }

    fn next_event(reader: &mut StdBufReader<UnixStream>) -> AttachEvent {
        let mut line = String::new();
        reader.read_line(&mut line).expect("attach connection closed unexpectedly");
        serde_json::from_str(line.trim()).expect("malformed AttachEvent line")
    }

    fn send_line(stream: &mut UnixStream, req: &Request) {
        let json = serde_json::to_string(req).unwrap();
        stream.write_all(json.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn attach_sends_an_empty_snapshot_first() {
        let socket = start_server();
        let (_stream, mut reader) = attach(&socket);
        let event = next_event(&mut reader);
        assert!(matches!(event, AttachEvent::Snapshot { agents } if agents.is_empty()));
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn attach_snapshot_reflects_agents_registered_before_it_connected() {
        let socket = start_server();
        let root_id = start_root(&socket);

        let (_stream, mut reader) = attach(&socket);
        let event = next_event(&mut reader);
        match event {
            AttachEvent::Snapshot { agents } => {
                assert_eq!(agents.len(), 1);
                assert_eq!(agents[0].id, root_id);
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn attach_streams_registered_and_status_changed_events_live() {
        let socket = start_server();
        let (_stream, mut reader) = attach(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::Snapshot { .. }));

        // A one-shot connection registers a root while we're attached...
        let root_id = start_root(&socket);
        match next_event(&mut reader) {
            AttachEvent::AgentRegistered { agent } => assert_eq!(agent.id, root_id),
            other => panic!("expected AgentRegistered, got {other:?}"),
        }

        // ...and pushes a status update...
        send(&socket, Request::Status {
            agent_id: root_id.clone(),
            status: AgentStatus::Blocked,
            message: Some("needs you".to_string()),
            context_pct: Some(42),
        });
        match next_event(&mut reader) {
            AttachEvent::StatusChanged { agent_id, status, message, context_pct } => {
                assert_eq!(agent_id, root_id);
                assert_eq!(status, AgentStatus::Blocked);
                assert_eq!(message.as_deref(), Some("needs you"));
                assert_eq!(context_pct, Some(42));
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }

        // ...both landing on the same attach connection the whole time, live.
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn attach_streams_removed_event() {
        let socket = start_server();
        let root_id = start_root(&socket);
        let (child_id, _) = spawn_child(&socket, &root_id, "doomed-child");

        let (_stream, mut reader) = attach(&socket);
        match next_event(&mut reader) {
            AttachEvent::Snapshot { agents } => assert_eq!(agents.len(), 2),
            other => panic!("expected Snapshot, got {other:?}"),
        }

        send(&socket, Request::Drop { agent_id: child_id.clone(), recursive: false });
        match next_event(&mut reader) {
            AttachEvent::AgentRemoved { agent_id } => assert_eq!(agent_id, child_id),
            other => panic!("expected AgentRemoved, got {other:?}"),
        }

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn attach_connection_still_accepts_watch_unwatch_resize_without_crashing() {
        // Exercises the request-reading half of the attach loop against a
        // dry-run SessionManager (no real PTY, so no grid ever comes back) —
        // the point here is that these requests don't desync or kill the
        // connection, not the rendered content (covered in session::pty's
        // own unit tests against a real session).
        let socket = start_server();
        let root_id = start_root(&socket);
        let (mut stream, mut reader) = attach(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::Snapshot { .. }));

        send_line(&mut stream, &Request::Watch { agent_id: root_id.clone() });
        send_line(&mut stream, &Request::Resize { cols: 100, lines: 40 });
        send_line(&mut stream, &Request::Scroll { delta: 5 });
        send_line(&mut stream, &Request::ScrollToBottom);
        send_line(&mut stream, &Request::Unwatch);

        // The connection must still be alive and forwarding registry events
        // after all three — prove it by registering a child and reading the
        // event through, instead of asserting on a fixed sleep.
        let (_child_id, _) = spawn_child(&socket, &root_id, "post-watch-child");
        assert!(matches!(next_event(&mut reader), AttachEvent::AgentRegistered { .. }));

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn scroll_with_nothing_watched_is_a_harmless_noop() {
        let socket = start_server();
        let (mut stream, mut reader) = attach(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::Snapshot { .. }));

        // No Watch has been sent yet — Scroll/ScrollToBottom must not panic
        // or desync the connection.
        send_line(&mut stream, &Request::Scroll { delta: 3 });
        send_line(&mut stream, &Request::ScrollToBottom);

        let root_id = start_root(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::AgentRegistered { agent } if agent.id == root_id));

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn one_shot_connections_keep_working_alongside_an_open_attach_connection() {
        let socket = start_server();
        let (_stream, _reader) = attach(&socket);

        // Ordinary one-shot request/response traffic must be unaffected by an
        // idle attach connection sitting on the same server.
        let resp = send(&socket, Request::List);
        assert!(resp.ok);
        assert!(matches!(resp.data, Some(OkBody::Agents { agents }) if agents.is_empty()));

        let _ = std::fs::remove_file(&socket);
    }

    // ── shutdown ──────────────────────────────────────────────────────────────

    #[test]
    fn shutdown_response_arrives_before_the_server_stops_accepting() {
        let socket = start_server();
        let root_id = start_root(&socket);

        let resp = send(&socket, Request::Shutdown);
        assert!(resp.ok, "Shutdown failed: {:?}", resp.error);

        // The response above already proves delivery survived the server
        // tearing down — a bonus check that the tree really was cleared
        // before the accept loop stopped (best-effort: the server may already
        // be gone by the time this connects, which is also a valid outcome).
        let _ = root_id;
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn shutdown_pushes_an_attach_event_to_watching_clients() {
        let socket = start_server();
        let (_stream, mut reader) = attach(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::Snapshot { .. }));

        let resp = send(&socket, Request::Shutdown);
        assert!(resp.ok, "Shutdown failed: {:?}", resp.error);

        assert!(matches!(next_event(&mut reader), AttachEvent::Shutdown));
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn server_stops_accepting_new_connections_after_shutdown() {
        let socket = start_server();
        let resp = send(&socket, Request::Shutdown);
        assert!(resp.ok, "Shutdown failed: {:?}", resp.error);

        // The accept loop returning is asynchronous relative to this thread
        // observing it — retry briefly rather than asserting on the very next
        // instant. A bare `connect()` isn't a reliable signal on its own: a
        // connection already queued in the kernel's listen backlog can still
        // succeed for a moment after the listener closes, then yield EOF with
        // no response — so check for a full round-trip failing, not just the
        // connect.
        let mut daemon_gone = false;
        for _ in 0..100 {
            if client::send(&socket, &Request::List).is_err() {
                daemon_gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(daemon_gone, "server should stop accepting connections once shut down");

        let _ = std::fs::remove_file(&socket);
    }
}
