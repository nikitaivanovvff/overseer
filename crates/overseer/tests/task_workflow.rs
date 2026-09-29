//! Small local task loop through the real CLI and socket server. No harness or provider.
use std::{path::{Path, PathBuf}, process::{Command, Stdio}, sync::Arc, time::Duration};
use overseer_core::{agent::{AgentId, AgentRegistry}, config::Config, git::GitClient,
    ipc::{self, AppCtx, protocol::Request}, session::SessionManager, tasks::TaskStore};

struct Server {
    socket: PathBuf,
    ctx: Arc<AppCtx>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("daemon.sock");
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::with_tasks(TaskStore::open(&dir.join("daemon.tasks.json")).unwrap())),
            sessions: Arc::new(SessionManager::dry_run()), socket: socket.clone(),
            git: Arc::new(GitClient::new()), config: Arc::new(Config::default()),
            watch_sessions: false, shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let serving = ctx.clone(); let path = socket.clone();
        let thread = std::thread::spawn(move || ipc::serve_blocking(serving, path, Some(tx)).unwrap());
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        Self { socket, ctx, thread: Some(thread) }
    }
    fn command(&self, args: &[&str], agent: Option<&str>) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_overseer"));
        cmd.arg("--socket").arg(&self.socket).args(args)
            .env_remove("OVERSEER_AGENT_ID").env_remove("OVERSEER_ROLE").env_remove("OVERSEER_TASK");
        if let Some(id) = agent { cmd.env("OVERSEER_AGENT_ID", id); }
        cmd
    }
    fn run(&self, args: &[&str], agent: Option<&str>) -> serde_json::Value {
        let output = self.command(args, agent).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.ctx.shutdown_notify.notify_one();
        if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
    }
}

#[test]
fn parent_can_wait_review_and_archive_a_result_after_session_removal_and_restart() {
    let dir = PathBuf::from(format!("/tmp/ovsr-loop-{}", &uuid::Uuid::new_v4().to_string()[..8]));
    std::fs::create_dir(&dir).unwrap();
    let id;
    {
        let server = Server::start(&dir);
        let root = server.run(&["start", "--cwd", dir.to_str().unwrap()], None);
        let parent = root["data"]["agent_id"].as_str().unwrap();
        let spawned = server.run(&["spawn", "--name", "review", "--task", "Review the parser", "--adapter", "claude"], Some(parent));
        id = spawned["data"]["agent_id"].as_str().unwrap().to_string();
        let waiting = server.command(&["wait", &id, "--timeout", "3"], Some(parent))
            .stdout(Stdio::piped()).spawn().unwrap();
        // A pending wait must not occupy the server's async event loop.
        let list = ipc::client::send_with_timeout(&server.socket, &Request::List, Duration::from_secs(1)).unwrap();
        assert!(list.ok);
        let report = server.run(&["complete", "--summary", "Parser reviewed", "--artifact", "src/parser.rs", "--validation", "Unit tests passed"], Some(&id));
        assert_eq!(report["data"]["task"]["state"], "complete");
        let output = waiting.wait_with_output().unwrap();
        assert!(output.status.success());
        let waited: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(waited["data"]["timed_out"], false);
        server.run(&["drop", &id], Some(parent));
        assert!(server.ctx.registry.get(&id.parse::<AgentId>().unwrap()).is_none());
        assert_eq!(server.run(&["tasks"], Some(parent))["data"]["tasks"][0]["state"], "complete");
    }
    {
        let server = Server::start(&dir);
        let retained = server.run(&["task", &id], None);
        assert_eq!(retained["data"]["task"]["assignment"], "Review the parser");
        assert_eq!(retained["data"]["task"]["result"]["summary"], "Parser reviewed");
        assert!(!server.command(&["archive", &id], None).output().unwrap().status.success());
        server.run(&["accept", &id], None);
        server.run(&["archive", &id], None);
        assert!(server.run(&["tasks"], None)["data"]["tasks"].as_array().unwrap().is_empty());
        assert_eq!(server.run(&["tasks", "--archived"], None)["data"]["tasks"].as_array().unwrap().len(), 1);
    }
    std::fs::remove_dir_all(dir).unwrap();
}
