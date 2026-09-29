//! Exercise the compiled CLI against a private, deterministic IPC peer. These
//! tests never start a harness, call a provider, or touch user configuration.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use overseer_core::agent::{AgentId, AgentStatus};
use overseer_core::ipc::protocol::{OkBody, Request, Response};
use serde_json::{json, Value};

struct Peer {
    path: PathBuf,
    listener: UnixListener,
    agent_id: AgentId,
}

impl Peer {
    fn new() -> Self {
        let path = PathBuf::from(format!("/tmp/ovsr-context-{}.sock", uuid::Uuid::new_v4()));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            path,
            listener,
            agent_id: AgentId::new(),
        }
    }

    fn command(&self, subcommand: &str, identified: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_overseer"));
        command.arg(subcommand).arg("--socket").arg(&self.path);
        // Remove inherited Overseer identity only for this child process.
        for key in [
            "OVERSEER_AGENT_ID",
            "OVERSEER_ROLE",
            "OVERSEER_TASK",
            "OVERSEER_PARENT_ID",
        ] {
            command.env_remove(key);
        }
        if identified {
            command
                .env("OVERSEER_AGENT_ID", self.agent_id.0.to_string())
                .env("OVERSEER_ROLE", "child");
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn serve(&self, count: usize, context: &str) -> JoinHandle<Vec<Request>> {
        let listener = self.listener.try_clone().unwrap();
        let context = context.to_owned();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            while requests.len() < count {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "CLI did not send expected requests"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept failed: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                let response = match &request {
                    Request::Context { .. } => Response::ok(Some(OkBody::Context {
                        context: context.clone(),
                        contract_version: 1,
                    })),
                    Request::Status { .. } => Response::ok(None),
                    unexpected => panic!("unexpected request: {unexpected:?}"),
                };
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
                requests.push(request);
            }
            requests
        })
    }

    fn assert_no_requests(&self) {
        assert!(
            matches!(self.listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn run(mut command: Command, payload: &[u8]) -> Output {
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Err(error) = input.write_all(payload) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn session_start_publishes_activity_then_emits_daemon_role_context() {
    let peer = Peer::new();
    let context =
        "You are a reviewer. Inspect the assigned changes; report findings.\nQuoted: \"safe\".";
    let server = peer.serve(2, context);
    let mut command = peer.command("codex-hook", true);
    command.env("OVERSEER_TASK", "Review authentication changes");
    let output = run(
        command,
        &serde_json::to_vec(&json!({
            "hook_event_name": "SessionStart", "source": "startup", "model": "verified-model"
        }))
        .unwrap(),
    );
    let requests = server.join().unwrap();
    assert!(matches!(&requests[0], Request::Status {
        agent_id, status: AgentStatus::Running, model_name: Some(model), adapter: Some(adapter), clear_context: true, ..
    } if agent_id == &peer.agent_id && model == "verified-model" && adapter == "codex"));
    assert!(matches!(&requests[1], Request::Context { agent_id } if agent_id == &peer.agent_id));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value,
        json!({"hookSpecificOutput": {
            "hookEventName": "SessionStart", "additionalContext": context
        }})
    );
}

#[test]
fn stop_reports_idle_without_completing_assignment_or_emitting_context() {
    let peer = Peer::new();
    let server = peer.serve(1, "must not be emitted");
    let output = run(
        peer.command("codex-hook", true),
        br#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#,
    );
    let requests = server.join().unwrap();
    assert!(matches!(&requests[0], Request::Status {
        agent_id, status: AgentStatus::Idle, message: None, clear_context: false, ..
    } if agent_id == &peer.agent_id));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    peer.assert_no_requests();
}

#[test]
fn native_subagents_invalid_payloads_and_unmanaged_sessions_do_not_send_status() {
    let peer = Peer::new();
    for payload in [
        br#"{"hook_event_name":"SubagentStop","agent_id":"native-child"}"#.to_vec(),
        br#"{"hook_event_name":"Stop","agent_type":"native-reviewer"}"#.to_vec(),
        b"invalid json".to_vec(),
        vec![b' '; 64 * 1024 + 1],
    ] {
        let output = run(peer.command("codex-hook", true), &payload);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({})
        );
    }
    let output = run(
        peer.command("codex-hook", false),
        br#"{"hook_event_name":"Stop"}"#,
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    peer.assert_no_requests();
}

#[test]
fn context_is_plain_text_for_managed_sessions_and_silent_outside_them() {
    let peer = Peer::new();
    let output = run(peer.command("context", false), b"");
    assert!(output.stdout.is_empty());
    peer.assert_no_requests();
    let server = peer.serve(1, "Resolved team-lead role");
    let output = run(peer.command("context", true), b"");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Resolved team-lead role\n"
    );
    let requests = server.join().unwrap();
    assert!(matches!(&requests[0], Request::Context { agent_id } if agent_id == &peer.agent_id));
}
