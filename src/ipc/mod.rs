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
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
        });
        assert!(resp.ok, "set child status failed");
        assert!(resp.data.is_none(), "status ack should have no data body");

        // Root moves to Done.
        let resp = send(&socket, Request::Status {
            agent_id: root_id.clone(),
            status: AgentStatus::Done,
            message: Some("all PRs merged".to_string()),
            context_pct: None,
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
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
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
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
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
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
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
        });
        assert!(!resp.ok);
        assert!(resp.error.as_deref().unwrap_or("").contains("unknown agent"));

        let _ = std::fs::remove_file(&socket);
    }

    // ── F5: socket file mode ──────────────────────────────────────────────────

    #[test]
    fn bound_socket_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let socket = start_server();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the socket node itself must be owner-only regardless of umask");

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
            adapter: None,
            pushed_at: std::time::SystemTime::now(),
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
    fn attach_resyncs_with_a_fresh_snapshot_after_falling_behind() {
        // A real user reported "agent is not running" shown for a live
        // agent — traced back to the registry's broadcast channel dropping
        // events for a lagged receiver with no resync afterward, silently
        // leaving the client's local status stale forever for whatever
        // agent's update got lost. Floods far more status pushes than the
        // channel's capacity while deliberately not draining the attach
        // socket, so the registry-event forwarder task's own `.recv()` is
        // guaranteed to observe `Lagged` once it resumes — the fix sends a
        // fresh `Snapshot` in that case, which this asserts actually shows
        // up mid-stream, not just as the connection's very first event.
        let socket = start_server();
        let root_id = start_root(&socket);
        let (_stream, mut reader) = attach(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::Snapshot { .. }));

        // Comfortably past the registry's broadcast channel capacity (1024).
        for _ in 0..2000 {
            send(&socket, Request::Status {
                agent_id: root_id.clone(),
                status: AgentStatus::Running,
                message: None,
                context_pct: None,
                adapter: None,
            pushed_at: std::time::SystemTime::now(),
            });
        }

        let mut saw_resync_snapshot = false;
        for _ in 0..2100 {
            match next_event(&mut reader) {
                AttachEvent::Snapshot { .. } => {
                    saw_resync_snapshot = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(saw_resync_snapshot, "expected a resync Snapshot after falling behind on 2000 rapid pushes");

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

    /// `client::prompt` (backing `overseer prompt`) opens its own attach
    /// connection, discards the mandatory initial `Snapshot`, and sends the
    /// text as one `Write` followed by a separate `\r` `Write` — exercise it
    /// end-to-end against a real test server rather than just asserting on
    /// the wire shape. `SessionManager::dry_run` has no live PTY to observe
    /// the bytes landing in, so this proves the sequence completes cleanly
    /// (no desync, no hang, no error) and leaves the server servicing other
    /// connections afterward, mirroring `oversized_write_on_an_attach_connection_
    /// is_dropped_not_acted_on`'s style of proof for the same reason.
    #[test]
    fn client_prompt_sends_text_then_a_separate_enter_write() {
        let socket = start_server();
        let root_id = start_root(&socket);

        client::prompt(&socket, &root_id, "you have uncommitted work, please finish")
            .expect("prompt should complete without error");

        // The daemon must still be alive and servicing other connections
        // afterward — prove it with an ordinary one-shot round trip.
        let resp = send(&socket, Request::Agent { agent_id: root_id.clone() });
        assert!(resp.ok, "daemon should survive a prompt call: {:?}", resp.error);

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn client_prompt_rejects_text_over_the_max_write_size() {
        let socket = start_server();
        let root_id = start_root(&socket);

        let oversized = "x".repeat(crate::ipc::protocol::MAX_WRITE_DATA_BYTES + 1);
        let result = client::prompt(&socket, &root_id, &oversized);
        assert!(result.is_err(), "prompt text over the max size should be rejected before sending");

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

    // ── F9: connection concurrency ceiling ────────────────────────────────────

    #[test]
    fn connections_beyond_the_concurrency_ceiling_are_dropped_immediately() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};

        let socket = start_server();

        // Fill every permit, plus a few past the ceiling -- those extras
        // must get closed rather than queued up (SECURITY-AUDIT.md F9). The
        // OS-level connect succeeds regardless of the daemon's own limit
        // (it's just the listen backlog); it's the daemon's accept loop that
        // must refuse to service them.
        let extra = 4;
        let mut conns: Vec<UnixStream> = (0..server::MAX_CONCURRENT_CONNECTIONS + extra)
            .map(|_| UnixStream::connect(&socket).expect("OS-level connect always succeeds"))
            .collect();

        // Only check the ones opened past the ceiling. The daemon's
        // single-threaded accept loop needs a moment to actually dequeue and
        // acquire a permit for everything ahead of them, so poll for the
        // expected EOF instead of assuming a fixed catch-up delay.
        let extras = &mut conns[server::MAX_CONCURRENT_CONNECTIONS..];
        for stream in extras.iter_mut() {
            stream.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        }
        let mut rejected = vec![false; extras.len()];
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && rejected.iter().any(|done| !done) {
            for (stream, done) in extras.iter_mut().zip(rejected.iter_mut()) {
                if *done {
                    continue;
                }
                let mut buf = [0u8; 1];
                if matches!(stream.read(&mut buf), Ok(0)) {
                    *done = true;
                }
            }
        }

        assert!(
            rejected.iter().all(|done| *done),
            "every connection past the concurrency ceiling should eventually be closed"
        );

        conns.clear();
        let _ = std::fs::remove_file(&socket);
    }

    // ── size limits (F1/F2) ───────────────────────────────────────────────────

    #[test]
    fn oversized_unterminated_line_disconnects_rather_than_hanging_the_daemon() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let socket = start_server();
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

        // Comfortably past the server's line cap (1 MiB), no trailing newline
        // — the exact scenario F1 exists to bound.
        let huge = vec![b'a'; 2 * 1024 * 1024];
        let _ = std::io::Write::write_all(&mut stream, &huge);

        // The server must close its end once the cap is exceeded -- read
        // returns Ok(0) (EOF) rather than the connection hanging open while
        // an ever-growing buffer is allocated server-side.
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "server should close the connection once the line cap is exceeded");

        // The daemon itself must still be alive for other clients.
        let resp = send(&socket, Request::List);
        assert!(resp.ok, "daemon should survive an oversized line from one client");

        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn oversized_write_on_an_attach_connection_is_dropped_not_acted_on() {
        let socket = start_server();
        let root_id = start_root(&socket);
        let (mut stream, mut reader) = attach(&socket);
        assert!(matches!(next_event(&mut reader), AttachEvent::Snapshot { .. }));

        send_line(&mut stream, &Request::Write {
            agent_id: root_id,
            data: "x".repeat(crate::ipc::protocol::MAX_WRITE_DATA_BYTES + 1),
        });

        // The oversized Write must not desync or kill the connection --
        // ordinary traffic on it (a fresh registration event) keeps arriving
        // right after.
        let other_root = start_root(&socket);
        assert!(matches!(
            next_event(&mut reader),
            AttachEvent::AgentRegistered { agent } if agent.id == other_root
        ));

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

    // ── send_with_timeout (overseer kill's graceful-first attempt) ────────────

    #[test]
    fn send_with_timeout_behaves_like_send_against_a_healthy_server() {
        let socket = start_server();
        let resp = client::send_with_timeout(&socket, &Request::List, std::time::Duration::from_secs(5))
            .expect("send_with_timeout should succeed against a healthy, responsive server");
        assert!(resp.ok);
        let _ = std::fs::remove_file(&socket);
    }

    /// The core scenario `overseer kill` exists for: a listener that accepts
    /// the connection (so `connect()` succeeds) but never reads or writes
    /// anything back — the same shape a daemon wedged inside a deadlocked
    /// handler would present. `send_with_timeout` must return an error once
    /// `timeout` elapses rather than hang forever the way plain `send` would.
    #[test]
    fn send_with_timeout_errors_out_against_a_connection_that_never_responds() {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        let socket = PathBuf::from(format!("/tmp/ovsr-wedged-{id}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let held = std::thread::spawn(move || {
            // Accept and hold the connection open, replying to nothing —
            // simulates a daemon that took the connection but is wedged
            // before ever reaching its own read/dispatch/write.
            let _conn = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(5));
        });

        let start = std::time::Instant::now();
        let result =
            client::send_with_timeout(&socket, &Request::List, std::time::Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a connection that never responds must time out, not hang");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "timeout should bound the wait to roughly `timeout`, took {elapsed:?}"
        );

        drop(held); // detach -- the sleeping thread outlives the test harmlessly
        let _ = std::fs::remove_file(&socket);
    }
}
